//! Penguin server.
//! SPDX-License-Identifier: Apache-2.0 OR GPL-3.0-or-later

mod forwarder;
mod websocket;

use std::sync::Arc;

use crate::arg::{BackendUrl, ServerArgs};
use crate::proto_version::PROTOCOL_VERSION;
use crate::tls::make_rustls_server_config;
use axum::extract::WebSocketUpgrade;
use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use axum_server::tls_rustls::RustlsConfig;
use http::{HeaderMap, HeaderValue};
use hyper::{client::HttpConnector, Body as HyperBody, Client as HyperClient};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use thiserror::Error;
use tower_http::trace::{DefaultMakeSpan, TraceLayer};
use tracing::{debug, error, info, trace, warn};
use websocket::handle_websocket;

/// Server Errors
#[derive(Debug, Error)]
pub enum Error {
    /// Invalid listening host
    #[error("invalid listening host: {0}")]
    InvalidHost(#[from] std::net::AddrParseError),
    /// TLS error
    #[error(transparent)]
    Tls(#[from] crate::tls::Error),
    /// IO error
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// Hyper error
    #[error("HTTP server error: {0}")]
    Hyper(#[from] hyper::Error),
}

/// Required state
#[derive(Clone, Debug)]
pub struct ServerState {
    /// Backend URL
    pub backend: Option<BackendUrl>,
    /// Websocket PSK
    pub ws_psk: Option<HeaderValue>,
    /// 404 response
    pub not_found_resp: String,
    /// Hyper client
    pub client: HyperClient<HttpsConnector<HttpConnector>, HyperBody>,
}

#[tracing::instrument(level = "trace")]
pub async fn server_main(args: ServerArgs) -> Result<(), Error> {
    let host = if args.host.starts_with('[') && args.host.ends_with(']') {
        // Remove brackets from IPv6 addresses
        &args.host[1..args.host.len() - 1]
    } else {
        &args.host
    };
    let sockaddr = (host.parse::<std::net::IpAddr>()?, args.port).into();

    #[cfg(feature = "rustls-native-roots")]
    let client_https = HttpsConnectorBuilder::new()
        .with_native_roots()
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .build();
    #[cfg(all(feature = "rustls-native-roots", not(feature = "rustls-native-roots")))]
    let client_https = HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .build();

    let state = ServerState {
        backend: args.backend,
        ws_psk: args.ws_psk,
        not_found_resp: args.not_found_resp,
        client: HyperClient::builder().build(client_https),
    };

    let mut app: Router<()> = Router::new()
        .route("/ws", get(ws_or_404_handler))
        .fallback(backend_or_404_handler)
        .with_state(state);
    if !args.obfs {
        app = app.route("/version", get(|| async { env!("CARGO_PKG_VERSION") }));
        app = app.route("/health", get(|| async { "OK" }));
    }
    let app = app.layer(
        TraceLayer::new_for_http()
            .make_span_with(DefaultMakeSpan::default().include_headers(false)),
    );

    if let Some(tls_key) = &args.tls_key {
        trace!("Enabling TLS");
        info!("Listening on wss://{}:{}/ws", args.host, args.port);
        let config = make_rustls_server_config(
            args.tls_cert.as_deref().unwrap(),
            tls_key,
            args.tls_ca.as_deref(),
        )
        .await?;
        let config = RustlsConfig::from_config(Arc::new(config));

        #[cfg(unix)]
        tokio::spawn(reload_cert_on_signal(
            config.clone(),
            args.tls_cert.unwrap(),
            tls_key.clone(),
            args.tls_ca.clone(),
        ));
        axum_server::bind_rustls(sockaddr, config)
            .serve(app.into_make_service())
            .await?;
    } else {
        info!("Listening on ws://{}:{}/ws", args.host, args.port);
        axum::Server::bind(&sockaddr)
            .serve(app.into_make_service())
            .await?;
    }
    Ok(())
}

/// `axum` example: `rustls_reload.rs`
#[cfg(unix)]
async fn reload_cert_on_signal(
    config: RustlsConfig,
    cert_path: String,
    key_path: String,
    client_ca_path: Option<String>,
) -> Result<(), Error> {
    let mut sigusr1 =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())?;
    loop {
        sigusr1.recv().await;
        info!("Reloading TLS certificate");
        let server_config =
            make_rustls_server_config(&cert_path, &key_path, client_ca_path.as_deref()).await?;
        config.reload_from_config(Arc::new(server_config));
    }
}

/// Reverse proxy and 404
async fn backend_or_404_handler(
    State(state): State<ServerState>,
    mut req: Request<Body>,
) -> Response {
    if let Some(backend) = &state.backend {
        let path = req.uri().path();
        let path_query = req
            .uri()
            .path_and_query()
            .map(|v| v.as_str())
            .unwrap_or(path);

        let uri = Uri::builder()
            // `unwrap()` should not panic because `BackendUrl` is validated
            // by clap.
            .scheme(backend.scheme.clone())
            .authority(backend.authority.clone())
            .path_and_query(format!("{}{}", backend.path.path(), path_query))
            .build()
            .unwrap();
        *req.uri_mut() = uri;
        return state.client.request(req).await.unwrap().into_response();
    }
    not_found_handler(State(state)).await
}

/// 404 handler
async fn not_found_handler(State(state): State<ServerState>) -> Response {
    (StatusCode::NOT_FOUND, state.not_found_resp).into_response()
}

/// Check the PSK and protocol version and upgrade to a websocket if the PSK matches (if required).
pub async fn ws_or_404_handler(
    State(state): State<ServerState>,
    ws: WebSocketUpgrade,
    headers: HeaderMap,
) -> Response {
    if let Some(predefined_psk) = &state.ws_psk {
        let supplied_psk = headers.get("x-penguin-psk");
        if supplied_psk.is_none() || supplied_psk.unwrap() != predefined_psk {
            warn!("Invalid PSK");
            return not_found_handler(State(state)).await;
        }
    }
    let proto = headers.get("sec-websocket-protocol");
    if proto.is_none() || proto.unwrap() != PROTOCOL_VERSION {
        warn!("Invalid protocol version");
        return not_found_handler(State(state)).await;
    }
    debug!("Upgrading to websocket");
    ws.on_upgrade(handle_websocket)
}
