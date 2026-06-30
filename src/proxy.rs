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
/// Takes pre-bound listeners (typically dual-stack loopback — `.localhost`
/// resolves to `::1` while Caddy connects via `127.0.0.1`) so callers only
/// persist state (pid files) after every bind has succeeded — a second racing
/// proxy must fail before writing anything it would clean up on exit.
pub async fn run_proxy(
    store: RouteStore,
    listeners: Vec<TcpListener>,
    opts: ProxyOptions,
) -> Result<()> {
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

    let accept_all = async {
        let mut set = tokio::task::JoinSet::new();
        for listener in listeners {
            let routes = routes.clone();
            let client = client.clone();
            set.spawn(async move {
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
            });
        }
        // any accept loop terminating is fatal
        match set.join_next().await {
            Some(Ok(r)) => r,
            Some(Err(e)) => Err(anyhow::anyhow!(e)),
            None => anyhow::Ok(()),
        }
    };

    tokio::select! {
        r = accept_all => r,
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

/// URL for a route, derived from the requested host: swap its first label,
/// keeping any explicit port (Caddy may front a non-standard port).
/// `nope.dev.example.test:54699` + route `app` -> `<scheme>://app.dev.example.test:54699`.
/// Works with any upstream domain — no configuration needed.
fn sibling_url(scheme: &str, requested_host: &str, label: &str) -> String {
    let (host, port) = match requested_host.split_once(':') {
        Some((h, p)) => (h, format!(":{p}")),
        None => (requested_host, String::new()),
    };
    match host.split_once('.') {
        Some((_, rest)) => format!("{scheme}://{label}.{rest}{port}"),
        None => format!("{scheme}://{label}{port}"),
    }
}

/// The `-…` extension `s` adds over `prefix`, if any.
fn strip_dash_prefix<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    s.strip_prefix(prefix)?.strip_prefix('-')
}

/// Split active routes into (suggested, same_worktree, others).
/// Suggested — prefix-related to the requested label. Forward: a running
/// route extends the label with `-…` (a worktree variant of what was asked
/// for). Reverse: the label extends a running route (stale worktree
/// hostname; only the base runs).
/// Same-worktree — siblings of a suggestion: the part the longer name adds
/// over the shorter is the worktree suffix (`demo-web` + variant
/// `demo-web-billing` -> `billing`), and any other route ending in
/// `-billing` is the same worktree's service for another package.
fn related_routes<'a>(
    label: &str,
    active: &'a [Route],
) -> (Vec<&'a Route>, Vec<&'a Route>, Vec<&'a Route>) {
    let mut suggested = Vec::new();
    let mut rest = Vec::new();
    let mut suffixes = Vec::new();
    for r in active {
        match strip_dash_prefix(&r.hostname, label)
            .or_else(|| strip_dash_prefix(label, &r.hostname))
        {
            Some(sfx) => {
                suggested.push(r);
                suffixes.push(sfx);
            }
            None => rest.push(r),
        }
    }
    let (same_worktree, others) = rest.into_iter().partition(|r: &&Route| {
        suffixes.iter().any(|sfx| {
            r.hostname
                .strip_suffix(sfx)
                .is_some_and(|p| p.ends_with('-'))
        })
    });
    (suggested, same_worktree, others)
}

const ARROW_SVG: &str = r#"<svg width="16" height="16" viewBox="0 0 16 16" fill="none"><path d="M6.5 3.5L11 8l-4.5 4.5" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/></svg>"#;

fn route_items(scheme: &str, host: &str, routes: &[&Route]) -> String {
    routes
        .iter()
        .map(|r| {
            let name = html_escape(&r.hostname);
            let url = html_escape(&sibling_url(scheme, host, &r.hostname));
            format!(
                "<li><a href=\"{url}\" class=\"card-link\"><span class=\"name\">{name}</span><span class=\"meta\"><code class=\"port\">127.0.0.1:{}</code><span class=\"arrow\">{ARROW_SVG}</span></span></a></li>",
                r.port
            )
        })
        .collect()
}

fn section(label: &str, scheme: &str, host: &str, routes: &[&Route]) -> String {
    format!(
        "<div class=\"section\"><p class=\"label\">{label}</p><ul class=\"card\">{}</ul></div>",
        route_items(scheme, host, routes)
    )
}

