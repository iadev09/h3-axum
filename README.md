# h3-axum 🔗

**QUIC** 🟣 &nbsp;&nbsp; **H3** 🧭 &nbsp;&nbsp; **Axum** 🦀 &nbsp;&nbsp; **Tower** 🏗️

Direct h3 → Axum. No middleman. Just an adapter.

---

## Philosophy 🧠

> **"HTTP/3 is not a version change."**
> *Truth just walked in.*

The abstractions that once pretended to hold the web together now dissolve under real
flow. For example, what is “session” or “login”?

We think we know *why* we need them — but they are artifacts of an older web,  
patches over the void.

Technically, it’s just changing a cookie string —  
an illusion of continuity built to hide the gap.

HTTP/1.x and HTTP/2 frameworks were wrappers — necessary abstractions around incomplete transports.  
But with HTTP/3, that layer of interpretation loses meaning.  
You don’t wrap a flow. You shift with it.

Our code is small, maybe even incomplete.
But that’s not a weakness — it’s awareness.
We know the shift we’re standing in.
Simplicity here isn’t limitation, it’s intention.


---

## The Truth 🔦

**HTTP/3 is not "HTTP/2 with QUIC"** — it's a **transport-level mutation**:

- **Streams are first-class.** Not multiplexed over one pipe, but independent QUIC streams.
- **Flow control at the right layer.** Application and transport backpressure finally align.
- **TLS is fused into transport.** 0-RTT and ALPN are not bolt-ons; they're native.

**Consequence:** Middleware designed to patch HTTP/1.1's limitations becomes unnecessary — or harmful.

> With HTTP/3, the web is a **transport**, not just a verbs API.

---

## The Problem 🧩

🧭 You have an Axum router. You want HTTP/3.

**Option A: Hyper**
`Your app → Axum → Hyper → h3 → quinn → QUIC`
Extra abstraction. Less control.

**Option B: DIY**
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

**That's it.** Direct h3 → Axum.

---

## What You Configure ⚙️

**Standard HTTP/3 setup** (same whether you use Hyper or not):

```rust
// QUIC transport (see docs.rs/quinn)
transport_config
.max_concurrent_bidi_streams(100_u32.into())
.max_idle_timeout(Some(Duration::from_secs(60).try_into() ? ));

// H3 protocol (see docs.rs/h3)
let h3_conn = h3::server::builder()
.max_field_section_size(8192)
.build(h3_quinn::Connection::new(conn))
.await?;

// TLS (see docs.rs/rustls)
tls_config.alpn_protocols = vec![b"h3".to_vec()];
tls_config.max_early_data_size = u32::MAX; // 0-RTT
```

**You configure h3 and quinn directly.** No abstractions, no magic.

---

## What h3-axum Provides 🎁

Just the adapter:

```rust
// Bridge h3 ↔ Axum
h3_axum::serve_h3_with_axum(app, resolver).await?;

// Distinguish graceful closes from errors
if h3_axum::is_graceful_h3_close( & err) { /* ... */ }
```

That's the entire library.

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

---

## Installation 📦

```toml
[dependencies]
h3-axum = "0.1"
h3 = "0.0.8"
h3-quinn = "0.0.10"
quinn = "0.11"
axum = "0.8"
tokio = { version = "1", features = ["full"] }
rustls = { version = "0.23", features = ["aws-lc-rs"] }
```

---

## Is This Production Ready? ✅

**The adapter:** ✅ Extracted from production code handling real HTTP/3 traffic.

**The configuration:** ✅ You have full control via standard h3/quinn APIs.

> **Honestly?** You don't even need this crate. You could replicate the adapter yourself in ~100 lines.
> We extracted it because it's common logic, not because it's complex.
> Think of `h3-axum` as **convenience**, not necessity.

You still handle:

- QUIC/TLS configuration (unavoidable)
- Connection lifecycle (application-specific)
- Context enrichment (real-IP, GeoIP, sessions, etc.)

The example shows production patterns for all of these.

---

## API 📚

### `serve_h3_with_axum(app: Router, resolver: RequestResolver) -> Result<(), BoxError>`

Serves an Axum Router over an HTTP/3 request stream.

**Handles:**

- Request body collection
- Body type conversion (h3 ↔ Axum)
- Response streaming
- Error cases

### `is_graceful_h3_close(err: &h3::error::ConnectionError) -> bool`

Returns `true` if the error represents a graceful close (NO_ERROR, ApplicationClose, etc.).

**Why?** HTTP/3 logs graceful closes as errors:

```
ERROR H3 error: Remote(Undefined(ConnectionClosed { error_code: NO_ERROR, ... }))
```

That's **not an error**. Use this helper to filter logs.

---

## Limitations 🚧

- This crate adapts classic **HTTP request/response** over **HTTP/3**.  
  It **does not** implement **WebTransport** bidirectional streams (bidi) or datagrams.
- If you need **socket‑like bidi streaming** (chat, real‑time state sync, multiplexed channels), see **Claviron Chat** —
  a minimal example using **WebTransport + `nwd1` frames + `NetId64`**.  
  *(reference: claviron‑chat repo)*
- Roadmap: bidi support will live in a **sibling crate** (e.g. `h3-webtransport-axum`), keeping `h3-axum` small and
  focused.

---

## Comparison ⚖️

| Approach          | Layers                    | Direct Control |
|-------------------|---------------------------|----------------|
| Axum + Hyper + h3 | Axum → Hyper → h3 → quinn | Limited        |
| Axum + h3-axum    | Axum → h3 → quinn         | Full(direct)   |

---

## Status 📡

**v0.1** — Production-quality adapter.

Extracted from production. Battle-tested. Configuration examples show real patterns.

---

## License 📝

MIT OR Apache-2.0

---

## Contributing 🤝

Found a bug? Open an issue.
Want better examples? PR welcome.

100 lines of code — and probably 10,000 lines of commentary.
That's fine. Let's meet again in other projects. :)

---

## Credits 🙏

**Philosophy:** ChatGPT — words that see beyond code.  
**Implementation:** Claude Code — the *tatlı amele* 🍯 who built the bridge.  
**Vision & Reality:** iadev09 — the human who shaped it.