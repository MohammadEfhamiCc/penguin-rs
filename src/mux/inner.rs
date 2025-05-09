//! Client side of the multiplexor
//
// SPDX-License-Identifier: Apache-2.0 OR GPL-3.0-or-later

use crate::dupe::Dupe;
use crate::frame::{ConnectPayload, FinalizedFrame, Frame, OpCode, Payload};
use crate::stream::MuxStream;
use crate::timing::{OptionalDuration, OptionalInterval};
use crate::{BindRequest, Datagram, Message, WebSocketStream, config};
use crate::{Error, Result, WsError};
use bytes::Bytes;
use futures_util::future::poll_fn;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt, task::AtomicWaker};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::task::Poll;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot};
use tokio::time::MissedTickBehavior;
use tracing::{debug, error, info, trace, warn};

#[derive(Debug)]
pub struct EstablishedStreamData {
    /// Channel for sending data to `MuxStream`'s `AsyncRead`
    sender: mpsc::Sender<Bytes>,
    /// Whether writes should succeed.
    /// There are two cases for `true`:
    /// 1. `Finish` has been sent.
    /// 2. The stream has been removed from `inner.streams`.
    // In general, our `Atomic*` types don't need more than `Relaxed` ordering
    // because we are not protecting memory accesses, but rather counting the
    // frames we have sent and received.
    finish_sent: Arc<AtomicBool>,
    /// Number of `Push` frames we are allowed to send before waiting for a `Acknowledge` frame.
    psh_send_remaining: Arc<AtomicU32>,
    /// Waker to wake up the task that sends frames because their `psh_send_remaining`
    /// has increased.
    writer_waker: Arc<AtomicWaker>,
}

#[derive(Debug)]
pub enum FlowSlot {
    /// A `Connect` frame was sent and waiting for the peer to `Acknowledge`.
    Requested(oneshot::Sender<Option<MuxStream>>),
    /// The stream is established.
    Established(EstablishedStreamData),
    /// A `Bind` request was sent and waiting for the peer to `Acknowledge` or `Reset`.
    BindRequested(oneshot::Sender<bool>),
}

impl FlowSlot {
    /// Take the sender and set the slot to `Established`.
    /// Returns `None` if the slot is already established.
    pub fn establish(
        &mut self,
        data: EstablishedStreamData,
    ) -> Option<oneshot::Sender<Option<MuxStream>>> {
        // Make sure it is not replaced in the error case
        if matches!(self, Self::Established(_) | Self::BindRequested(_)) {
            error!("establishing an established or invalid slot");
            return None;
        }
        let sender = match std::mem::replace(self, Self::Established(data)) {
            Self::Requested(sender) => sender,
            Self::Established(_) | Self::BindRequested(_) => unreachable!(),
        };
        Some(sender)
    }
}

/// Multiplexor inner
pub struct MultiplexorInner {
    /// Where tasks queue frames to be sent
    pub tx_frame_tx: mpsc::UnboundedSender<FinalizedFrame>,
    /// Interval between keepalive `Ping`s
    pub keepalive_interval: OptionalDuration,
    /// Open stream channels: `flow_id` -> `FlowSlot`
    pub flows: Arc<RwLock<HashMap<u32, FlowSlot>>>,
    /// Channel for notifying the task of a dropped `MuxStream` (to send the flow ID)
    /// Sending 0 means that the multiplexor is being dropped and the
    /// task should exit.
    /// The reason we need `their_port` is to ensure the connection is `Reset`ted
    /// if the user did not call `poll_shutdown` on the `MuxStream`.
    pub dropped_ports_tx: mpsc::UnboundedSender<u32>,
    /// Default threshold for `Acknowledge` replies. See [`MuxStream`] for more details.
    pub default_rwnd_threshold: u32,
}

impl std::fmt::Debug for MultiplexorInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MultiplexorInner")
            .field("keepalive_interval", &self.keepalive_interval)
            .field("default_rwnd_threshold", &self.default_rwnd_threshold)
            .finish_non_exhaustive()
    }
}

impl Dupe for MultiplexorInner {
    #[inline]
    fn dupe(&self) -> Self {
        Self {
            tx_frame_tx: self.tx_frame_tx.dupe(),
            keepalive_interval: self.keepalive_interval,
            flows: self.flows.dupe(),
            dropped_ports_tx: self.dropped_ports_tx.dupe(),
            default_rwnd_threshold: self.default_rwnd_threshold,
        }
    }
}

