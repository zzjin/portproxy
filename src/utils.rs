use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// Dual-stack loopback by default: `.localhost` resolves to `::1` per
/// RFC 6761, while Caddy/probes typically connect via `127.0.0.1`.
pub const DEFAULT_LISTEN: &[&str] = &["127.0.0.1:1355", "[::1]:1355"];
pub const MIN_APP_PORT: u16 = 4000;
pub const MAX_APP_PORT: u16 = 4999;
pub const MAX_HOPS: u32 = 5;

pub fn state_dir() -> PathBuf {
    if let Ok(d) = std::env::var("PORTPROXY_STATE_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    dirs::home_dir()
        .expect("cannot determine home directory")
        .join(".portproxy")
}

/// Sanitize an arbitrary string into a DNS label: lowercase, non-alphanumeric
/// runs collapse to a single hyphen, trimmed, capped at 63 chars with a 6-char
/// content hash suffix on truncation so distinct long names stay distinct.
pub fn sanitize_label(input: &str) -> String {
    let mut out = String::new();
    let mut prev_hyphen = true; // trims leading hyphens
    for c in input.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_hyphen = false;
        } else if !prev_hyphen {
            out.push('-');
            prev_hyphen = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.len() > 63 {
        let hash = format!("{:x}", Sha256::digest(input.as_bytes()));
        let mut head = out[..56].to_string();
        while head.ends_with('-') {
            head.pop();
        }
        out = format!("{head}-{}", &hash[..6]);
    }
    out
}

/// First DNS label of a Host header value (port stripped, lowercased).
pub fn host_label(host: &str) -> Option<String> {
    let host = host.trim();
    if host.is_empty() || host.starts_with('[') {
        return None;
    }
    let host = host.split(':').next()?;
    let label = host.split('.').next()?.trim().to_lowercase();
    if label.is_empty() {
        None
    } else {
        Some(label)
    }
}

/// Probe `listen` for a live portproxy (any response carrying `x-portproxy: 1`).
pub fn is_proxy_running(listen: &str) -> bool {
    use std::io::{Read, Write};
    use std::time::Duration;
    let Ok(addr) = listen.parse::<std::net::SocketAddr>() else {
        return false;
    };
    let Ok(mut s) = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_millis(1000)));
    if s.write_all(b"HEAD / HTTP/1.0\r\nHost: portproxy-probe\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut buf = String::new();
    let _ = s.read_to_string(&mut buf);
    buf.to_lowercase().contains("x-portproxy: 1")
}

/// True when any of the listen addresses answers as a live portproxy.
pub fn is_any_proxy_running(addrs: &[String]) -> bool {
    addrs.iter().any(|a| is_proxy_running(a))
}

pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_basic() {
        assert_eq!(sanitize_label("My App!"), "my-app");
        assert_eq!(sanitize_label("@scope/pkg"), "scope-pkg");
        assert_eq!(sanitize_label("--a--b--"), "a-b");
    }

    #[test]
    fn sanitize_truncates_63_with_hash() {
        let long = "a".repeat(80);
        let s = sanitize_label(&long);
        assert!(s.len() <= 63);
        assert!(s.contains('-'));
        // distinct long inputs stay distinct
        let other = format!("{}b", "a".repeat(80));
        assert_ne!(s, sanitize_label(&other));
    }

    #[test]
    fn host_label_extracts_first() {
        assert_eq!(
            host_label("sample-web.dev.example.test").as_deref(),
            Some("sample-web")
        );
        assert_eq!(host_label("App.localhost:1355").as_deref(), Some("app"));
        assert_eq!(host_label("single"), Some("single".into()));
        assert_eq!(host_label(""), None);
        assert_eq!(host_label("[::1]:80"), None);
    }

    #[test]
    fn state_dir_env_override() {
        std::env::set_var("PORTPROXY_STATE_DIR", "/tmp/pp-test");
        assert_eq!(state_dir(), PathBuf::from("/tmp/pp-test"));
        std::env::remove_var("PORTPROXY_STATE_DIR");
    }
}
