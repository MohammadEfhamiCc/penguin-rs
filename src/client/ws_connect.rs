//! `WebSocket` connection.
//
// SPDX-License-Identifier: Apache-2.0 OR GPL-3.0-or-later

use crate::arg::ClientArgs;
use crate::tls::make_tls_connector;
use http::header::HeaderValue;
use penguin_mux::{Dupe, PROTOCOL_VERSION};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, handshake::client::Request};
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async_tls_with_config,
};
use tracing::{debug, warn};

/// Perform a `WebSocket` handshake.
#[tracing::instrument(skip_all, fields(server = %args.server.0), level = "debug")]
pub async fn handshake(
    args: &ClientArgs,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, super::Error> {
    // We already sanitized https URLs to wss
    let is_tls = args
        .server
        .scheme()
        .expect("URL scheme should be present (this is a bug)")
        .as_str()
        == "wss";

    // Use a request to allow additional headers
    let mut req: Request = args.server.0.dupe().into_client_request()?;
    let req_headers = req.headers_mut();
    // Add protocol version
    req_headers.insert(
        "sec-websocket-protocol",
        HeaderValue::from_static(PROTOCOL_VERSION),
    );
    // Add PSK
    if let Some(ref ws_psk) = args.ws_psk {
        req_headers.insert("x-penguin-psk", ws_psk.dupe());
    }
    // Add potentially custom hostname
    if let Some(ref hostname) = args.hostname {
        req_headers.insert("host", hostname.dupe());
    }
    // Now add custom headers
    for header in &args.header {
        req_headers.insert(&header.name, header.value.dupe());
    }

    let connector = if is_tls {
        make_tls_connector(
            args.tls_cert.as_deref(),
            args.tls_key.as_deref(),
            args.tls_ca.as_deref(),
            args.tls_skip_verify,
        )
        .await?
    } else {
        // No TLS
        warn!("Using insecure WebSocket connection");
        Connector::Plain
    };
    let handshake = Box::pin(connect_async_tls_with_config(
        req,
        None,
        false,
        Some(connector),
    ));
    tokio::select! {
        result = handshake => {
            let (ws_stream, _response) = result?;
            // We don't need to check the response now...
            debug!("WebSocket handshake succeeded");
            Ok(ws_stream)
        }
        () = args.handshake_timeout.sleep() => Err(super::Error::HandshakeTimeout),
        Ok(()) = tokio::signal::ctrl_c() => Err(super::Error::HandshakeCancelled),
    }
}