impl MultiplexorInner {
    /// Processing task
    /// Does the following:
    /// - Receives messages from `WebSocket` and processes them
    /// - Sends received datagrams to the `datagram_tx` channel
    /// - Sends received streams to the appropriate handler
    /// - Responds to ping/pong messages
    // It doesn't make sense to return a `Result` here because we can't propagate
    // the error to the user from a spawned task.
    // Instead, the user will notice when `rx` channels return `None`.
    #[tracing::instrument(skip_all, level = "trace")]
    pub async fn task<S: WebSocketStream<WsError>>(
        self,
        ws: S,
        taskdata: super::TaskData,
    ) -> Result<()> {
        let super::TaskData {
            datagram_tx,
            con_recv_stream_tx,
            mut tx_frame_rx,
            bnd_request_tx,
            mut dropped_ports_rx,
        } = taskdata;
        // Split the `WebSocket` stream into a `Sink` and `Stream` so we can process them concurrently
        let (mut ws_sink, mut ws_stream) = ws.split();
        // This is modified from an unrolled version of `tokio::try_join!` with our custom cancellation
        // logic and to make sure that tasks are not cancelled at random points.
        let (e, should_drain_frame_rx) = {
            let mut process_dropped_ports_task_fut =
                pin!(self.process_dropped_ports_task(&mut dropped_ports_rx));
            let mut process_frame_recv_task_fut =
                pin!(self.process_frame_recv_task(&mut tx_frame_rx, &mut ws_sink));
            let mut process_ws_next_fut = pin!(self.process_ws_next(
                &mut ws_stream,
                &datagram_tx,
                &con_recv_stream_tx,
                bnd_request_tx.as_ref()
            ));
            poll_fn(|cx| {
                if let Poll::Ready(r) = process_dropped_ports_task_fut.as_mut().poll(cx) {
                    let should_drain_frame_rx = r.is_ok();
                    debug!("mux dropped ports task finished: {r:?}");
                    return Poll::Ready((r, should_drain_frame_rx));
                }
                if let Poll::Ready(r) = process_ws_next_fut.as_mut().poll(cx) {
                    debug!("mux ws next task finished: {r:?}");
                    return Poll::Ready((r, false));
                }
                if let Poll::Ready(r) = process_frame_recv_task_fut.as_mut().poll(cx) {
                    debug!("mux frame recv task finished: {r:?}");
                    return Poll::Ready((r, false));
                }
                Poll::Pending
            })
            .await
        };
        self.wind_down(
            should_drain_frame_rx,
            ws_sink
                .reunite(ws_stream)
                .expect("Failed to reunite sink and stream (this is a bug)"),
            datagram_tx,
            con_recv_stream_tx,
            tx_frame_rx,
        )
        .await?;
        e
    }

    /// Process dropped ports from the `dropped_ports_rx` channel.
    /// Returns when either [`MultiplexorInner`] or [`Multiplexor`] itself is dropped.
    #[tracing::instrument(skip_all, level = "trace")]
    #[inline]
    pub async fn process_dropped_ports_task(
        &self,
        dropped_ports_rx: &mut mpsc::UnboundedReceiver<u32>,
    ) -> Result<()> {
        while let Some(flow_id) = dropped_ports_rx.recv().await {
            if flow_id == 0 {
                // `our_port` is `0`, which means the multiplexor itself is being dropped.
                debug!("mux dropped");
                break;
            }
            self.close_port(flow_id, false).await;
        }
        // None: only happens when the last sender (i.e. `dropped_ports_tx` in `MultiplexorInner`)
        // is dropped,
        // which can be combined with the case when the multiplexor itself is being dropped.
        // If this returns, our end is dropped, but we should still try to flush everything we
        // already have in the `frame_rx` before closing.
        // we should make some attempt to flush `frame_rx` before exiting.
        Ok(())
    }

