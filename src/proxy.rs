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
    /// Startup window during which idle shutdown is suppressed.
    pub grace: Duration,
    /// Continuous zero-route time after which the proxy exits.
    pub idle_delay: Duration,
}

impl Default for ProxyOptions {
    fn default() -> Self {
        Self {
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
/// Takes a pre-bound listener so callers only persist state (pid files) after
/// the bind has succeeded — a second racing proxy must fail before writing
/// anything it would clean up on exit.
pub async fn run_proxy(store: RouteStore, listener: TcpListener, opts: ProxyOptions) -> Result<()> {
    let routes: RouteMap = Default::default();
    let (tx, rx) = watch::channel(false);

    {
        let routes = routes.clone();
        tokio::spawn(async move {
            loop {
                let live = store.load();
                let _ = tx.send(!live.is_empty());
                *routes.write().await = live.into_iter().map(|r| (r.hostname.clone(), r)).collect();
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
        let mut active: Vec<Route> = routes.read().await.values().cloned().collect();
        active.sort_by(|a, b| a.hostname.cmp(&b.hostname));
        let scheme = req
            .headers()
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("http")
            .to_string();
        return Ok(stamp(not_found(&label, &host, &scheme, &active)));
    };

    let is_upgrade = req
        .headers()
        .get(hyper::header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_lowercase().contains("upgrade"))
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

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// URL for a route, derived from the requested host: swap its first label.
/// `nope.dev.example.test` + route `app` -> `<scheme>://app.dev.example.test`.
/// Works with any upstream domain — no configuration needed.
fn sibling_url(scheme: &str, requested_host: &str, label: &str) -> String {
    let host_no_port = requested_host.split(':').next().unwrap_or(requested_host);
    match host_no_port.split_once('.') {
        Some((_, rest)) => format!("{scheme}://{label}.{rest}"),
        None => format!("{scheme}://{label}"),
    }
}

fn not_found(label: &str, host: &str, scheme: &str, active: &[Route]) -> Response<Body> {
    let label = html_escape(label);
    let list: String = if active.is_empty() {
        "<p class=empty><em>No apps running.</em></p>".to_string()
    } else {
        let items: String = active
            .iter()
            .map(|r| {
                let name = html_escape(&r.hostname);
                let url = html_escape(&sibling_url(scheme, host, &r.hostname));
                format!(
                    "<li><a href=\"{url}\">{name}</a> <span class=port>127.0.0.1:{}</span></li>",
                    r.port
                )
            })
            .collect();
        format!("<ul>{items}</ul>")
    };
    let html = format!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>portproxy &mdash; not found</title>
<style>
  body {{ font-family: system-ui, sans-serif; max-width: 40rem; margin: 4rem auto;
         padding: 0 1rem; line-height: 1.6; color: #1a1a1a; background: #fff; }}
  h1 {{ font-size: 1.4rem; }}
  code {{ background: rgba(127,127,127,.15); padding: .15em .4em; border-radius: 4px; }}
  ul {{ padding-left: 1.2rem; }}
  li {{ margin: .3rem 0; }}
  a {{ color: #0070f3; text-decoration: none; }}
  a:hover {{ text-decoration: underline; }}
  .port {{ color: #888; font-size: .85em; margin-left: .5em; }}
  .hint {{ color: #666; margin-top: 2rem; font-size: .9em; }}
  @media (prefers-color-scheme: dark) {{
    body {{ color: #ededed; background: #111; }}
    a {{ color: #52a8ff; }}
  }}
</style>
</head>
<body>
<h1>No app named <code>{label}</code></h1>
<p>Active apps:</p>
{list}
<p class=hint>Start one with: <code>portproxy {label} your-command</code>
or <code>portproxy run your-command</code></p>
</body>
</html>"#
    );
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header("content-type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(html)).map_err(|e| match e {}).boxed())
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sibling_url_swaps_first_label() {
        assert_eq!(
            sibling_url("https", "nope.dev.example.test", "app"),
            "https://app.dev.example.test"
        );
        assert_eq!(
            sibling_url("http", "nope.localhost:1355", "app"),
            "http://app.localhost"
        );
        assert_eq!(sibling_url("http", "bare", "app"), "http://app");
    }

    #[test]
    fn html_escape_neutralizes_markup() {
        assert_eq!(
            html_escape(r#"<img src=x onerror="x">&"#),
            "&lt;img src=x onerror=&quot;x&quot;&gt;&amp;"
        );
    }
}
