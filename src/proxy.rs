use crate::routes::RouteStore;
use crate::types::Route;
use crate::utils::{host_label, MAX_HOPS};
use anyhow::Result;
use http_body_util::{combinators::BoxBody, BodyExt, Empty, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response, StatusCode};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::watch;

pub struct ProxyOptions {
    pub listen: SocketAddr,
    /// Startup window during which idle shutdown is suppressed.
    pub grace: Duration,
    /// Continuous zero-route time after which the proxy exits.
    pub idle_delay: Duration,
}

impl ProxyOptions {
    pub fn new(listen: SocketAddr) -> Self {
        Self {
            listen,
            grace: Duration::from_secs(10),
            idle_delay: Duration::from_secs(5),
        }
    }
}

type RouteMap = Arc<tokio::sync::RwLock<HashMap<String, Route>>>;
type Body = BoxBody<Bytes, hyper::Error>;
type HttpClient = Client<HttpConnector, Incoming>;

/// Run the proxy until it goes idle (no routes for `idle_delay` after `grace`).
/// routes.json is the IPC: reloaded every 100 ms, dead-PID entries dropped.
pub async fn run_proxy(store: RouteStore, opts: ProxyOptions) -> Result<()> {
    let listener = TcpListener::bind(opts.listen).await?;
    let routes: RouteMap = Default::default();
    let (tx, rx) = watch::channel(false);

    {
        let routes = routes.clone();
        tokio::spawn(async move {
            loop {
                let live = store.load();
                let _ = tx.send(!live.is_empty());
                *routes.write().await = live
                    .into_iter()
                    .map(|r| (r.hostname.clone(), r))
                    .collect();
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
    }

    let client: HttpClient = Client::builder(TokioExecutor::new()).build_http();

    let accept_loop = async {
        loop {
            let (stream, peer) = listener.accept().await?;
            let routes = routes.clone();
            let client = client.clone();
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(move |req| {
                    handle(req, routes.clone(), client.clone(), peer)
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .with_upgrades()
                    .await;
            });
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    };

    tokio::select! {
        r = accept_loop => r,
        _ = idle_watch(rx, opts.grace, opts.idle_delay) => Ok(()),
    }
}

async fn idle_watch(rx: watch::Receiver<bool>, grace: Duration, idle: Duration) {
    tokio::time::sleep(grace).await;
    loop {
        if *rx.borrow() {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }
        let deadline = tokio::time::Instant::now() + idle;
        let mut still_idle = true;
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if *rx.borrow() {
                still_idle = false;
                break;
            }
        }
        if still_idle {
            return;
        }
    }
}

async fn handle(
    mut req: Request<Incoming>,
    routes: RouteMap,
    client: HttpClient,
    peer: SocketAddr,
) -> Result<Response<Body>, hyper::Error> {
    let hops = req
        .headers()
        .get("x-portproxy-hops")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0);
    if hops >= MAX_HOPS {
        return Ok(stamp(text(StatusCode::LOOP_DETECTED, "loop detected")));
    }

    let host = req
        .headers()
        .get(hyper::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let Some(label) = host_label(&host) else {
        return Ok(stamp(text(StatusCode::BAD_REQUEST, "missing Host header")));
    };
    let route = routes.read().await.get(&label).cloned();
    let Some(route) = route else {
        let mut names: Vec<String> = routes.read().await.keys().cloned().collect();
        names.sort();
        return Ok(stamp(not_found(&label, &names)));
    };

    let is_upgrade = req
        .headers()
        .get(hyper::header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .map_or(false, |v| v.to_lowercase().contains("upgrade"))
        && req.headers().contains_key(hyper::header::UPGRADE);
    if is_upgrade {
        return Ok(stamp(websocket_tunnel(req, route.port, &host).await));
    }

    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".into());
    let uri: hyper::Uri = format!("http://127.0.0.1:{}{}", route.port, path)
        .parse()
        .expect("valid backend uri");
    *req.uri_mut() = uri;
    set_forward_headers(req.headers_mut(), &host, peer, route.port);

    match client.request(req).await {
        Ok(resp) => Ok(stamp(resp.map(|b| b.boxed()))),
        Err(_) => Ok(stamp(text(
            StatusCode::BAD_GATEWAY,
            &format!(
                "backend for \"{label}\" (port {}) is not responding",
                route.port
            ),
        ))),
    }
}

fn set_forward_headers(h: &mut hyper::HeaderMap, host: &str, peer: SocketAddr, port: u16) {
    use hyper::header::{HeaderValue, HOST};
    let hops = h
        .get("x-portproxy-hops")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0);
    h.insert(
        "x-portproxy-hops",
        HeaderValue::from_str(&(hops + 1).to_string()).unwrap(),
    );
    // backend sees localhost so Vite-style host checks pass
    h.insert(
        HOST,
        HeaderValue::from_str(&format!("localhost:{port}")).unwrap(),
    );
    // append, preserving values already set by Caddy/Nginx
    let xff = match h.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        Some(prev) => format!("{prev}, {}", peer.ip()),
        None => peer.ip().to_string(),
    };
    if let Ok(v) = HeaderValue::from_str(&xff) {
        h.insert("x-forwarded-for", v);
    }
    if !h.contains_key("x-forwarded-proto") {
        h.insert("x-forwarded-proto", HeaderValue::from_static("http"));
    }
    if !h.contains_key("x-forwarded-host") {
        if let Ok(v) = HeaderValue::from_str(host) {
            h.insert("x-forwarded-host", v);
        }
    }
}

async fn websocket_tunnel(req: Request<Incoming>, port: u16, host: &str) -> Response<Body> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut backend = match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
        Ok(s) => s,
        Err(_) => match tokio::net::TcpStream::connect(("::1", port)).await {
            Ok(s) => s,
            Err(_) => return text(StatusCode::BAD_GATEWAY, "backend unreachable"),
        },
    };

    // raw handshake to the backend
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/")
        .to_string();
    let mut raw = format!("{} {} HTTP/1.1\r\n", req.method(), path);
    raw.push_str(&format!("Host: localhost:{port}\r\n"));
    for (k, v) in req.headers() {
        if k == hyper::header::HOST {
            continue;
        }
        if let Ok(v) = v.to_str() {
            raw.push_str(&format!("{k}: {v}\r\n"));
        }
    }
    raw.push_str(&format!("X-Forwarded-Host: {host}\r\n\r\n"));
    if backend.write_all(raw.as_bytes()).await.is_err() {
        return text(StatusCode::BAD_GATEWAY, "backend handshake write failed");
    }

    // read response headers byte-by-byte: must not overshoot into WS frames
    let mut head = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 16 * 1024 || backend.read_exact(&mut byte).await.is_err() {
            return text(StatusCode::BAD_GATEWAY, "backend handshake read failed");
        }
        head.push(byte[0]);
    }
    let head_str = String::from_utf8_lossy(&head).to_string();
    let mut lines = head_str.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(502);
    let mut builder = Response::builder().status(status);
    for line in lines {
        if let Some((k, v)) = line.split_once(": ") {
            builder = builder.header(k, v);
        }
    }
    let resp = match builder.body(Empty::<Bytes>::new().map_err(|e| match e {}).boxed()) {
        Ok(r) => r,
        Err(_) => return text(StatusCode::BAD_GATEWAY, "bad backend handshake"),
    };

    if status == 101 {
        tokio::spawn(async move {
            if let Ok(upgraded) = hyper::upgrade::on(req).await {
                let mut client_io = TokioIo::new(upgraded);
                let _ = tokio::io::copy_bidirectional(&mut client_io, &mut backend).await;
            }
        });
    }
    resp
}

fn stamp(mut resp: Response<Body>) -> Response<Body> {
    resp.headers_mut()
        .insert("x-portproxy", hyper::header::HeaderValue::from_static("1"));
    resp
}

fn text(status: StatusCode, msg: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(
            Full::new(Bytes::from(msg.to_string()))
                .map_err(|e| match e {})
                .boxed(),
        )
        .unwrap()
}

fn not_found(label: &str, names: &[String]) -> Response<Body> {
    let list: String = if names.is_empty() {
        "<li><em>none</em></li>".to_string()
    } else {
        names
            .iter()
            .map(|n| format!("<li><code>{n}</code></li>"))
            .collect()
    };
    let html = format!(
        "<!doctype html><meta charset=utf-8><title>portproxy: not found</title>\
         <body style=\"font-family:system-ui;max-width:40rem;margin:4rem auto\">\
         <h1>No app named <code>{label}</code></h1>\
         <p>Active routes:</p><ul>{list}</ul></body>"
    );
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header("content-type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(html)).map_err(|e| match e {}).boxed())
        .unwrap()
}
