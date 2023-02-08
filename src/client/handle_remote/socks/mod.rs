//! SOCKS server.
//! SPDX-License-Identifier: Apache-2.0 OR GPL-3.0-or-later

mod v4;
mod v5;

use super::tcp::{open_tcp_listener, request_tcp_channel};
use super::HandlerResources;
use crate::client::{ClientIdMapEntry, StreamCommand};
use crate::Dupe;
use bytes::{Buf, Bytes, BytesMut};
use penguin_mux::{DatagramFrame, IntKey};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use tokio::io::{AsyncBufRead, BufStream};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

// Errors that can occur while handling a SOCKS request.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Error writing to the client that is
    /// not our fault.
    #[error(transparent)]
    Write(#[from] std::io::Error),
    #[error("Client with version={0} is not SOCKSv4 or SOCKSv5")]
    SocksVersion(u8),
    #[error("Unsupported SOCKS command: {0}")]
    InvalidCommand(u8),
    #[error("Invalid SOCKS address type: {0}")]
    AddressType(u8),
    #[error("Cannot {0} in SOCKS request: {1}")]
    ProcessSocksRequest(&'static str, std::io::Error),
    #[error("Invalid domain name: {0}")]
    DomainName(#[from] std::string::FromUtf8Error),
    #[error("Client does not support NOAUTH")]
    OtherAuth,
    #[error("Timed out waiting for a new channel: {0}")]
    Timeout(#[from] oneshot::error::RecvError),
    /// Fatal error that we should propagate to main.
    #[error(transparent)]
    Fatal(#[from] super::Error),
}

#[tracing::instrument(skip(handler_resources), level = "debug")]
#[inline]
pub(super) async fn handle_socks(
    lhost: &'static str,
    lport: u16,
    handler_resources: &HandlerResources,
) -> Result<(), super::Error> {
    // Failing to open the listener is a fatal error and should be propagated.
    let listener = open_tcp_listener(lhost, lport).await?;
    let mut socks_jobs = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            Some(finished) = socks_jobs.join_next() => {
                if let Err(e) = finished.expect("SOCKS job panicked (this is a bug)") {
                    if let Error::Fatal(e) = e {
                        return Err(e);
                    }
                    info!("{e}");
                }
            }
            result = listener.accept() => {
                // A failed accept() is a fatal error and should be propagated.
                let (stream, _) = result?;
                let handler_resources = handler_resources.dupe();
                socks_jobs.spawn(async move {
                    handle_socks_connection(stream, lhost, &handler_resources).await
                });
            }
        }
    }
}

pub(super) async fn handle_socks_stdio(
    handler_resources: &HandlerResources,
) -> Result<(), super::Error> {
    if let Err(e) =
        handle_socks_connection(super::Stdio::new(), "localhost", handler_resources).await
    {
        if let Error::Fatal(e) = e {
            return Err(e);
        }
        info!("{e}");
    }
    Ok(())
}

/// Handle a SOCKS5 connection.
/// Based on socksv5's example.
/// We need to be able to request additional channels, so we need `command_tx`
#[tracing::instrument(skip_all, level = "trace")]
pub(super) async fn handle_socks_connection<RW>(
    stream: RW,
    local_addr: &str,
    handler_resources: &HandlerResources,
) -> Result<(), Error>
where
    RW: AsyncRead + AsyncWrite + Unpin,
{
    let mut bufrw = BufStream::new(stream);
    let version = bufrw
        .read_u8()
        .await
        .map_err(|e| Error::ProcessSocksRequest("read version", e))?;
    match version {
        4 => handle_socks4_connection(bufrw, handler_resources).await,
        5 => handle_socks5_connection(bufrw, local_addr, handler_resources).await,
        version => Err(Error::SocksVersion(version)),
    }
}

async fn handle_socks4_connection<RW>(
    mut stream: RW,
    handler_resources: &HandlerResources,
) -> Result<(), Error>
where
    RW: AsyncBufRead + AsyncWrite + Unpin,
{
    let (command, rhost, rport) = v4::read_request(&mut stream).await?;
    debug!("SOCKSv4 request for {rhost}:{rport}");
    if command == 0x01 {
        // CONNECT
        // This fails only if main has exited, which is a fatal error.
        let stream_command_tx_permit = handler_resources
            .stream_command_tx
            .reserve()
            .await
            .map_err(|_| super::Error::RequestStream)?;
        handle_connect(stream, &rhost, rport, stream_command_tx_permit, false).await
    } else {
        v4::write_response(&mut stream, 0x5b).await?;
        Err(Error::InvalidCommand(command))
    }
}

async fn handle_socks5_connection<RW>(
    mut stream: RW,
    local_addr: &str,
    handler_resources: &HandlerResources,
) -> Result<(), Error>
where
    RW: AsyncBufRead + AsyncWrite + Unpin,
{
    // Complete the handshake
    let methods = v5::read_auth_methods(&mut stream).await?;
    if !methods.contains(&0x00) {
        // Send back NO ACCEPTABLE METHODS
        // Note that we are not compliant with RFC 1928 here, as we MUST
        // support GSSAPI and SHOULD support USERNAME/PASSWORD
        v5::write_auth_method(&mut stream, 0xff).await?;
        return Err(Error::OtherAuth);
    }
    // Send back NO AUTHENTICATION REQUIRED
    v5::write_auth_method(&mut stream, 0x00).await?;
    // Read the request
    let (command, rhost, rport) = v5::read_request(&mut stream).await?;
    debug!("SOCKSv5 cmd({command}) for {rhost}:{rport}");
    match command {
        0x01 => {
            // CONNECT
            // This fails only if main has exited, which is a fatal error.
            let stream_command_tx_permit = handler_resources
                .stream_command_tx
                .reserve()
                .await
                .map_err(|_| super::Error::RequestStream)?;
            handle_connect(stream, &rhost, rport, stream_command_tx_permit, true).await
        }
        0x03 => {
            // UDP ASSOCIATE
            handle_associate(stream, &rhost, rport, local_addr, handler_resources).await
        }
        // We don't support BIND because I can't ask the remote host to bind
        _ => {
            v5::write_response_unspecified(&mut stream, 0x07).await?;
            Err(Error::InvalidCommand(command))
        }
    }
}

async fn handle_connect<RW>(
    mut stream: RW,
    rhost: &str,
    rport: u16,
    stream_command_tx_permit: mpsc::Permit<'_, StreamCommand>,
    version_is_5: bool,
) -> Result<(), Error>
where
    RW: AsyncBufRead + AsyncWrite + Unpin,
{
    // Establish a connection to the remote host
    let mut channel = request_tcp_channel(stream_command_tx_permit, rhost.into(), rport).await?;
    // Send back a successful response
    if version_is_5 {
        v5::write_response_unspecified(&mut stream, 0x00).await?;
    } else {
        v4::write_response(&mut stream, 0x5a).await?;
    };
    stream.flush().await?;
    tokio::io::copy_bidirectional(&mut stream, &mut channel).await?;
    Ok(())
}

async fn handle_associate<RW>(
    mut stream: RW,
    rhost: &str,
    rport: u16,
    local_addr: &str,
    handler_resources: &HandlerResources,
) -> Result<(), Error>
where
    RW: AsyncBufRead + AsyncWrite + Unpin,
{
    let socket = match UdpSocket::bind((local_addr, 0)).await {
        Ok(s) => s,
        Err(e) => {
            v5::write_response_unspecified(&mut stream, 0x01).await?;
            return Err(Error::ProcessSocksRequest("bind udp socket", e));
        }
    };
    let sock_local_addr = match socket.local_addr() {
        Ok(a) => a,
        Err(e) => {
            v5::write_response_unspecified(&mut stream, 0x01).await?;
            return Err(Error::ProcessSocksRequest("get udp socket local addr", e));
        }
    };
    let relay_task = tokio::spawn(udp_relay(
        rhost.to_string(),
        rport,
        handler_resources.dupe(),
        socket,
    ));
    // Send back a successful response
    v5::write_response(&mut stream, 0x00, sock_local_addr).await?;
    // My crude way to detect when the client closes the connection
    stream.read(&mut [0; 1]).await.ok();
    relay_task.abort();
    Ok(())
}

/// UDP task spawned by the TCP connection
#[allow(clippy::similar_names)]
async fn udp_relay(
    _rhost: String,
    _rport: u16,
    handler_resources: HandlerResources,
    socket: UdpSocket,
) -> Result<(), Error> {
    let socket = Arc::new(socket);
    loop {
        let Some((dst, dport, data, src, sport)) = handle_udp_relay_header(&socket).await? else {
            continue
        };
        let mut udp_client_id_map = handler_resources.udp_client_id_map.write().await;
        let client_id = u32::next_available_key(&*udp_client_id_map);
        udp_client_id_map.insert(
            client_id,
            ClientIdMapEntry::new((src, sport).into(), socket.dupe(), true),
        );
        drop(udp_client_id_map);
        let datagram_frame = DatagramFrame {
            host: dst.into(),
            port: dport,
            sid: client_id,
            data,
        };
        // This fails only if main has exited, which is a fatal error.
        handler_resources
            .datagram_tx
            .send(datagram_frame)
            .await
            .map_err(|_| super::Error::SendDatagram)?;
    }
}

/// Parse a UDP relay request
async fn handle_udp_relay_header(
    socket: &UdpSocket,
) -> Result<Option<(String, u16, Bytes, IpAddr, u16)>, Error> {
    let mut buf = BytesMut::zeroed(65536);
    let (len, addr) = socket.recv_from(&mut buf).await?;
    buf.truncate(len);
    // let _reserved = &buf[..2];
    let frag = buf[2];
    if frag != 0 {
        warn!("Fragmented UDP packets are not implemented");
        return Ok(None);
    }
    let atyp = buf[3];
    let (dst, port, processed) = match atyp {
        0x01 => {
            // IPv4
            let array: [u8; 4] = buf[4..8]
                .try_into()
                .expect("slice with incorrect length (this is a bug)");
            let dst = Ipv4Addr::from(array).to_string();
            let port = (u16::from(buf[8]) << 8) | u16::from(buf[9]);
            (dst, port, 10)
        }
        0x03 => {
            // Domain name
            let len = usize::from(buf[4]);
            let dst = String::from_utf8_lossy(&buf[5..5 + len]).to_string();
            let port = (u16::from(buf[5 + len]) << 8) | u16::from(buf[6 + len]);
            (dst, port, 7 + len)
        }
        0x04 => {
            // IPv6
            let array: [u8; 16] = buf[4..20]
                .try_into()
                .expect("slice with incorrect length (this is a bug)");
            let dst = Ipv6Addr::from(array).to_string();
            let port = (u16::from(buf[20]) << 8) | u16::from(buf[21]);
            (dst, port, 22)
        }
        _ => {
            warn!("Dropping datagram with invalid address type {atyp}");
            return Ok(None);
        }
    };
    buf.advance(processed);
    Ok(Some((dst, port, buf.freeze(), addr.ip(), addr.port())))
}

/// Send a UDP relay response
pub async fn send_udp_relay_response(
    socket: &UdpSocket,
    target: &SocketAddr,
    data: &[u8],
) -> std::io::Result<usize> {
    // Write the header
    let mut content = vec![0; 3];
    match target.ip() {
        IpAddr::V4(ip) => {
            content.extend(ip.octets());
            content.extend([0x01]);
        }
        IpAddr::V6(ip) => {
            content.extend(ip.octets());
            content.extend([0x04]);
        }
    }
    content.extend(&target.port().to_be_bytes());
    content.extend(data);
    socket.send_to(&content, target).await
}
