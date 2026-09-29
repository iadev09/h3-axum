//! # h3-axum
//!
//! Transport your Axum router over HTTP/3.
//!
//! Use your existing Axum handlers, extractors, and middleware with HTTP/3/QUIC
//! without changing your application code.
//!
//! ## Quick Start
//!
//! ```ignore
//! use h3_axum::serve_h3_with_axum;
//!
//! // Your normal Axum router (unchanged!)
//! let app = Router::new()
//!     .route("/", get(handler));
//!
//! // Serve it over H3 (one line)
//! serve_h3_with_axum(app, resolver).await?;
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod connection;
#[cfg(feature = "webtransport")]
mod webtransport;

use std::{
    error::Error,
    pin::Pin,
    task::{Context, Poll},
};

use bytes::{Buf, Bytes};
use http::{Request, Response};
use http_body::{Body, Frame};

pub use connection::serve_h3_connection_with_axum;
#[cfg(feature = "webtransport")]
pub use webtransport::{WebTransportSession, WebTransportUpgrade, WebTransportUpgradeError};

/// Boxed error type
pub type BoxError = Box<dyn Error + Send + Sync + 'static>;

/// Check if an H3 connection error represents a graceful close.
///
/// HTTP/3 and QUIC have multiple ways to signal graceful connection closure.
/// This function identifies them to avoid logging benign closes as errors.
///
/// # Example
///
/// ```ignore
/// match h3_conn.accept().await {
///     Err(e) if is_graceful_h3_close(&e) => {
///         tracing::debug!("Connection closed gracefully");
///     }
///     Err(e) => {
///         tracing::error!("Connection error: {:?}", e);
///     }
///     // ...
/// }
/// ```
pub fn is_graceful_h3_close(err: &h3::error::ConnectionError) -> bool {
    // h3 0.0.8 maps transport-level QUIC ConnectionClosed errors to
    // Undefined, so its typed predicate cannot yet recognize that graceful
    // close. Keep the compatibility inspection until hyperium/h3#336 is part
    // of a published h3 release.
    let err_debug = format!("{err:?}");

    if err_debug.contains("NO_ERROR")
        || err_debug.contains("ApplicationClose: 0x0")
        || err_debug.contains("ApplicationClose(0x0)")
        || err_debug.contains("ConnectionClosed")
    {
        return true;
    }

    let mut source: &(dyn Error + 'static) = err;
    while let Some(error) = source.source() {
        let source_debug = format!("{error:?}");
        if source_debug.contains("NO_ERROR") || source_debug.contains("ApplicationClose") {
            return true;
        }
        source = error;
    }

    false
}

/// An Axum request body backed directly by an HTTP/3 receive stream.
///
/// Keeping the request body on the QUIC stream preserves transport
/// backpressure and lets a handler start producing a response before the
/// request body has been fully received.
struct H3RequestBody<S>
where
    S: h3::quic::RecvStream,
{
    stream: h3::server::RequestStream<S, Bytes>,
    data_complete: bool,
    trailers_complete: bool,
}

impl<S> H3RequestBody<S>
where
    S: h3::quic::RecvStream,
{
    fn new(stream: h3::server::RequestStream<S, Bytes>) -> Self {
        Self {
            stream,
            data_complete: false,
            trailers_complete: false,
        }
    }
}

impl<S> Body for H3RequestBody<S>
where
    S: h3::quic::RecvStream + Unpin,
{
    type Data = Bytes;
    type Error = h3::error::StreamError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if !self.data_complete {
            match self.stream.poll_recv_data(cx) {
                Poll::Ready(Ok(Some(mut chunk))) => {
                    let bytes = chunk.copy_to_bytes(chunk.remaining());
                    return Poll::Ready(Some(Ok(Frame::data(bytes))));
                }
                Poll::Ready(Ok(None)) => self.data_complete = true,
                Poll::Ready(Err(error)) => return Poll::Ready(Some(Err(error))),
                Poll::Pending => return Poll::Pending,
            }
        }

        if self.trailers_complete {
            return Poll::Ready(None);
        }

        match self.stream.poll_recv_trailers(cx) {
            Poll::Ready(Ok(Some(trailers))) => {
                self.trailers_complete = true;
                Poll::Ready(Some(Ok(Frame::trailers(trailers))))
            }
            Poll::Ready(Ok(None)) => {
                self.trailers_complete = true;
                Poll::Ready(None)
            }
            Poll::Ready(Err(error)) => {
                self.trailers_complete = true;
                Poll::Ready(Some(Err(error)))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Serve an Axum Router over an H3 request.
///
/// This is the main function that bridges your existing Axum Router to HTTP/3.
/// It handles the H3 protocol details so your service doesn't have to.
///
/// # Example
///
/// ```ignore
/// use axum::{Router, routing::get};
/// use h3_axum::serve_h3_with_axum;
///
/// let app = Router::new()
///     .route("/", get(|| async { "Hello H3!" }));
///
/// // When you get an H3 request:
/// serve_h3_with_axum(app, resolver).await?;
/// ```
pub async fn serve_h3_with_axum<Q>(
    app: axum::Router,
    resolver: h3::server::RequestResolver<Q, Bytes>,
) -> Result<(), BoxError>
where
    Q: h3::quic::Connection<Bytes>,
    Q::BidiStream: h3::quic::BidiStream<Bytes>,
    <Q::BidiStream as h3::quic::BidiStream<Bytes>>::RecvStream: Send + Unpin + 'static,
{
    // Resolve the H3 request
    let (request_head, stream) = resolver.resolve_request().await?;
    serve_resolved_h3_with_axum::<Q>(app, request_head, stream).await
}

pub(crate) async fn serve_resolved_h3_with_axum<Q>(
    app: axum::Router,
    request_head: Request<()>,
    stream: h3::server::RequestStream<Q::BidiStream, Bytes>,
) -> Result<(), BoxError>
where
    Q: h3::quic::Connection<Bytes>,
    Q::BidiStream: h3::quic::BidiStream<Bytes>,
    <Q::BidiStream as h3::quic::BidiStream<Bytes>>::RecvStream: Send + Unpin + 'static,
{
    let (send_stream, recv_stream) = stream.split();

    // Build Axum request
    let (parts, _) = request_head.into_parts();
    let body = axum::body::Body::new(H3RequestBody::new(recv_stream));
    let axum_req = Request::from_parts(parts, body);

    // Call Axum router
    let axum_resp = tower::ServiceExt::oneshot(app, axum_req).await?;

    send_axum_response(send_stream, axum_resp).await
}

pub(crate) async fn send_axum_response<S>(
    mut send_stream: h3::server::RequestStream<S, Bytes>,
    axum_resp: Response<axum::body::Body>,
) -> Result<(), BoxError>
where
    S: h3::quic::SendStream<Bytes>,
{
    // Send response back over H3
    let (parts, axum_body) = axum_resp.into_parts();
    let head_only: Response<()> = Response::from_parts(parts, ());
    send_stream.send_response(head_only).await?;

    // Forward each body frame so streaming responses retain backpressure and
    // response trailers are not discarded.
    let mut body_stream = std::pin::pin!(axum_body);
    while let Some(frame_result) = http_body_util::BodyExt::frame(&mut body_stream).await {
        let frame = frame_result?;
        match frame.into_data() {
            Ok(chunk) if !chunk.is_empty() => send_stream.send_data(chunk).await?,
            Ok(_) => {}
            Err(frame) => {
                if let Ok(trailers) = frame.into_trailers() {
                    send_stream.send_trailers(trailers).await?;
                }
            }
        }
    }

    send_stream.finish().await?;

    Ok(())
}
