use bytes::Bytes;
use futures_util::{StreamExt, stream::FuturesUnordered};
use tokio::task::JoinSet;

use crate::{BoxError, is_graceful_h3_close, serve_resolved_h3_with_axum};

/// An HTTP/3 server connection backed by Quinn.
pub type QuinnH3Connection = h3::server::Connection<h3_quinn::Connection, Bytes>;

/// Drive one HTTP/3 connection and dispatch every resolved request through an
/// Axum router.
///
/// Request heads are resolved concurrently so a peer that opens a stream but
/// delays its HEADERS frame cannot block unrelated streams on the same
/// connection. Each ordinary request is then served independently; a reset or
/// body error remains local to that request stream.
///
/// With the `webtransport` feature enabled, a WebTransport CONNECT request is
/// sent through the same router with a
/// [`WebTransportUpgrade`](crate::WebTransportUpgrade) request extractor. A
/// handler that accepts the upgrade takes ownership of the connection.
///
/// The caller still owns UDP binding, TLS and QUIC configuration. In
/// particular, WebTransport support must be advertised on the `h3` server
/// builder before constructing `connection`.
pub async fn serve_h3_connection_with_axum(
    app: axum::Router,
    mut connection: QuinnH3Connection,
) -> Result<(), BoxError> {
    let mut resolving = FuturesUnordered::new();
    let mut requests = JoinSet::new();
    let mut accepting = true;

    loop {
        if !accepting && resolving.is_empty() {
            break;
        }

        tokio::select! {
            result = requests.join_next(), if !requests.is_empty() => {
                handle_request_task(result)?;
            }
            resolved = resolving.next(), if !resolving.is_empty() => {
                let Some(resolved) = resolved else {
                    continue;
                };
                let (request, stream) = resolved?;

                #[cfg(feature = "webtransport")]
                if crate::webtransport::is_webtransport_connect(&request) {
                    match crate::webtransport::dispatch(
                        app.clone(),
                        request,
                        stream,
                        connection,
                    ).await? {
                        Some(connection_back) => {
                            connection = connection_back;
                            continue;
                        }
                        None => {
                            resolving.clear();
                            break;
                        }
                    }
                }

                let app = app.clone();
                requests.spawn(async move {
                    // Stream failures are scoped to this request. The low-level
                    // `serve_h3_with_axum` API remains available to callers that
                    // want to inspect each one themselves.
                    let _ = serve_resolved_h3_with_axum::<h3_quinn::Connection>(
                        app,
                        request,
                        stream,
                    ).await;
                });
            }
            accepted = connection.accept(), if accepting => {
                match accepted {
                    Ok(Some(resolver)) => resolving.push(resolver.resolve_request()),
                    Ok(None) => accepting = false,
                    Err(error) if is_graceful_h3_close(&error) => accepting = false,
                    Err(error) => return Err(Box::new(error)),
                }
            }
        }
    }

    while let Some(result) = requests.join_next().await {
        handle_request_task(Some(result))?;
    }

    Ok(())
}

fn handle_request_task(result: Option<Result<(), tokio::task::JoinError>>) -> Result<(), BoxError> {
    if let Some(Err(error)) = result {
        return Err(Box::new(error));
    }
    Ok(())
}
