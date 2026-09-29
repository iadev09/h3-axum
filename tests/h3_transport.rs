use std::{
    convert::Infallible,
    future::poll_fn,
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use axum::{
    Router,
    body::Body,
    extract::Request,
    response::Response,
    routing::{get, post},
};
use bytes::{Buf, Bytes};
use h3_quinn::quinn;
use http::{HeaderMap, HeaderValue, StatusCode};
use http_body::{Frame, SizeHint};
use http_body_util::BodyExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

const TEST_DEADLINE: Duration = Duration::from_secs(5);

struct TestServer {
    addr: SocketAddr,
    certificate: rustls::pki_types::CertificateDer<'static>,
    task: tokio::task::JoinHandle<()>,
}

impl TestServer {
    async fn spawn(app: Router) -> Self {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()])
            .expect("generate test certificate");
        let certificate = rustls::pki_types::CertificateDer::from(certified.cert);
        let key =
            rustls::pki_types::PrivateKeyDer::Pkcs8(certified.key_pair.serialize_der().into());

        let mut tls = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], key)
            .expect("configure test TLS");
        tls.alpn_protocols = vec![b"h3".to_vec()];

        let server_config = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(tls)
                .expect("configure test QUIC TLS"),
        ));
        let endpoint =
            quinn::Endpoint::server(server_config, SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                .expect("bind test endpoint");
        let addr = endpoint.local_addr().expect("read test address");

        let task = tokio::spawn(async move {
            let incoming = endpoint.accept().await.expect("accept QUIC connection");
            let connection = incoming.await.expect("complete QUIC handshake");
            let mut h3 = h3::server::builder()
                .build(h3_quinn::Connection::new(connection))
                .await
                .expect("build H3 server connection");

            while let Some(resolver) = h3.accept().await.expect("accept H3 request") {
                let app = app.clone();
                tokio::spawn(async move {
                    h3_axum::serve_h3_with_axum(app, resolver)
                        .await
                        .expect("serve Axum request");
                });
            }
        });

        Self {
            addr,
            certificate,
            task,
        }
    }

    async fn connect(
        &self,
    ) -> (
        quinn::Endpoint,
        h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>,
        tokio::task::JoinHandle<()>,
    ) {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(self.certificate.clone())
            .expect("trust test certificate");

        let mut tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        tls.alpn_protocols = vec![b"h3".to_vec()];

        let client_config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(tls)
                .expect("configure H3 client TLS"),
        ));
        let mut endpoint = quinn::Endpoint::client(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))
            .expect("bind H3 client");
        endpoint.set_default_client_config(client_config);

        let connection = endpoint
            .connect(self.addr, "localhost")
            .expect("start QUIC connection")
            .await
            .expect("complete QUIC connection");
        let (mut driver, sender) = h3::client::new(h3_quinn::Connection::new(connection))
            .await
            .expect("build H3 client");
        let driver = tokio::spawn(async move {
            let _ = poll_fn(|cx| driver.poll_close(cx)).await;
        });

        (endpoint, sender, driver)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn response_can_start_before_request_body_finishes() {
    tokio::time::timeout(TEST_DEADLINE, async {
        let app = Router::new().route(
            "/first-chunk",
            post(|request: Request| async move {
                let frame = request
                    .into_body()
                    .frame()
                    .await
                    .expect("request has a frame")
                    .expect("request frame is valid");
                frame.into_data().expect("request frame contains data")
            }),
        );
        let server = TestServer::spawn(app).await;
        let (_endpoint, mut sender, _driver) = server.connect().await;

        let request = http::Request::post("https://localhost/first-chunk")
            .body(())
            .expect("build request");
        let mut stream = sender.send_request(request).await.expect("send headers");
        stream
            .send_data(Bytes::from_static(b"first"))
            .await
            .expect("send first request chunk");

        // The response arrives while the request send side remains open. This
        // is the observable guarantee that the adapter does not buffer the
        // complete request before dispatching it to Axum.
        let response = stream.recv_response().await.expect("receive response");
        assert_eq!(response.status(), StatusCode::OK);
        let chunk = stream
            .recv_data()
            .await
            .expect("receive response data")
            .expect("response contains data");
        assert_eq!(chunk.chunk(), b"first");

        // Intentionally do not finish the request. Receiving the complete
        // response first is the property under test.
    })
    .await
    .expect("streaming request test reached its declared deadline");
}

