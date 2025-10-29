//! Complete HTTP/3 + Axum integration example
//!
//! This example shows how to use your existing Axum Router over HTTP/3:
//! - Build your Axum Router with all its ergonomics (extractors, state, etc.)
//! - Use h3_axum::serve_h3_with_axum() to transport it over HTTP/3
//! - Use h3_axum::is_graceful_h3_close() for proper error handling
//!
//! The key line is just:
//!   h3_axum::serve_h3_with_axum(app, resolver).await?;
//!
//! That's it! Your Axum router now speaks HTTP/3.
//!
//! Run with: cargo run --example server

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{get, post},
    Router,
};
use bytes::Bytes;
use h3_quinn::quinn;
use serde::{Deserialize, Serialize};

// Application state (shared across requests)
#[derive(Clone)]
struct AppState {
    message: String,
}

// JSON request/response types
#[derive(Deserialize)]
struct CreateUser {
    username: String,
    email: String,
}

#[derive(Serialize)]
struct User {
    id: u64,
    username: String,
    email: String,
}

#[derive(Deserialize)]
struct Pagination {
    page: Option<u32>,
    per_page: Option<u32>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Install crypto provider
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("Failed to install crypto provider");

    // Setup tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Build Axum app with all the nice features
    let app_state = AppState {
        message: "Hello from H3!".to_string(),
    };

    let app = Router::new()
        .route("/", get(root_handler))
        .route("/users", get(list_users).post(create_user))
        .route("/users/{id}", get(get_user))
        .route("/echo", post(echo_json))
        .with_state(app_state);

    // ========================================================================
    // STANDARD HTTP/3 SERVER SETUP
    // All of this is direct configuration - no middleman, no abstractions
    // ========================================================================

    // Generate self-signed certificate (for production, use real certs)
    // See: https://docs.rs/rcgen
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into());
    let cert = rustls::pki_types::CertificateDer::from(cert.cert);

    // Configure TLS with rustls (standard rustls configuration)
    // See: https://docs.rs/rustls/latest/rustls/server/struct.ServerConfig.html
    let mut tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)?;

    // HTTP/3 requires ALPN protocol negotiation
    tls_config.alpn_protocols = vec![b"h3".to_vec()];

    // Enable 0-RTT (early data) - allows clients to send data in first packet
    // WARNING: 0-RTT data can be replayed, only use for idempotent operations
    tls_config.max_early_data_size = u32::MAX;

    // Configure QUIC transport with Quinn (standard Quinn configuration)
    // See: https://docs.rs/quinn/latest/quinn/struct.ServerConfig.html
    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(tls_config)?,
    ));

    // Configure QUIC transport parameters directly
    // ALL OF THIS IS STANDARD QUINN - you have full control, no middleman
    // See: https://docs.rs/quinn/latest/quinn/struct.TransportConfig.html
    let transport_config = Arc::get_mut(&mut server_config.transport).unwrap();
    transport_config
        .max_concurrent_bidi_streams(100_u32.into())   // Max concurrent HTTP requests
        .max_concurrent_uni_streams(100_u32.into())    // Max concurrent unidirectional streams
        .max_idle_timeout(Some(std::time::Duration::from_secs(60).try_into()?));  // Connection timeout

    // Bind and listen
    let addr: SocketAddr = "127.0.0.1:4433".parse()?;
    let endpoint = quinn::Endpoint::server(server_config, addr)?;

    tracing::info!("HTTP/3 server with Axum Router listening on https://{}", addr);
    tracing::info!("Try:");
    tracing::info!("  curl --http3-only -k https://localhost:4433/");
    tracing::info!("  curl --http3-only -k https://localhost:4433/users");
    tracing::info!("  curl --http3-only -k https://localhost:4433/users/123");
    tracing::info!("  curl --http3-only -k https://localhost:4433/users?page=2");

    // Accept connections
    while let Some(incoming) = endpoint.accept().await {
        let app = app.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(incoming, app).await {
                tracing::error!("Connection error: {}", e);
            }
        });
    }

    Ok(())
}

async fn handle_connection(
    incoming: quinn::Incoming,
    app: Router,
) -> Result<(), Box<dyn std::error::Error>> {
    let conn = incoming.await?;
    let remote_addr = conn.remote_address();

    tracing::info!("New connection from {}", remote_addr);

    // Build H3 connection (standard h3 + h3-quinn integration)
    // See: https://docs.rs/h3/latest/h3/server/struct.Builder.html
    // You can configure H3 protocol settings directly here:
    //   .max_field_section_size(8192) - header size limits
    //   .send_grease(true) - GREASE for compatibility testing
    let h3_conn = h3::server::builder()
        .build(h3_quinn::Connection::new(conn))
        .await?;

    tokio::pin!(h3_conn);

    // Accept H3 requests (standard h3 API)
    loop {
        match h3_conn.accept().await {
            Ok(Some(resolver)) => {
                let app = app.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_request(resolver, app).await {
                        tracing::error!("Request error: {}", e);
                    }
                });
            }
            Ok(None) => {
                tracing::info!("Connection closed by peer: {}", remote_addr);
                break;
            }
            Err(e) => {
                // h3-axum helper: distinguish graceful closes from errors
                if h3_axum::is_graceful_h3_close(&e) {
                    tracing::debug!("Connection closed gracefully: {}", remote_addr);
                } else {
                    tracing::error!("H3 connection error: {:?}", e);
                }
                break;
            }
        }
    }

    Ok(())
}

async fn handle_request(
    resolver: h3::server::RequestResolver<h3_quinn::Connection, Bytes>,
    app: Router,
) -> Result<(), h3_axum::BoxError> {
    // Use h3-axum to serve Axum over H3!
    h3_axum::serve_h3_with_axum(app, resolver).await
}

// ============================================================================
// Axum Handlers - Full Axum ergonomics!
// ============================================================================

async fn root_handler(State(state): State<AppState>) -> impl IntoResponse {
    state.message
}

async fn list_users(Query(pagination): Query<Pagination>) -> impl IntoResponse {
    let page = pagination.page.unwrap_or(1);
    let per_page = pagination.per_page.unwrap_or(10);

    let users = vec![
        User {
            id: 1,
            username: "alice".to_string(),
            email: "alice@example.com".to_string(),
        },
        User {
            id: 2,
            username: "bob".to_string(),
            email: "bob@example.com".to_string(),
        },
    ];

    tracing::info!("Listing users: page={}, per_page={}", page, per_page);
    Json(users)
}

async fn get_user(Path(user_id): Path<u64>) -> impl IntoResponse {
    tracing::info!("Getting user: {}", user_id);

    let user = User {
        id: user_id,
        username: format!("user_{}", user_id),
        email: format!("user_{}@example.com", user_id),
    };

    Json(user)
}

async fn create_user(Json(payload): Json<CreateUser>) -> impl IntoResponse {
    tracing::info!("Creating user: {}", payload.username);

    let user = User {
        id: 42, // In real app, generate from DB
        username: payload.username,
        email: payload.email,
    };

    (StatusCode::CREATED, Json(user))
}

async fn echo_json(Json(value): Json<serde_json::Value>) -> impl IntoResponse {
    Json(value)
}
