# h3-axum

Direct `h3` → Axum. No listener or TLS policy hidden in the middle.

> With HTTP/3, the web is a **transport**, not just a verbs API.

---

## The Problem

🧭 You have an Axum router. You want HTTP/3.

Write your own h3 ↔ Axum adapter.  
Handle body conversions, protocol details, error cases.

---

## The Solution 🛠️

```rust
// 1. Your Axum router (unchanged)
let app = Router::new()
.route("/users", get(list_users));

// 2. Standard HTTP/3 setup (h3 + quinn)
let h3_conn = h3::server::builder()
.build(h3_quinn::Connection::new(conn))
.await?;

// 3. Bridge h3 → Axum (one line)
h3_axum::serve_h3_with_axum(app, resolver).await?;
```

That's it. Direct h3 → Axum.

--- 

## What h3-axum Provides

The crate adapts one resolved HTTP/3 request stream to an Axum `Router`:

```rust
// Bridge h3 ↔ Axum
h3_axum::serve_h3_with_axum(app, resolver).await?;

// Distinguish graceful closes from errors
if h3_axum::is_graceful_h3_close( & err) { /* ... */ }
```

For callers that want the crate to drive a complete Quinn-backed connection,
the connection-level API dispatches every request through the same router:

```rust
h3_axum::serve_h3_connection_with_axum(app, h3_conn).await?;
```

The bridge preserves the HTTP message rather than collecting it into one
buffer:

- request data is exposed to Axum as a streaming body;
- a handler may start its response before the request body is complete;
- response data is forwarded frame by frame, including long-lived SSE bodies;
- request and response trailers are preserved; and
- QUIC flow control remains the source of backpressure in both directions.

Connection acceptance, TLS and QUIC configuration, task ownership, graceful
shutdown, and application policy remain with the caller. In particular,
applications decide whether a request is safe to process as 0-RTT data. A
future `h3` release is expected to expose that stream metadata directly; the
adapter will surface it through Axum request extensions when it is available
from the published dependency.

The required upstream work is already merged:

- [`hyperium/h3#323`](https://github.com/hyperium/h3/pull/323) exposes 0-RTT
  state on each request stream. Once released, `h3-axum` can attach that state
  to the Axum request without guessing whether a method is idempotent.
- [`hyperium/h3#336`](https://github.com/hyperium/h3/pull/336) preserves QUIC
  `ConnectionClosed` errors as structured values. The adapter already uses
  `ConnectionError::is_h3_no_error()`, so a dependency update will extend its
  graceful-close classification without debug-string parsing.

## WebTransport

WebTransport is opt-in because it adds the `h3-webtransport` and H3 datagram
dependencies:

```toml
h3-axum = { version = "0.3", features = ["webtransport"] }
```

The connection driver recognizes a WebTransport extended CONNECT request and
sends it through the same Axum router as ordinary HTTP/3 requests. The handler
claims the connection with the `WebTransportUpgrade` extractor:

```rust
use axum::{Router, routing::{any, get}};
use h3_axum::WebTransportUpgrade;

let app = Router::new()
    .route("/health", get(|| async { "ok" }))
    // `WebTransportUpgrade` rejects ordinary requests, so this route still
    // handles only a WebTransport extended CONNECT.
    .route("/session", any(webtransport));

async fn webtransport(upgrade: WebTransportUpgrade) {
    let session = upgrade.accept().await.expect("accept WebTransport session");

    // Own and drive `session` here until this WebTransport session ends.
    // For example: session.accept_bi().await, accept_uni(), or datagrams.
    run_session(session).await;
}

let mut builder = h3::server::builder();
builder
    .enable_webtransport(true)
    .enable_extended_connect(true)
    .enable_datagram(true)
    .max_webtransport_sessions(1);

let connection = builder
    .build(h3_quinn::Connection::new(quinn_connection))
    .await?;
h3_axum::serve_h3_connection_with_axum(app, connection).await?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

`accept()` must be awaited before the handler returns. A successful claim
transfers the complete HTTP/3 connection to the WebTransport session; the
handler's Axum response is therefore not sent. If a matching route chooses not
to call `accept()`, its normal Axum response is sent and the connection driver
continues accepting HTTP/3 requests. Listener ownership, TLS, QUIC transport
configuration and the WebTransport session loop remain application policy.

---

## Example ▶️

**Complete working server** in [`examples/server.rs`](examples/server.rs):

- Axum Router with extractors (Path, Query, Json)
- Quinn + h3 setup with TLS
- Connection lifecycle and graceful shutdown
- Error handling

**Run it:**

```bash
cargo run --example server

# Test:
curl --http3-only -k https://localhost:4433/
curl --http3-only -k https://localhost:4433/users/123
```

## License 📝

MIT or Apache-2.0
