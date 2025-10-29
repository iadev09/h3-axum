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

use std::error::Error;

use bytes::{Buf, Bytes};
use http::{Request, Response};
use http_body_util::BodyExt;

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
    // Check error string representation for graceful close patterns
    // Since h3 error types are private/non-exhaustive, string matching is idiomatic.
    // Common graceful close patterns from production:
    // - Remote(Undefined(ConnectionClosed { error_code: NO_ERROR, ... }))
    // - ApplicationClose: 0x0 - QUIC NO_ERROR application close
    let err_debug = format!("{:?}", err);

    if err_debug.contains("NO_ERROR")
        || err_debug.contains("ApplicationClose: 0x0")
        || err_debug.contains("ApplicationClose(0x0)")
        || err_debug.contains("ConnectionClosed")
    {
        return true;
    }

    // Walk error source chain for typed QUIC-level causes
    let mut cur: &(dyn std::error::Error + 'static) = err;
    while let Some(src) = cur.source() {
        let src_debug = format!("{:?}", src);
        if src_debug.contains("NO_ERROR") || src_debug.contains("ApplicationClose") {
            return true;
        }
        cur = src;
    }

    false
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
{
    // Resolve the H3 request
    let (request_head, mut stream) = resolver.resolve_request().await?;

    // Read request body from H3
    let mut body_bytes = bytes::BytesMut::new();
    loop {
        match stream.recv_data().await {
            Ok(Some(mut chunk)) => {
                body_bytes.extend_from_slice(&chunk.copy_to_bytes(chunk.remaining()));
            }
            Ok(None) => break,
            Err(e) => {
                // Send 400 Bad Request on body read error
                let mut error_response: Response<()> = Response::new(());
                *error_response.status_mut() = http::StatusCode::BAD_REQUEST;
                let _ = stream.send_response(error_response).await;
                let _ = stream.finish().await;
                return Err(Box::new(e));
            }
        }
    }

    // Build Axum request
    let (parts, _) = request_head.into_parts();
    let axum_req = Request::from_parts(parts, axum::body::Body::from(body_bytes.freeze()));

    // Call Axum router
    let axum_resp = tower::ServiceExt::oneshot(app, axum_req).await?;

    // Send response back over H3
    let (parts, axum_body) = axum_resp.into_parts();
    let head_only: Response<()> = Response::from_parts(parts, ());
    stream.send_response(head_only).await?;

    // Stream response body
    let body_bytes = axum_body.collect().await?.to_bytes();
    if !body_bytes.is_empty() {
        stream.send_data(body_bytes.into()).await?;
    }

    stream.finish().await?;

    Ok(())
}
