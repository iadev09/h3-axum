use std::{
    error::Error,
    fmt,
    sync::{Arc, Mutex},
};

use axum::{
    extract::FromRequestParts,
    http::{Method, StatusCode, request::Parts},
};
use bytes::Bytes;
use h3::ext::Protocol;
use http::Request;
use tower::ServiceExt;

use crate::{BoxError, connection::QuinnH3Connection, send_axum_response};

type QuinnBidiStream = <h3_quinn::Connection as h3::quic::OpenStreams<Bytes>>::BidiStream;
type H3RequestStream = h3::server::RequestStream<QuinnBidiStream, Bytes>;

/// A WebTransport session backed by the Quinn HTTP/3 connection.
pub type WebTransportSession =
    h3_webtransport::server::WebTransportSession<h3_quinn::Connection, Bytes>;

struct PendingUpgrade {
    request: Request<()>,
    stream: H3RequestStream,
    connection: QuinnH3Connection,
}

/// Axum extractor for accepting a WebTransport CONNECT request.
///
/// This extractor is present only on WebTransport CONNECT requests dispatched
/// by [`crate::serve_h3_connection_with_axum`]. Call [`Self::accept`] before
/// the handler returns. Accepting transfers ownership of the HTTP/3 connection
/// to the returned session, so the connection driver stops accepting ordinary
/// requests from that connection.
#[derive(Clone)]
pub struct WebTransportUpgrade {
    pending: Arc<Mutex<Option<PendingUpgrade>>>,
}

impl WebTransportUpgrade {
    /// Accept the CONNECT request and take ownership of its WebTransport
    /// session.
    ///
    /// The WebTransport handshake response is sent by this operation. The
    /// Axum handler's eventual response is ignored after a successful claim.
    pub async fn accept(self) -> Result<WebTransportSession, WebTransportUpgradeError> {
        let pending = self
            .pending
            .lock()
            .map_err(|_| WebTransportUpgradeError::StatePoisoned)?
            .take()
            .ok_or(WebTransportUpgradeError::AlreadyClaimed)?;

        WebTransportSession::accept(pending.request, pending.stream, pending.connection)
            .await
            .map_err(WebTransportUpgradeError::Handshake)
    }
}

impl<S> FromRequestParts<S> for WebTransportUpgrade
where
    S: Send + Sync,
{
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Self>()
            .cloned()
            .ok_or(StatusCode::BAD_REQUEST)
    }
}

/// Failure while claiming a WebTransport upgrade.
#[derive(Debug)]
pub enum WebTransportUpgradeError {
    /// The handler or another clone already claimed this upgrade.
    AlreadyClaimed,
    /// The internal upgrade state was poisoned by a panic.
    StatePoisoned,
    /// The HTTP/3 WebTransport handshake failed.
    Handshake(h3::error::StreamError),
}

impl fmt::Display for WebTransportUpgradeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyClaimed => formatter.write_str("WebTransport upgrade already claimed"),
            Self::StatePoisoned => formatter.write_str("WebTransport upgrade state is poisoned"),
            Self::Handshake(error) => write!(formatter, "WebTransport handshake failed: {error}"),
        }
    }
}

impl Error for WebTransportUpgradeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Handshake(error) => Some(error),
            Self::AlreadyClaimed | Self::StatePoisoned => None,
        }
    }
}

pub(crate) fn is_webtransport_connect(request: &Request<()>) -> bool {
    request.method() == Method::CONNECT
        && request.extensions().get::<Protocol>() == Some(&Protocol::WEB_TRANSPORT)
}

pub(crate) async fn dispatch(
    app: axum::Router,
    request: Request<()>,
    stream: H3RequestStream,
    connection: QuinnH3Connection,
) -> Result<Option<QuinnH3Connection>, BoxError> {
    let accept_request = request_for_accept(&request)?;
    let pending = Arc::new(Mutex::new(Some(PendingUpgrade {
        request: accept_request,
        stream,
        connection,
    })));

    let (mut parts, ()) = request.into_parts();
    parts.extensions.insert(WebTransportUpgrade {
        pending: Arc::clone(&pending),
    });

    let response = app
        .oneshot(Request::from_parts(parts, axum::body::Body::empty()))
        .await?;

    let unclaimed = pending
        .lock()
        .map_err(|_| WebTransportUpgradeError::StatePoisoned)?
        .take();

    let Some(unclaimed) = unclaimed else {
        return Ok(None);
    };

    let (send_stream, _recv_stream) = unclaimed.stream.split();
    send_axum_response(send_stream, response).await?;

    Ok(Some(unclaimed.connection))
}

fn request_for_accept(request: &Request<()>) -> Result<Request<()>, http::Error> {
    let mut accept = Request::builder()
        .method(request.method().clone())
        .uri(request.uri().clone())
        .version(request.version());

    *accept.headers_mut().expect("request builder is valid") = request.headers().clone();

    let mut accept = accept.body(())?;
    if let Some(protocol) = request.extensions().get::<Protocol>() {
        accept.extensions_mut().insert(*protocol);
    }

    Ok(accept)
}