    /// Poll `frame_rx` and process the frame received and send keepalive pings as needed.
    /// It never returns an `Ok(())`, and propagates errors from the `Sink` processing.
    #[tracing::instrument(skip_all, level = "trace")]
    #[inline]
    async fn process_frame_recv_task<S: WebSocketStream<WsError>>(
        &self,
        frame_rx: &mut mpsc::UnboundedReceiver<FinalizedFrame>,
        ws_sink: &mut SplitSink<S, Message>,
    ) -> Result<()> {
        let mut interval = OptionalInterval::from(self.keepalive_interval);
        // If we missed a tick, it is probably doing networking, so we don't need to
        // make up for it.
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    trace!("sending keepalive ping");
                    ws_sink.send(Message::Ping(Bytes::new())).await.map_err(Box::new)?;
                }
                Some(frame) = frame_rx.recv() => {
                    // Buffer `Push` frames, and flush everything else immediately
                    if frame.is_empty() {
                        // Flush
                        ws_sink.flush().await
                    } else if frame.opcode()? == OpCode::Push {
                        ws_sink.feed(Message::Binary(frame.into())).await
                    } else {
                        ws_sink.send(Message::Binary(frame.into())).await
                    }
                    .map_err(Box::new)?;
                }
                else => {
                    // Only happens when `frame_rx` is closed
                    // cannot happen because `Self` contains one sender unless
                    // there is a bug in our code or `tokio` itself.
                    panic!("frame receiver should not be closed (this is a bug)");
                }
            }
        }
        // This returns if we cannot sink or cannot receive from `frame_rx` anymore,
        // in either case, it does not make sense to check `frame_rx`.
    }

    /// Process the return value of `ws.next()`
    /// Returns `Ok(())` when a `Close` message was received or the WebSocket was otherwise closed by the peer.
    #[tracing::instrument(skip_all, level = "trace")]
    async fn process_ws_next<S: WebSocketStream<WsError>>(
        &self,
        ws_stream: &mut SplitStream<S>,
        datagram_tx: &mpsc::Sender<Datagram>,
        con_recv_stream_tx: &mpsc::Sender<MuxStream>,
        bnd_request_tx: Option<&mpsc::Sender<BindRequest<'static>>>,
    ) -> Result<()> {
        loop {
            match ws_stream.next().await {
                Some(Ok(msg)) => {
                    trace!("received message length = {}", msg.len());
                    if self
                        .process_message(msg, datagram_tx, con_recv_stream_tx, bnd_request_tx)
                        .await?
                    {
                        // Received a `Close` message
                        break Ok(());
                    }
                }
                Some(Err(e)) => {
                    error!("Failed to receive message from WebSocket: {e}");
                    break Err(Error::WebSocket(Box::new(e)));
                }
                None => {
                    debug!("WebSocket closed by peer");
                    break Ok(());
                }
            }
        }
        // In this case, peer already requested close, so we should not attempt to send any more frames.
    }

    /// Wind down the multiplexor task.
    #[tracing::instrument(skip_all, level = "trace")]
    async fn wind_down<S: WebSocketStream<WsError>>(
        &self,
        should_drain_frame_rx: bool,
        mut ws: S,
        datagram_tx: mpsc::Sender<Datagram>,
        con_recv_stream_tx: mpsc::Sender<MuxStream>,
        mut frame_rx: mpsc::UnboundedReceiver<FinalizedFrame>,
    ) -> Result<()> {
        debug!("closing all connections");
        // We first make sure the streams can no longer send
        for (_, stream_data) in self.flows.write().iter() {
            if let FlowSlot::Established(stream_data) = stream_data {
                // Prevent the user from writing
                // Atomic ordering: It does not matter whether the user calls `poll_shutdown` or not,
                // the stream is shut down and the final value of `finish_sent` is `true`.
                stream_data.finish_sent.store(true, Ordering::Relaxed);
                // If there is a writer waiting for `Acknowledge`, wake it up because it will never receive one.
                // Waking it here and the user should receive a `BrokenPipe` error.
                stream_data.writer_waker.wake();
            }
        }
        // Now if `should_drain_frame_rx` is `true`, we will process the remaining frames in `frame_rx`.
        // If it is `false`, then we reached here because the peer is now not interested
        // in our connection anymore, and we should just mind our own business and serve the connections
        // on our end.
        // We must use `try_recv` because, again, `Self` contains one sender.
        if should_drain_frame_rx {
            while let Ok(frame) = frame_rx.try_recv() {
                debug!("sending remaining frame after mux drop");
                if let Err(e) = ws.feed(Message::Binary(frame.into())).await {
                    warn!("Failed to send remaining frame after mux drop: {e}");
                    // Don't keep trying to send frames after an error
                    break;
                }
                // will be flushed in `ws.close()` anyways
                // ws.flush().await.ok();
            }
        }
        // This will flush the remaining frames already queued for sending as well
        ws.close().await.ok();
        // The above line only closes the `Sink`. Before we terminate connections,
        // we dispatch the remaining frames in the `Source` to our streams.
        while let Some(Ok(msg)) = ws.next().await {
            debug!(
                "processing remaining message after closure length = {}",
                msg.len()
            );
            self.process_message(msg, &datagram_tx, &con_recv_stream_tx, None)
                .await?;
        }
        // Finally, we send EOF to all established streams.
        let senders = self
            .flows
            .write()
            .drain()
            .filter_map(|(_, stream_slot)| {
                if let FlowSlot::Established(stream_data) = stream_slot {
                    Some(stream_data.sender)
                } else {
                    None
                    // else: just drop the sender for `Requested` slots, and the user
                    // will get `Error::Closed` from `client_new_stream_channel`
                }
            })
            .collect::<Vec<_>>();
        for sender in senders {
            sender.send(Bytes::new()).await.ok();
        }
        Ok(())
    }
}

