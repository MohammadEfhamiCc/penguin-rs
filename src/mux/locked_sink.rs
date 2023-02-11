//! A wrapper around `Sink + Stream` that can be cloned and shared between tasks.
//! SPDX-License-Identifier: Apache-2.0 OR GPL-3.0-or-later
#![deny(missing_docs)]

use crate::ws::{Message, Result, WebSocketError, WebSocketStream};
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use std::future::poll_fn;
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use tracing::trace;

/// A wrapper around `Sink + Stream` that can be cloned and shared between tasks.
pub struct LockedWebSocket<S>(Arc<Mutex<S>>);

impl<S> LockedWebSocket<S> {
    /// Create a new `LockedWebSocket` from a `WebSocketStream`
    #[inline]
    pub fn new(websocket: S) -> Self {
        Self(Arc::new(Mutex::new(websocket)))
    }
}

impl<S: WebSocketStream> LockedWebSocket<S> {
    /// Lock and send the resulting `Message` from a computation.
    /// The computation is only executed if the sink is ready.
    /// The computation may return `Poll::Pending` to indicate that it is not
    /// ready yet, in which case the task should be woken up when it is ready.
    #[inline]
    pub fn poll_send_with(
        &self,
        cx: &mut Context<'_>,
        msg_fn: impl FnOnce(&mut Context<'_>) -> Poll<Message>,
    ) -> Poll<Result<()>> {
        let mut sink = self.0.lock();
        // `ready`: if we return here, nothing happens
        ready!(sink.poll_ready_unpin(cx))?;
        let msg = ready!(msg_fn(cx));
        let result = sink.start_send_unpin(msg);
        trace!("message sent");
        Poll::Ready(result)
    }

    /// Lock and send the resulting `Message` from a computation.
    ///
    /// # Cancel safety
    /// This function is cancel safe. If the task is cancelled, it is
    /// guaranteed that the message will not be sent.
    #[inline]
    pub async fn send_with(&self, msg_fn: impl Fn() -> Message) -> Result<()> {
        poll_fn(|cx| self.poll_send_with(cx, |_cx| Poll::Ready(msg_fn()))).await
    }

    /// Lock and flush the sink
    #[inline]
    pub fn poll_flush(&self, cx: &mut Context<'_>) -> Poll<Result<()>> {
        self.0.lock().poll_flush_unpin(cx)
    }

    /// Lock and flush the sink, ignoring errors that indicate the connection
    /// is closed.
    /// It is sometimes acceptable when the other side closes the connection
    /// because the user should only discover this when they try to work with
    /// the stream for the next time.
    #[inline]
    pub fn poll_flush_ignore_closed(&self, cx: &mut Context<'_>) -> Poll<Result<()>> {
        match ready!(self.poll_flush(cx)) {
            Ok(()) => Poll::Ready(Ok(())),
            Err(e) if e.because_closed() => Poll::Ready(Ok(())),
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    #[inline]
    pub async fn flush_ignore_closed(&self) -> Result<()> {
        poll_fn(|cx| self.poll_flush_ignore_closed(cx)).await
    }

    /// Lock and close the sink
    #[inline]
    pub fn poll_close(&self, cx: &mut Context<'_>) -> Poll<Result<()>> {
        self.0.lock().poll_close_unpin(cx)
    }

    #[inline]
    pub async fn close(&self) -> Result<()> {
        poll_fn(|cx| self.poll_close(cx)).await
    }

    #[inline]
    pub fn poll_next(&self, cx: &mut Context<'_>) -> Poll<Option<Result<Message>>> {
        self.0.lock().poll_next_unpin(cx)
    }

    #[inline]
    pub async fn next(&self) -> Option<Result<Message>> {
        poll_fn(|cx| self.poll_next(cx)).await
    }
}

impl<S> crate::dupe::Dupe for LockedWebSocket<S> {
    #[inline]
    fn dupe(&self) -> Self {
        Self(self.0.dupe())
    }
}

impl<S: std::fmt::Debug> std::fmt::Debug for LockedWebSocket<S> {
    #[inline]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        <Mutex<S> as std::fmt::Debug>::fmt(&*self.0, f)
    }
}