#[tokio::test]
async fn response_streams_data_and_trailers() {
    tokio::time::timeout(TEST_DEADLINE, async {
        let app = Router::new().route(
            "/frames",
            get(|| async {
                let (frames, receiver) = mpsc::channel(2);
                frames
                    .send(Ok::<_, Infallible>(Frame::data(Bytes::from_static(
                        b"event: ready\n\n",
                    ))))
                    .await
                    .expect("queue response data");
                let mut trailers = HeaderMap::new();
                trailers.insert("x-stream-result", HeaderValue::from_static("complete"));
                frames
                    .send(Ok(Frame::trailers(trailers)))
                    .await
                    .expect("queue response trailers");
                drop(frames);

                let mut response = Response::new(Body::new(StreamBody {
                    inner: ReceiverStream::new(receiver),
                }));
                response.headers_mut().insert(
                    http::header::CONTENT_TYPE,
                    HeaderValue::from_static("text/event-stream"),
                );
                response
            }),
        );
        let server = TestServer::spawn(app).await;
        let (_endpoint, mut sender, _driver) = server.connect().await;

        let request = http::Request::get("https://localhost/frames")
            .body(())
            .expect("build request");
        let mut stream = sender.send_request(request).await.expect("send request");
        stream.finish().await.expect("finish request");

        let response = stream.recv_response().await.expect("receive response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(http::header::CONTENT_TYPE),
            Some(&HeaderValue::from_static("text/event-stream"))
        );
        let data = stream
            .recv_data()
            .await
            .expect("receive response data")
            .expect("response contains data");
        assert_eq!(data.chunk(), b"event: ready\n\n");
        assert!(
            stream
                .recv_data()
                .await
                .expect("finish response data")
                .is_none()
        );
        let trailers = stream
            .recv_trailers()
            .await
            .expect("receive trailers")
            .expect("response has trailers");
        assert_eq!(
            trailers.get("x-stream-result"),
            Some(&HeaderValue::from_static("complete"))
        );
    })
    .await
    .expect("response frame test reached its declared deadline");
}

#[tokio::test]
async fn request_trailers_reach_axum_handlers() {
    tokio::time::timeout(TEST_DEADLINE, async {
        let app = Router::new().route(
            "/trailers",
            post(|request: Request| async move {
                let mut body = request.into_body();
                while let Some(frame) = body.frame().await {
                    let frame = frame.expect("request frame is valid");
                    if let Ok(trailers) = frame.into_trailers() {
                        return trailers
                            .get("x-request-result")
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or("missing")
                            .to_owned();
                    }
                }
                "absent".to_owned()
            }),
        );
        let server = TestServer::spawn(app).await;
        let (_endpoint, mut sender, _driver) = server.connect().await;

        let request = http::Request::post("https://localhost/trailers")
            .body(())
            .expect("build request");
        let mut stream = sender.send_request(request).await.expect("send request");
        let mut trailers = HeaderMap::new();
        trailers.insert("x-request-result", HeaderValue::from_static("received"));
        stream
            .send_trailers(trailers)
            .await
            .expect("send request trailers");
        stream.finish().await.expect("finish request trailers");

        let response = stream.recv_response().await.expect("receive response");
        assert_eq!(response.status(), StatusCode::OK);
        let data = stream
            .recv_data()
            .await
            .expect("receive response data")
            .expect("response contains data");
        assert_eq!(data.chunk(), b"received");
    })
    .await
    .expect("request trailer test reached its declared deadline");
}

struct StreamBody {
    inner: ReceiverStream<Result<Frame<Bytes>, Infallible>>,
}

impl http_body::Body for StreamBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        use tokio_stream::Stream;

        std::pin::Pin::new(&mut self.get_mut().inner).poll_next(cx)
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::default()
    }
}