impl MultiplexorInner {
    /// Process an incoming message
    /// Returns `Ok(true)` if a `Close` message was received.
    #[tracing::instrument(skip_all, level = "trace")]
    #[inline]
    async fn process_message(
        &self,
        msg: Message,
        datagram_tx: &mpsc::Sender<Datagram>,
        con_recv_stream_tx: &mpsc::Sender<MuxStream>,
        bnd_request_tx: Option<&mpsc::Sender<BindRequest<'static>>>,
    ) -> Result<bool> {
        match msg {
            Message::Binary(data) => {
                let frame = data.try_into()?;
                trace!("received stream frame: {frame:?}");
                self.process_frame(frame, datagram_tx, con_recv_stream_tx, bnd_request_tx)
                    .await?;
                Ok(false)
            }
            Message::Ping(_data) => {
                // `tokio-tungstenite` handles `Ping` messages automatically
                trace!("received ping");
                Ok(false)
            }
            Message::Pong(_data) => {
                trace!("received pong");
                Ok(false)
            }
            Message::Close(_) => {
                debug!("received close");
                Ok(true)
            }
            Message::Text(text) => {
                debug!("received `Text` message: `{text}'");
                Err(Error::TextMessage)
            }
            Message::Frame(_) => {
                unreachable!("`Frame` message should not be received");
            }
        }
    }