fn not_found_html(label: &str, host: &str, scheme: &str, active: &[Route]) -> String {
    let list: String = if active.is_empty() {
        "<p class=\"empty\">No apps running.</p>".to_string()
    } else {
        let (suggested, same_wt, others) = related_routes(label, active);
        if suggested.is_empty() {
            section("Active apps", scheme, host, &others)
        } else {
            let mut s = section("Did you mean", scheme, host, &suggested);
            if !same_wt.is_empty() {
                s.push_str(&section("Same worktree", scheme, host, &same_wt));
            }
            if !others.is_empty() {
                s.push_str(&section("Other running apps", scheme, host, &others));
            }
            s
        }
    };
    let label = html_escape(label);
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="light dark">
<title>404 &mdash; portproxy</title>
<style>
  *, *::before, *::after {{ margin: 0; padding: 0; box-sizing: border-box; }}
  :root {{
    --bg: #fff;
    --fg: #171717;
    --border: #eaeaea;
    --surface: #fafafa;
    --text-2: #666;
    --text-3: #a1a1a1;
    --accent: #0070f3;
    --font-sans: system-ui, -apple-system, 'Segoe UI', Roboto, sans-serif;
    --font-mono: ui-monospace, 'SFMono-Regular', Menlo, Consolas, monospace;
  }}
  @media (prefers-color-scheme: dark) {{
    :root {{
      --bg: #000;
      --fg: #ededed;
      --border: rgba(255,255,255,0.12);
      --surface: #111;
      --text-2: #888;
      --text-3: #666;
      --accent: #3291ff;
    }}
  }}
  html {{ height: 100%; }}
  body {{
    font-family: var(--font-sans);
    background: var(--bg);
    color: var(--fg);
    min-height: 100%;
    -webkit-font-smoothing: antialiased;
  }}
  .page {{
    min-height: 100vh;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    padding: 32px 24px;
  }}
  .hero {{
    display: flex;
    flex-direction: column;
    align-items: center;
  }}
  .hero h1 {{
    font-family: var(--font-mono);
    font-size: clamp(72px, 14vw, 128px);
    font-weight: 600;
    line-height: 1;
    letter-spacing: -0.04em;
  }}
  .hero h2 {{
    font-size: 13px;
    font-weight: 400;
    color: var(--text-3);
    margin-top: 16px;
    text-transform: uppercase;
    letter-spacing: 0.15em;
  }}
  .content {{
    margin-top: 48px;
    width: 100%;
    max-width: 480px;
  }}
  .desc {{
    font-size: 14px;
    color: var(--text-2);
    text-align: center;
    line-height: 1.7;
  }}
  .desc code {{
    font-family: var(--font-mono);
    font-size: 13px;
    color: var(--fg);
    font-weight: 500;
  }}
  .section {{ margin-top: 32px; }}
  .label {{
    font-size: 12px;
    font-weight: 500;
    color: var(--text-3);
    text-transform: uppercase;
    letter-spacing: 0.1em;
    margin-bottom: 10px;
  }}
  .card {{
    list-style: none;
    border: 1px solid var(--border);
    border-radius: 12px;
    overflow: hidden;
  }}
  .card > li {{ border-bottom: 1px solid var(--border); }}
  .card > li:last-child {{ border-bottom: none; }}
  .card-link {{
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 13px 16px;
    text-decoration: none;
    color: inherit;
    transition: background 0.15s ease;
  }}
  .card-link:hover {{ background: var(--surface); }}
  .card-link .name {{
    font-size: 14px;
    font-weight: 500;
    transition: color 0.15s ease;
    overflow-wrap: anywhere;
  }}
  .card-link:hover .name {{ color: var(--accent); }}
  .card-link .meta {{
    display: flex;
    align-items: center;
    gap: 10px;
    flex-shrink: 0;
    margin-left: 16px;
  }}
  .card-link .port {{
    font-family: var(--font-mono);
    font-size: 13px;
    color: var(--text-3);
  }}
  .card-link .arrow {{
    color: var(--text-3);
    display: flex;
    transition: transform 0.2s ease, color 0.2s ease;
  }}
  .card-link:hover .arrow {{
    transform: translateX(2px);
    color: var(--text-2);
  }}
  .terminal {{
    font-family: var(--font-mono);
    font-size: 13px;
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 12px;
    padding: 14px 20px;
    line-height: 1.8;
    color: var(--fg);
    overflow-x: auto;
    white-space: nowrap;
  }}
  .terminal .prompt {{
    color: var(--text-3);
    user-select: none;
  }}
  .empty {{
    font-size: 14px;
    color: var(--text-3);
    text-align: center;
    padding: 32px 0;
  }}
  .footer {{
    margin-top: 64px;
    font-size: 11px;
    color: var(--text-3);
    font-family: var(--font-mono);
    letter-spacing: 0.08em;
  }}
  .footer a {{
    color: inherit;
    text-decoration: none;
    transition: color 0.15s ease;
  }}
  .footer a:hover {{ color: var(--accent); }}
</style>
</head>
<body>
<div class="page">
<div class="hero"><h1>404</h1><h2>Not Found</h2></div>
<div class="content">
<p class="desc">No app named <code>{label}</code></p>
{list}
<div class="section">
<p class="label">Start one with</p>
<div class="terminal"><span class="prompt">$ </span>portproxy {label} your-command<br><span class="prompt">$ </span>portproxy run your-command</div>
</div>
</div>
<p class="footer"><a href="https://github.com/zzjin/portproxy" rel="noreferrer">portproxy</a></p>
</div>
</body>
</html>"#
    )
}

