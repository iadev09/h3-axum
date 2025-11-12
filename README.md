# h3-axum

Direct h3 → Axum. No middleman. Just an adapter.

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

# # What h3-axum Provides

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

## License 📝

MIT or Apache-2.0