    /// Process a stream frame
    /// Does the following:
    /// - If `flag` is [`Connect`](crate::frame::OpCode::Connect),
    ///   - Find an available `dport` and send a `Acknowledge`.
    ///   - Create a new `MuxStream` and send it to the `stream_tx` channel.
    /// - If `flag` is `Acknowledge`,
    ///   - Existing stream with the matching `dport`: increment the `psh_send_remaining` counter.
    ///   - New stream: create a `MuxStream` and send it to the `stream_tx` channel.
    /// - If `flag` is `Bind`,
    ///   - Send the request to the user if we are accepting `Bind` requests and reply `Finish`.
    ///   - Otherwise, send back a `Reset` frame.
    /// - Otherwise, we find the sender with the matching `dport` and
    ///   - Send the data to the sender.
    ///   - If the receiver is closed or the port does not exist, send back a
    ///     `Reset` frame.
    #[tracing::instrument(skip_all, fields(flow_id), level = "debug")]
    #[inline]
    async fn process_frame(
        &self,
        frame: Frame<'static>,
        datagram_tx: &mpsc::Sender<Datagram>,
        con_recv_stream_tx: &mpsc::Sender<MuxStream>,
        bnd_request_tx: Option<&mpsc::Sender<BindRequest<'static>>>,
    ) -> Result<()> {
        let Frame {
            id: flow_id,
            payload,
        } = frame;
        tracing::Span::current().record("flow_id", format_args!("{flow_id:08x}"));
        let send_rst = async {
            self.tx_frame_tx
                .send(Frame::new_reset(flow_id).finalize())
                .ok()
            // Error only happens if the `frame_tx` channel is closed, at which point
            // we don't care about sending a `Reset` frame anymore
        };
        match payload {
            Payload::Connect(ConnectPayload {
                rwnd: peer_rwnd,
                target_host,
                target_port,
            }) => {
                // In this case, `target_host` is always owned already
                self.con_recv_new_stream(
                    flow_id,
                    target_host.into_owned(),
                    target_port,
                    peer_rwnd,
                    con_recv_stream_tx,
                )
                .await?;
            }
            Payload::Acknowledge(payload) => {
                trace!("received `Acknowledge`");
                // Three cases:
                // 1. Peer acknowledged `Connect`
                // 2. Peer acknowledged some `Push` frames
                // 3. Something unexpected
                let (should_new_stream, should_send_rst) = match self.flows.read().get(&flow_id) {
                    Some(FlowSlot::Established(stream_data)) => {
                        debug!("peer processed {payload} frames");
                        // We have an established stream, so process the `Acknowledge`
                        // Atomic ordering: as long as the value is incremented atomically,
                        // whether a writer sees the new value or the old value is not
                        // important. If it sees the old value and decides to return
                        // `Poll::Pending`, it will be woken up by the `Waker` anyway.
                        stream_data
                            .psh_send_remaining
                            .fetch_add(payload, Ordering::Relaxed);
                        stream_data.writer_waker.wake();
                        (false, false)
                    }
                    Some(FlowSlot::Requested(_)) => {
                        debug!("new stream with peer rwnd {payload}");
                        (true, false)
                    }
                    Some(FlowSlot::BindRequested(_)) => {
                        warn!("Peer replied `Acknowledge` to a `Bind` request");
                        (false, true)
                    }
                    None => {
                        debug!("stream does not exist, sending `Reset`");
                        (false, true)
                    }
                };
                if should_new_stream {
                    self.ack_recv_new_stream(flow_id, payload)?;
                } else if should_send_rst {
                    send_rst.await;
                }
            }
            Payload::Finish => {
                let sender = if let Some(FlowSlot::Established(stream_data)) =
                    self.flows.read().get(&flow_id)
                {
                    Some(stream_data.sender.dupe())
                } else {
                    None
                };

                // Make sure the user receives `EOF`.
                // This part is refactored out so that we don't hold the lock across await
                if let Some(sender) = sender {
                    sender.send(Bytes::new()).await.ok();
                    // And our end can still send
                } else {
                    let slot = self.flows.write().remove(&flow_id);
                    match slot {
                        Some(FlowSlot::Established(_)) => unreachable!(),
                        Some(FlowSlot::Requested(_)) => {
                            // This is an invalid reply to a `Connect` frame
                            warn!("Peer replied `Finish` to a `Connect` request");
                            send_rst.await;
                        }
                        Some(FlowSlot::BindRequested(sender)) => {
                            // Peer successfully bound the port
                            sender.send(true).ok();
                            // If the send above fails, the receiver is dropped,
                            // so we can just ignore it.
                        }
                        None => warn!("Bogus `Finish` frame"),
                    }
                }
            }
            Payload::Reset => {
                debug!("received `Reset`");
                // `true` because we don't want to reply `Reset` with `Reset`.
                self.close_port(flow_id, true).await;
            }
            Payload::Push(data) => {
                let sender = if let Some(FlowSlot::Established(stream_data)) =
                    self.flows.read().get(&flow_id)
                {
                    Some(stream_data.sender.dupe())
                } else {
                    None
                };
                // This part is refactored out so that we don't hold the lock across await
                if let Some(sender) = sender {
                    // In this case, `data` is always owned already
                    match sender.try_send(data.into_owned()) {
                        Err(TrySendError::Full(_)) => {
                            // Peer does not respect the `rwnd` limit, this should not happen in normal circumstances.
                            // let's send `Reset`.
                            warn!("Peer does not respect `rwnd` limit, dropping stream");
                            self.close_port(flow_id, false).await;
                        }
                        Err(TrySendError::Closed(_)) => {
                            // Else, the corresponding `MuxStream` is dropped
                            // The job to remove the port from the map is done by `close_port_task`,
                            // so not being able to send is the same as not finding the port;
                            // just timing is different.
                            trace!("dropped `MuxStream` not yet removed from the map");
                        }
                        Ok(()) => (),
                    }
                } else {
                    warn!("Bogus `Push` frame");
                    send_rst.await;
                }
            }
            Payload::Bind(payload) => {
                if let Some(sender) = bnd_request_tx {
                    debug!(
                        "received `Bind` request: [{:?}]:{}",
                        payload.target_host, payload.target_port
                    );
                    let request = BindRequest {
                        flow_id,
                        payload,
                        tx_frame_tx: self.tx_frame_tx.dupe(),
                    };
                    if let Err(e) = sender.send(request).await {
                        warn!("Failed to send `Bind` request: {e}");
                    }
                    // Let the user decide what to reply using `BindRequest::reply`
                } else {
                    info!("Received `Bind` request but configured to not accept such requests");
                    self.tx_frame_tx
                        .send(Frame::new_reset(flow_id).finalize())
                        .ok();
                }
            }
            Payload::Datagram(payload) => {
                trace!("received datagram frame: {payload:?}");
                // Only fails if the receiver is dropped or the queue is full.
                // The first case means the multiplexor itself is dropped;
                // In the second case, we just drop the frame to avoid blocking.
                // It is UDP, after all.
                let datagram = Datagram {
                    flow_id,
                    target_host: payload.target_host.into_owned(),
                    target_port: payload.target_port,
                    data: payload.data.into_owned(),
                };
                if let Err(e) = datagram_tx.try_send(datagram) {
                    match e {
                        TrySendError::Full(_) => {
                            warn!("Dropped datagram: {e}");
                        }
                        TrySendError::Closed(_) => {
                            return Err(Error::Closed);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Shared code for new stream stuff
    #[inline]
    fn new_stream_shared(
        &self,
        flow_id: u32,
        peer_rwnd: u32,
        dest_host: Bytes,
        dest_port: u16,
    ) -> (MuxStream, EstablishedStreamData) {
        // `tx` is our end, `rx` is the user's end
        let (frame_tx, frame_rx) = mpsc::channel(config::RWND_USIZE);
        let finish_sent = Arc::new(AtomicBool::new(false));
        let psh_send_remaining = Arc::new(AtomicU32::new(peer_rwnd));
        let writer_waker = Arc::new(AtomicWaker::new());
        let stream_data = EstablishedStreamData {
            sender: frame_tx,
            finish_sent: finish_sent.dupe(),
            psh_send_remaining: psh_send_remaining.dupe(),
            writer_waker: writer_waker.dupe(),
        };
        // Save the TX end of the stream so we can write to it when subsequent frames arrive
        let stream = MuxStream {
            frame_rx,
            flow_id,
            dest_host,
            dest_port,
            finish_sent,
            psh_send_remaining,
            psh_recvd_since: 0,
            writer_waker,
            buf: Bytes::new(),
            frame_tx: self.tx_frame_tx.dupe(),
            dropped_ports_tx: self.dropped_ports_tx.dupe(),
            rwnd_threshold: self.default_rwnd_threshold.min(peer_rwnd),
        };
        (stream, stream_data)
    }

    /// Create a new stream because this end received a [`Connect`](crate::frame::OpCode::Connect) frame.
    /// Create a new `MuxStream`, add it to the map, and send an `Acknowledge` frame.
    /// If `our_port` is 0, a new port will be allocated.
    #[tracing::instrument(skip_all, level = "debug")]
    #[inline]
    async fn con_recv_new_stream(
        &self,
        flow_id: u32,
        dest_host: Bytes,
        dest_port: u16,
        peer_rwnd: u32,
        con_recv_stream_tx: &mpsc::Sender<MuxStream>,
    ) -> Result<()> {
        // Scope the following block to reduce locked time
        let stream = {
            // Save the TX end of the stream so we can write to it when subsequent frames arrive
            let mut streams = self.flows.write();
            if streams.contains_key(&flow_id) {
                debug!("resetting `Connect` with in-use flow_id");
                self.tx_frame_tx
                    .send(Frame::new_reset(flow_id).finalize())
                    .ok();
                // On the other side, `process_frame` will pass the `Reset` frame to
                // `close_port`, which takes the port out of the map and inform `Multiplexor::new_stream_channel`
                // to retry.
                // The existing connection at the same `flow_id` is not affected. For conforminh implementations,
                // This only happens when both ends are trying to establish a new connection at the same time
                // and also happen to have chosen the same `flow_id`.
                // In this case, the peer would also receive our `Connect` frame and, depending on the timing,
                // `Reset` us too or `Acknowledge` us.
                return Ok(());
            }
            let (stream, stream_data) =
                self.new_stream_shared(flow_id, peer_rwnd, dest_host, dest_port);
            // No write should occur between our check and insert
            streams.insert(flow_id, FlowSlot::Established(stream_data));
            stream
        };
        // Send a `Acknowledge`
        // Make sure `Acknowledge` is sent before the stream is sent to the user
        // so that the stream is `Established` when the user uses it.
        trace!("sending `Acknowledge`");
        self.tx_frame_tx
            .send(Frame::new_acknowledge(flow_id, config::RWND).finalize())
            .map_err(|_| Error::Closed)?;
        // At the con_recv side, we use `con_recv_stream_tx` to send the new stream to the
        // user.
        trace!("sending stream to user");
        // This goes to the user
        con_recv_stream_tx
            .send(stream)
            .await
            .map_err(|_| Error::SendStreamToClient)?;
        Ok(())
    }

    /// Create a new `MuxStream` by finalizing a Con/Ack handshsake and
    /// change the state of the port to `Established`.
    #[tracing::instrument(skip_all, level = "debug")]
    #[inline]
    fn ack_recv_new_stream(&self, flow_id: u32, peer_rwnd: u32) -> Result<()> {
        // Change the state of the port to `Established` and send the stream to the user
        // At the client side, we use the associated oneshot channel to send the new stream
        trace!("sending stream to user");
        let (stream, stream_data) = self.new_stream_shared(flow_id, peer_rwnd, Bytes::new(), 0);
        self.flows
            .write()
            .get_mut(&flow_id)
            .ok_or(Error::ConnAckGone)?
            .establish(stream_data)
            .ok_or(Error::ConnAckGone)?
            .send(Some(stream))
            .map_err(|_| Error::SendStreamToClient)?;
        Ok(())
    }

    /// Close a port. That is, send `Reset` if `Finish` is not sent,
    /// and remove it from the map.
    #[tracing::instrument(skip_all)]
    #[inline]
    async fn close_port(&self, flow_id: u32, inhibit_rst: bool) {
        // Free the port for reuse
        let removed = self.flows.write().remove(&flow_id);
        match removed {
            Some(FlowSlot::Established(stream_data)) => {
                // Make sure the user receives `EOF`.
                stream_data.sender.send(Bytes::new()).await.ok();
                // Ignore the error if the user already dropped the stream
                // Atomic ordering:
                // Load part:
                // If the user calls `poll_shutdown`, but we see `true` here,
                // the other end will receive a bogus `Reset` frame, which is fine.
                // Store part:
                // It does not matter whether the user calls `poll_shutdown` or not,
                // the stream is shut down and the final value of `finish_sent` is `true`.
                let finish_sent = stream_data.finish_sent.swap(true, Ordering::Relaxed);
                if !finish_sent && !inhibit_rst {
                    // If the user did not call `poll_shutdown`, we send a `Reset` frame
                    self.tx_frame_tx
                        .send(Frame::new_reset(flow_id).finalize())
                        .ok();
                    // Ignore the error because the other end will EOF everything anyway
                }
                // If there is a writer waiting for `Acknowledge`, wake it up because it will never receive one.
                // Waking it here and the user should receive a `BrokenPipe` error.
                stream_data.writer_waker.wake();
                debug!("freed connection");
            }
            Some(FlowSlot::Requested(sender)) => {
                sender.send(None).ok();
                // Ignore the error if the user already cancelled the requesting future
                debug!("peer cancelled `Connect`");
            }
            Some(FlowSlot::BindRequested(sender)) => {
                sender.send(false).ok();
                // Ignore the error if the user already cancelled the requesting future
                debug!("peer rejected `Bind`");
            }
            None => {
                debug!("connection not found, nothing to close");
            }
        }
    }
}
