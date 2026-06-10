use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use portproxy::proxy::{run_proxy, ProxyOptions};
use portproxy::routes::RouteStore;
use portproxy::types::Route;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};

/// Backend echoing method, path and selected request headers as JSON.
async fn spawn_backend() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let svc = service_fn(|req: Request<hyper::body::Incoming>| async move {
                    let info = serde_json::json!({
                        "method": req.method().as_str(),
                        "path": req.uri().path(),
                        "host": req.headers().get("host")
                            .and_then(|v| v.to_str().ok()).unwrap_or(""),
                        "xfh": req.headers().get("x-forwarded-host")
                            .and_then(|v| v.to_str().ok()).unwrap_or(""),
                        "xff": req.headers().get("x-forwarded-for")
                            .and_then(|v| v.to_str().ok()).unwrap_or(""),
                        "xfp": req.headers().get("x-forwarded-proto")
                            .and_then(|v| v.to_str().ok()).unwrap_or(""),
                        "hops": req.headers().get("x-portproxy-hops")
                            .and_then(|v| v.to_str().ok()).unwrap_or(""),
                    });
                    Ok::<_, hyper::Error>(Response::new(Full::new(Bytes::from(info.to_string()))))
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    addr
}

struct TestProxy {
    addr: SocketAddr,
    _dir: tempfile::TempDir,
    handle: tokio::task::JoinHandle<()>,
}

async fn spawn_proxy(routes: Vec<Route>, grace: Duration, idle: Duration) -> TestProxy {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("routes.json"),
        serde_json::to_string(&routes).unwrap(),
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let store = RouteStore::new(dir.path().to_path_buf());
    let opts = ProxyOptions {
        grace,
        idle_delay: idle,
    };
    let handle = tokio::spawn(async move {
        run_proxy(store, listener, opts).await.unwrap();
    });
    // wait until accepting
    for _ in 0..50 {
        if TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    TestProxy {
        addr,
        _dir: dir,
        handle,
    }
}

async fn request(
    addr: SocketAddr,
    host: &str,
    extra: &[(&str, &str)],
) -> (StatusCode, hyper::HeaderMap, String) {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    let mut builder = Request::builder().uri("/hello").header("host", host);
    for (k, v) in extra {
        builder = builder.header(*k, *v);
    }
    let req = builder.body(Empty::<Bytes>::new()).unwrap();
    let resp = sender.send_request(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8_lossy(&body).to_string())
}

fn route(label: &str, port: u16) -> Route {
    Route {
        hostname: label.into(),
        port,
        pid: 0,
    }
}

#[tokio::test]
async fn routes_by_first_label_and_sets_forwarded_headers() {
    let backend = spawn_backend().await;
    let p = spawn_proxy(
        vec![route("app", backend.port())],
        Duration::from_secs(60),
        Duration::from_secs(60),
    )
    .await;

    let (status, headers, body) = request(p.addr, "app.dev.example.test", &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get("x-portproxy").unwrap(), "1");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["path"], "/hello");
    assert_eq!(v["host"], format!("localhost:{}", backend.port()));
    assert_eq!(v["xfh"], "app.dev.example.test");
    assert_eq!(v["xfp"], "http");
    assert_eq!(v["hops"], "1");
    assert!(!v["xff"].as_str().unwrap().is_empty());

    // preserves Caddy-set forwarded headers, appends to XFF
    let (_, _, body) = request(
        p.addr,
        "app.dev.example.test",
        &[
            ("x-forwarded-proto", "https"),
            ("x-forwarded-for", "1.2.3.4"),
        ],
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["xfp"], "https");
    assert!(v["xff"].as_str().unwrap().starts_with("1.2.3.4, "));

    p.handle.abort();
}

#[tokio::test]
async fn unknown_label_404_lists_routes() {
    let backend = spawn_backend().await;
    let p = spawn_proxy(
        vec![route("app", backend.port())],
        Duration::from_secs(60),
        Duration::from_secs(60),
    )
    .await;
    let (status, headers, body) = request(
        p.addr,
        "nope.dev.example.test",
        &[("x-forwarded-proto", "https")],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(headers.get("x-portproxy").unwrap(), "1");
    assert!(body.contains("nope"));
    // clickable link: first label swapped, upstream domain + scheme preserved
    assert!(
        body.contains("href=\"https://app.dev.example.test\""),
        "{body}"
    );
    p.handle.abort();
}

#[tokio::test]
async fn hop_limit_508() {
    let backend = spawn_backend().await;
    let p = spawn_proxy(
        vec![route("app", backend.port())],
        Duration::from_secs(60),
        Duration::from_secs(60),
    )
    .await;
    let (status, _, _) = request(p.addr, "app.x", &[("x-portproxy-hops", "5")]).await;
    assert_eq!(status, StatusCode::LOOP_DETECTED);
    p.handle.abort();
}

#[tokio::test]
async fn dead_backend_502() {
    // port with nothing listening
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_port = probe.local_addr().unwrap().port();
    drop(probe);
    let p = spawn_proxy(
        vec![route("app", dead_port)],
        Duration::from_secs(60),
        Duration::from_secs(60),
    )
    .await;
    let (status, _, _) = request(p.addr, "app.x", &[]).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    p.handle.abort();
}

#[tokio::test]
async fn idle_exit_after_grace() {
    let p = spawn_proxy(
        vec![],
        Duration::from_millis(200),
        Duration::from_millis(300),
    )
    .await;
    let done = tokio::time::timeout(Duration::from_secs(3), p.handle).await;
    assert!(done.is_ok(), "proxy did not exit while idle");
}

#[tokio::test]
async fn websocket_echo_through_proxy() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // raw-TCP WS backend: accept upgrade, then echo bytes verbatim
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        let mut head = Vec::new();
        loop {
            let n = s.read(&mut buf).await.unwrap();
            head.extend_from_slice(&buf[..n]);
            if head.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        s.write_all(
            b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
              Connection: Upgrade\r\nSec-WebSocket-Accept: dGVzdA==\r\n\r\n",
        )
        .await
        .unwrap();
        // echo loop
        loop {
            let n = match s.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if s.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }
    });

    let p = spawn_proxy(
        vec![route("ws", ws_port)],
        Duration::from_secs(60),
        Duration::from_secs(60),
    )
    .await;

    let mut c = TcpStream::connect(p.addr).await.unwrap();
    c.write_all(
        b"GET /socket HTTP/1.1\r\nHost: ws.x.test\r\nConnection: Upgrade\r\n\
          Upgrade: websocket\r\nSec-WebSocket-Key: dGVzdA==\r\n\
          Sec-WebSocket-Version: 13\r\n\r\n",
    )
    .await
    .unwrap();
    // read 101 response head
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        c.read_exact(&mut b).await.unwrap();
        head.push(b[0]);
        assert!(head.len() < 8192);
    }
    let head = String::from_utf8_lossy(&head);
    assert!(head.starts_with("HTTP/1.1 101"), "got: {head}");
    assert!(head.to_lowercase().contains("sec-websocket-accept"));

    // tunnel echoes raw bytes
    c.write_all(b"frame-payload").await.unwrap();
    let mut echo = [0u8; 13];
    tokio::time::timeout(Duration::from_secs(2), c.read_exact(&mut echo))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&echo, b"frame-payload");
    p.handle.abort();
}
