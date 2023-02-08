//! Server-side forwarding implementation.
//! Pipes TCP streams or forwards UDP Datagrams to and from another host.
//! SPDX-License-Identifier: Apache-2.0 OR GPL-3.0-or-later

use crate::{config, Dupe};
use bytes::Bytes;
use penguin_mux::DatagramFrame;
use std::net::SocketAddr;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::{
    net::{lookup_host, UdpSocket},
    sync::mpsc::Sender,
};
use tracing::{debug, trace};

/// Error type for the forwarder.
#[derive(Error, Debug)]
pub(super) enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("Invalid host: {0}")]
    Host(#[from] std::str::Utf8Error),
}

/// Bind a UDP socket with the same address family as the given target,
/// connect to the target, and send the given data.
/// Finally, return the bound socket and the target address.
#[inline]
async fn bind_and_send(target: (&str, u16), data: &[u8]) -> Result<(UdpSocket, SocketAddr), Error> {
    let targets = lookup_host(target).await?;
    let mut last_err = None;
    for target in targets {
        let socket = match if target.is_ipv4() {
            UdpSocket::bind(("0.0.0.0", 0)).await
        } else {
            UdpSocket::bind(("::", 0)).await
        } {
            Ok(socket) => socket,
            Err(e) => {
                last_err = Some(e);
                continue;
            }
        };
        debug!("bound to {}", socket.local_addr()?);
        if let Err(e) = socket.connect(target).await {
            last_err = Some(e);
            continue;
        }
        socket.send(data).await?;
        return Ok((socket, target));
    }
    Err(last_err
        .unwrap_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "could not resolve to any address",
            )
        })
        .into())
}

/// Send a UDP datagram to the given host and port and wait for a response
/// in the following `UDP_PRUNE_TIMEOUT` seconds.
#[tracing::instrument(skip(datagram_tx), level = "debug")]
pub(super) async fn udp_forward_to(
    datagram_frame: DatagramFrame,
    datagram_tx: Sender<DatagramFrame>,
) -> Result<(), Error> {
    trace!("got datagram frame: {datagram_frame:?}");
    let rhost = datagram_frame.host;
    let rhost_str = std::str::from_utf8(&rhost)?;
    let rport = datagram_frame.port;
    let data = datagram_frame.data;
    let client_id = datagram_frame.sid;
    let (socket, target) = bind_and_send((rhost_str, rport), &data).await?;
    trace!("sent UDP packet to {target}");
    loop {
        let mut buf = vec![0; 65536];
        match tokio::time::timeout(config::UDP_PRUNE_TIMEOUT, socket.recv(&mut buf)).await {
            Ok(Ok(len)) => {
                trace!("got UDP response from {target}");
                buf.truncate(len);
                let datagram_frame = DatagramFrame {
                    sid: client_id,
                    host: rhost.dupe(),
                    port: rport,
                    data: Bytes::from(buf),
                };
                if datagram_tx.send(datagram_frame).await.is_err() {
                    // The main loop has exited, so we should exit too.
                    break;
                }
            }
            Ok(Err(e)) => {
                return Err(e.into());
            }
            Err(_) => {
                trace!("UDP prune timeout");
                break;
            }
        };
    }
    debug!("UDP forwarding finished");
    Ok(())
}

/// Start a TCP forwarding server on the given listener.
///
/// This forwarder is trivial: it just pipes the TCP stream to and from the
/// channel.
///
/// # Errors
/// It carries the errors from the underlying TCP or channel IO functions.
#[tracing::instrument(skip(channel), level = "debug")]
pub(super) async fn tcp_forwarder_on_channel(
    mut channel: super::websocket::MuxStream,
) -> Result<(), Error> {
    let rhost = std::str::from_utf8(&channel.dest_host)?;
    let rport = channel.dest_port;
    trace!("attempting TCP connect to {rhost} port={rport}");
    let mut rstream = TcpStream::connect((rhost, rport)).await?;
    debug!("TCP forwarding to {:?}", rstream.peer_addr());
    tokio::io::copy_bidirectional(&mut channel, &mut rstream).await?;
    trace!("TCP forwarding finished");
    Ok(())
}