fn not_found(label: &str, host: &str, scheme: &str, active: &[Route]) -> Response<Body> {
    let html = not_found_html(label, host, scheme, active);
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
            sibling_url("https", "nope.dev.example.test:54699", "app"),
            "https://app.dev.example.test:54699"
        );
        assert_eq!(
            sibling_url("http", "nope.localhost:1355", "app"),
            "http://app.localhost:1355"
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

    fn route(hostname: &str) -> Route {
        Route {
            hostname: hostname.into(),
            port: 4000,
            pid: 1,
        }
    }

    fn names<'a>(routes: &[&'a Route]) -> Vec<&'a str> {
        routes.iter().map(|r| r.hostname.as_str()).collect()
    }

    #[test]
    fn related_routes_finds_worktree_variants() {
        let active = vec![
            route("demo-web-billing"),
            route("demo-webby"),
            route("other-api"),
        ];
        let (suggested, same_wt, others) = related_routes("demo-web", &active);
        assert_eq!(names(&suggested), ["demo-web-billing"]);
        assert!(same_wt.is_empty());
        assert_eq!(names(&others), ["demo-webby", "other-api"]);
    }

    #[test]
    fn related_routes_groups_same_worktree_siblings() {
        let active = vec![
            route("demo-web-billing"),
            route("demo-api-billing"),
            route("demo-admin-billing"),
            route("other-api-audit"),
        ];
        let (suggested, same_wt, others) = related_routes("demo-web", &active);
        assert_eq!(names(&suggested), ["demo-web-billing"]);
        assert_eq!(
            names(&same_wt),
            [
                "demo-api-billing",
                "demo-admin-billing"
            ]
        );
        assert_eq!(names(&others), ["other-api-audit"]);
    }

    #[test]
    fn related_routes_finds_base_for_stale_worktree_host() {
        let active = vec![route("demo-web"), route("other-api")];
        let (suggested, same_wt, others) = related_routes("demo-web-old-branch", &active);
        assert_eq!(names(&suggested), ["demo-web"]);
        assert!(same_wt.is_empty());
        assert_eq!(names(&others), ["other-api"]);
    }

    #[test]
    fn related_routes_empty_for_unrelated_label() {
        let active = vec![route("demo-web"), route("other-api")];
        let (suggested, same_wt, others) = related_routes("zzz", &active);
        assert!(suggested.is_empty());
        assert!(same_wt.is_empty());
        assert_eq!(names(&others), ["demo-web", "other-api"]);
    }

    #[test]
    fn not_found_html_sections_related_routes() {
        let active = vec![
            route("demo-web-billing"),
            route("demo-api-billing"),
            route("other-api"),
        ];
        let html = not_found_html("demo-web", "demo-web.localhost:1355", "http", &active);
        assert!(html.contains("Did you mean"));
        assert!(html.contains("http://demo-web-billing.localhost:1355"));
        assert!(html.contains("Same worktree"));
        assert!(html.contains("http://demo-api-billing.localhost:1355"));
        assert!(html.contains("Other running apps"));
        assert!(html.contains("other-api"));
    }

    #[test]
    fn not_found_html_plain_list_when_nothing_related() {
        let html = not_found_html("zzz", "zzz.localhost:1355", "http", &[route("demo-web")]);
        assert!(html.contains("Active apps"));
        assert!(html.contains("demo-web"));
        assert!(!html.contains("Did you mean"));
    }
}
