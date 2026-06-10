# portproxy Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rust CLI giving dev servers stable named URLs behind Caddy/Nginx: spawn-on-demand Host-routing reverse proxy with idle self-exit, auto name/worktree discovery, file-based route registry.

**Architecture:** Single binary, two modes. Wrapper mode registers a route in `~/.portproxy/routes.json` (mkdir-lock, PID liveness) and runs the dev command with `PORT` injected. Proxy mode (self-spawned, `setsid`-detached) is a hyper HTTP/1.1 reverse proxy matching the first DNS label of `Host`, reloading routes every 100 ms, exiting after 5 s of zero routes. No IPC — the JSON file is the IPC. No TLS/DNS — upstream Caddy owns that.

**Tech Stack:** tokio, hyper 1.x + hyper-util, http-body-util, clap (derive), serde/serde_json, toml, dirs, rand, sha2, nix, libc, anyhow, colored. Dev: tempfile.

**Spec:** `docs/superpowers/specs/2026-06-10-portproxy-design.md`

---

### Task 1: Cargo scaffold + types + utils (state dir, label sanitize, host parsing)

**Files:**
- Create: `Cargo.toml`, `src/main.rs` (stub), `src/types.rs`, `src/utils.rs`

- [ ] **Step 1: Scaffold**

`Cargo.toml`:

```toml
[package]
name = "portproxy"
version = "0.1.0"
edition = "2021"
license = "MIT"
description = "Stable named URLs for local dev servers behind a TLS-terminating reverse proxy"

[dependencies]
tokio = { version = "1", features = ["full"] }
hyper = { version = "1", features = ["full"] }
hyper-util = { version = "0.1", features = ["full"] }
http-body-util = "0.1"
clap = { version = "4", features = ["derive"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
dirs = "5"
rand = "0.8"
sha2 = "0.10"
nix = { version = "0.29", features = ["signal", "process"] }
libc = "0.2"
anyhow = "1"
colored = "2"

[dev-dependencies]
tempfile = "3"

[lib]
name = "portproxy"
path = "src/lib.rs"

[[bin]]
name = "portproxy"
path = "src/main.rs"
```

`src/lib.rs` re-exports modules for tests; `src/main.rs` stub `fn main() {}`.

- [ ] **Step 2: Failing unit tests in `src/utils.rs`**

```rust
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
        assert!(s.contains('-')); // hash suffix
    }

    #[test]
    fn host_label_extracts_first() {
        assert_eq!(host_label("sample-web.dev.example.test").as_deref(), Some("sample-web"));
        assert_eq!(host_label("App.localhost:1355").as_deref(), Some("app"));
        assert_eq!(host_label("").as_deref(), None);
    }

    #[test]
    fn state_dir_env_override() {
        std::env::set_var("PORTPROXY_STATE_DIR", "/tmp/pp-test");
        assert_eq!(state_dir(), std::path::PathBuf::from("/tmp/pp-test"));
        std::env::remove_var("PORTPROXY_STATE_DIR");
    }
}
```

- [ ] **Step 3: Implement `src/utils.rs` + `src/types.rs`**

```rust
// src/types.rs
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Route {
    pub hostname: String, // single DNS label, e.g. "sample-web-auth"
    pub port: u16,
    pub pid: u32, // wrapper PID; 0 = static alias
}
```

```rust
// src/utils.rs
use sha2::{Digest, Sha256};
use std::path::PathBuf;

pub const DEFAULT_LISTEN: &str = "127.0.0.1:1355";
pub const MIN_APP_PORT: u16 = 4000;
pub const MAX_APP_PORT: u16 = 4999;
pub const MAX_HOPS: u32 = 5;

pub fn state_dir() -> PathBuf {
    if let Ok(d) = std::env::var("PORTPROXY_STATE_DIR") {
        if !d.is_empty() { return PathBuf::from(d); }
    }
    dirs::home_dir().expect("cannot determine home directory").join(".portproxy")
}

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
    while out.ends_with('-') { out.pop(); }
    if out.len() > 63 {
        let hash = format!("{:x}", Sha256::digest(input.as_bytes()));
        let mut head = out[..56].to_string();
        while head.ends_with('-') { head.pop(); }
        out = format!("{head}-{}", &hash[..6]);
    }
    out
}

/// First DNS label of a Host header value (port stripped, lowercased).
pub fn host_label(host: &str) -> Option<String> {
    let host = host.trim();
    if host.is_empty() || host.starts_with('[') { return None; }
    let host = host.split(':').next()?;
    let label = host.split('.').next()?.trim().to_lowercase();
    if label.is_empty() { None } else { Some(label) }
}

/// Probe `listen` for a live portproxy (any response with `x-portproxy: 1`).
pub fn is_proxy_running(listen: &str) -> bool {
    use std::io::{Read, Write};
    use std::time::Duration;
    let Ok(addr) = listen.parse::<std::net::SocketAddr>() else { return false };
    let Ok(mut s) = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else { return false };
    let _ = s.set_read_timeout(Some(Duration::from_millis(1000)));
    if s.write_all(b"HEAD / HTTP/1.0\r\nHost: portproxy-probe\r\n\r\n").is_err() { return false; }
    let mut buf = String::new();
    let _ = s.read_to_string(&mut buf);
    buf.to_lowercase().contains("x-portproxy: 1")
}

pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 { return false; }
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
}
```

- [ ] **Step 4: `cargo test` → all pass. Commit `feat: scaffold + types + utils`**

---

### Task 2: RouteStore (`src/routes.rs`) — JSON + mkdir lock + liveness + conflict

**Files:** Create `src/routes.rs`, register in `src/lib.rs`.

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn store() -> (RouteStore, tempfile::TempDir) {
        let d = tempdir().unwrap();
        (RouteStore::new(d.path().to_path_buf()), d)
    }

    #[test]
    fn add_and_load() {
        let (s, _d) = store();
        s.add_route("app", 4001, std::process::id(), false).unwrap();
        let r = s.load();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].port, 4001);
    }

    #[test]
    fn dead_pid_filtered_on_load() {
        let (s, _d) = store();
        s.add_route("dead", 4002, 999_999_999, false).unwrap_err(); // invalid? no — use raw write
    }

    #[test]
    fn conflict_live_pid_errors_without_force() {
        let (s, _d) = store();
        s.add_route("app", 4001, std::process::id(), false).unwrap();
        let e = s.add_route("app", 4002, std::process::id(), false).unwrap_err();
        assert!(e.to_string().contains("already registered"));
    }

    #[test]
    fn alias_pid_zero_survives_load() {
        let (s, _d) = store();
        s.add_route("static", 8080, 0, false).unwrap();
        assert_eq!(s.load().len(), 1);
    }

    #[test]
    fn remove_route_works() {
        let (s, _d) = store();
        s.add_route("app", 4001, std::process::id(), false).unwrap();
        s.remove_route("app").unwrap();
        assert!(s.load().is_empty());
    }
}
```

(`dead_pid_filtered_on_load` writes a routes.json with a bogus PID via `load_raw`/direct file write, then asserts `load()` is empty but `load_raw()` still has it.)

- [ ] **Step 2: Implement**

```rust
// src/routes.rs
use crate::types::Route;
use crate::utils::pid_alive;
use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::time::Duration;

pub struct RouteStore { dir: PathBuf }

impl RouteStore {
    pub fn new(dir: PathBuf) -> Self { Self { dir } }
    fn routes_path(&self) -> PathBuf { self.dir.join("routes.json") }
    fn lock_path(&self) -> PathBuf { self.dir.join("routes.lock") }

    /// Routes as on disk, including dead-PID entries (used by `prune`).
    pub fn load_raw(&self) -> Vec<Route> {
        let Ok(data) = std::fs::read_to_string(self.routes_path()) else { return vec![] };
        serde_json::from_str(&data).unwrap_or_default() // corrupt file => self-heal as empty
    }

    /// Live routes: aliases (pid 0) plus entries whose wrapper PID is alive.
    pub fn load(&self) -> Vec<Route> {
        self.load_raw().into_iter()
            .filter(|r| r.pid == 0 || pid_alive(r.pid))
            .collect()
    }

    fn save(&self, routes: &[Route]) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let tmp = self.dir.join(".routes.json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(routes)?)?;
        std::fs::rename(&tmp, self.routes_path())?;
        Ok(())
    }

    fn with_lock<T>(&self, f: impl FnOnce(&mut Vec<Route>) -> Result<T>) -> Result<T> {
        std::fs::create_dir_all(&self.dir)?;
        let lock = self.lock_path();
        let mut waited = Duration::ZERO;
        loop {
            match std::fs::create_dir(&lock) {
                Ok(()) => break,
                Err(_) => {
                    // stale lock: older than 10s
                    if let Ok(meta) = std::fs::metadata(&lock) {
                        if meta.modified().ok()
                            .and_then(|m| m.elapsed().ok())
                            .map_or(false, |e| e > Duration::from_secs(10)) {
                            let _ = std::fs::remove_dir(&lock);
                            continue;
                        }
                    }
                    if waited > Duration::from_secs(5) {
                        bail!("timed out waiting for route lock at {}", lock.display());
                    }
                    std::thread::sleep(Duration::from_millis(50));
                    waited += Duration::from_millis(50);
                }
            }
        }
        let result = (|| {
            let mut routes = self.load(); // filtered: dead entries dropped and persisted below
            let out = f(&mut routes)?;
            self.save(&routes)?;
            Ok(out)
        })();
        let _ = std::fs::remove_dir(&lock);
        result
    }

    pub fn add_route(&self, hostname: &str, port: u16, pid: u32, force: bool) -> Result<()> {
        let hostname = hostname.to_string();
        self.with_lock(|routes| {
            if let Some(existing) = routes.iter().find(|r| r.hostname == hostname) {
                let live = existing.pid == 0 || pid_alive(existing.pid);
                if live && !force {
                    bail!("\"{hostname}\" is already registered by a running process (PID {}). Use --force to override.", existing.pid);
                }
                if live && existing.pid != 0 {
                    let _ = nix::sys::signal::kill(
                        nix::unistd::Pid::from_raw(existing.pid as i32),
                        nix::sys::signal::Signal::SIGTERM,
                    );
                }
                routes.retain(|r| r.hostname != hostname);
            }
            routes.push(Route { hostname: hostname.clone(), port, pid });
            Ok(())
        })
    }

    pub fn remove_route(&self, hostname: &str) -> Result<()> {
        self.with_lock(|routes| {
            routes.retain(|r| r.hostname != hostname);
            Ok(())
        })
    }

    pub fn remove_raw_entry(&self, hostname: &str) -> Result<()> {
        // like remove_route but operates on raw list (prune cleanup)
        self.with_lock(|_| Ok(())).ok(); // ensure dir
        let mut raw = self.load_raw();
        raw.retain(|r| r.hostname != hostname);
        self.save(&raw)
    }
}
```

- [ ] **Step 3: `cargo test` pass. Commit `feat: route store with lock + pid liveness + conflict`**

---

### Task 3: Port allocation + framework flag injection (`src/ports.rs`)

**Files:** Create `src/ports.rs`.

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_free_port_in_range() {
        let p = find_free_port().unwrap();
        assert!((4000..=4999).contains(&p));
    }

    #[test]
    fn vite_gets_port_strictport_host() {
        let cmd = vec!["vite".into()];
        let out = inject_framework_flags(&cmd, 4123);
        assert_eq!(out, vec!["vite", "--port", "4123", "--strictPort", "--host", "127.0.0.1"]);
    }

    #[test]
    fn npx_runner_skipped_for_detection() {
        let cmd: Vec<String> = ["npx", "astro", "dev"].map(String::from).to_vec();
        let out = inject_framework_flags(&cmd, 4001);
        assert_eq!(out, vec!["npx", "astro", "dev", "--port", "4001", "--host", "127.0.0.1"]);
    }

    #[test]
    fn unknown_tool_untouched() {
        let cmd: Vec<String> = ["next", "dev"].map(String::from).to_vec();
        assert_eq!(inject_framework_flags(&cmd, 4001), cmd); // next honors PORT env
    }
}
```

- [ ] **Step 2: Implement**

```rust
// src/ports.rs
use rand::Rng;
use crate::utils::{MIN_APP_PORT, MAX_APP_PORT};

pub fn port_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

pub fn find_free_port() -> Option<u16> {
    let mut rng = rand::thread_rng();
    for _ in 0..50 {
        let p = rng.gen_range(MIN_APP_PORT..=MAX_APP_PORT);
        if port_free(p) { return Some(p); }
    }
    (MIN_APP_PORT..=MAX_APP_PORT).find(|&p| port_free(p))
}

/// Tool basename after skipping package runners (npx, pnpx, bunx, `pnpm dlx`,
/// `yarn dlx`, `npm exec`). Returns (index_of_tool, tool_name).
fn detect_tool(cmd: &[String]) -> Option<(usize, String)> {
    let mut i = 0;
    while i < cmd.len() {
        let base = std::path::Path::new(&cmd[i])
            .file_name()?.to_string_lossy().to_string();
        match base.as_str() {
            "npx" | "pnpx" | "bunx" => { i += 1; }
            "pnpm" | "yarn" if cmd.get(i + 1).map(String::as_str) == Some("dlx") => { i += 2; }
            "npm" if cmd.get(i + 1).map(String::as_str) == Some("exec") => { i += 2; }
            _ => return Some((i, base)),
        }
    }
    None
}

/// Append --port/--host flags for tools that ignore $PORT.
pub fn inject_framework_flags(cmd: &[String], port: u16) -> Vec<String> {
    let mut out = cmd.to_vec();
    let Some((_, tool)) = detect_tool(cmd) else { return out };
    let p = port.to_string();
    match tool.as_str() {
        "vite" | "react-router" | "rsbuild" => {
            out.extend(["--port".into(), p, "--strictPort".into(), "--host".into(), "127.0.0.1".into()]);
        }
        "astro" | "ng" => {
            out.extend(["--port".into(), p, "--host".into(), "127.0.0.1".into()]);
        }
        _ => {}
    }
    out
}
```

- [ ] **Step 3: `cargo test` pass. Commit `feat: port finder + framework flag injection`**

---

### Task 4: Name inference (`src/naming.rs`) + project config (`src/config.rs`)

**Files:** Create `src/naming.rs`, `src/config.rs`.

- [ ] **Step 1: Failing tests (tempdir fixtures)**

```rust
// naming.rs tests
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn portproxy_toml_wins() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("portproxy.toml"), "name = \"custom\"").unwrap();
        std::fs::write(d.path().join("package.json"), r#"{"name":"@org/pkg"}"#).unwrap();
        assert_eq!(infer_name(d.path()), "custom");
    }

    #[test]
    fn package_json_portproxy_key_beats_name() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("package.json"),
            r#"{"name":"@org/pkg","portproxy":"override"}"#).unwrap();
        assert_eq!(infer_name(d.path()), "override");
    }

    #[test]
    fn package_json_name_strips_scope_walks_up() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("package.json"), r#"{"name":"@org/myapp"}"#).unwrap();
        let sub = d.path().join("src/deep");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(infer_name(&sub), "myapp");
    }

    #[test]
    fn cargo_toml_name() {
        let d = tempdir().unwrap();
        std::fs::write(d.path().join("Cargo.toml"), "[package]\nname = \"rusty_app\"").unwrap();
        assert_eq!(infer_name(d.path()), "rusty-app");
    }

    #[test]
    fn falls_back_to_dir_name() {
        let d = tempdir().unwrap();
        let app = d.path().join("My Cool App");
        std::fs::create_dir(&app).unwrap();
        assert_eq!(infer_name(&app), "my-cool-app");
    }
}
```

- [ ] **Step 2: Implement**

```rust
// src/config.rs
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize, Default)]
pub struct GlobalConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
    pub base_domain: Option<String>,
    #[serde(default = "default_scheme")]
    pub scheme: String,
}
fn default_listen() -> String { crate::utils::DEFAULT_LISTEN.into() }
fn default_scheme() -> String { "https".into() }

impl GlobalConfig {
    pub fn load(state_dir: &Path) -> Self {
        std::fs::read_to_string(state_dir.join("config.toml")).ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }
    pub fn url_for(&self, label: &str) -> Option<String> {
        self.base_domain.as_ref().map(|d| format!("{}://{label}.{d}", self.scheme))
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct ProjectConfig { pub name: Option<String> }

impl ProjectConfig {
    pub fn load(dir: &Path) -> Option<Self> {
        let s = std::fs::read_to_string(dir.join("portproxy.toml")).ok()?;
        toml::from_str(&s).ok()
    }
}
```

```rust
// src/naming.rs
use crate::config::ProjectConfig;
use crate::utils::sanitize_label;
use std::path::Path;

/// Inference chain (cwd upward): portproxy.toml (cwd only) -> package.json
/// "portproxy" key -> package.json "name" (scope stripped) -> Cargo.toml
/// package.name -> git main-repo root basename -> cwd basename.
pub fn infer_name(cwd: &Path) -> String {
    if let Some(pc) = ProjectConfig::load(cwd) {
        if let Some(n) = pc.name { return sanitize_label(&n); }
    }
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if let Some(n) = package_json_name(d) { return sanitize_label(&n); }
        if let Some(n) = cargo_toml_name(d) { return sanitize_label(&n); }
        dir = d.parent();
    }
    if let Some(n) = git_root_name(cwd) { return sanitize_label(&n); }
    sanitize_label(&cwd.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default())
}

fn package_json_name(dir: &Path) -> Option<String> {
    let data = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&data).ok()?;
    // dedicated key first
    match v.get("portproxy") {
        Some(serde_json::Value::String(s)) => return Some(s.clone()),
        Some(serde_json::Value::Object(o)) => {
            if let Some(serde_json::Value::String(s)) = o.get("name") { return Some(s.clone()); }
        }
        _ => {}
    }
    let name = v.get("name")?.as_str()?;
    Some(name.rsplit('/').next().unwrap_or(name).to_string()) // strip @scope/
}

fn cargo_toml_name(dir: &Path) -> Option<String> {
    let data = std::fs::read_to_string(dir.join("Cargo.toml")).ok()?;
    let v: toml::Value = toml::from_str(&data).ok()?;
    Some(v.get("package")?.get("name")?.as_str()?.to_string())
}

fn git_root_name(cwd: &Path) -> Option<String> {
    // main repo root even from a linked worktree: parent of --git-common-dir
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .current_dir(cwd).output().ok()?;
    if !out.status.success() { return None; }
    let common = std::path::PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    Some(common.parent()?.file_name()?.to_string_lossy().to_string())
}
```

- [ ] **Step 3: `cargo test` pass. Commit `feat: name inference + config files`**

---

### Task 5: Worktree detection (`src/worktree.rs`)

**Files:** Create `src/worktree.rs`.

- [ ] **Step 1: Failing tests** — build real repos with `git` in tempdirs:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::tempdir;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let st = Command::new("git").args(args).current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t")
            .status().unwrap();
        assert!(st.success());
    }

    fn make_repo() -> tempfile::TempDir {
        let d = tempdir().unwrap();
        let main = d.path().join("main");
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init", "-b", "main"]);
        std::fs::write(main.join("f"), "x").unwrap();
        git(&main, &["add", "."]);
        git(&main, &["commit", "-m", "init"]);
        d
    }

    #[test]
    fn no_worktrees_no_suffix() {
        let d = make_repo();
        assert_eq!(worktree_suffix(&d.path().join("main")), None);
    }

    #[test]
    fn main_checkout_unsuffixed_even_with_worktrees() {
        let d = make_repo();
        let main = d.path().join("main");
        git(&main, &["worktree", "add", "-b", "feature/auth", "../wt-auth"]);
        assert_eq!(worktree_suffix(&main), None);
    }

    #[test]
    fn linked_worktree_gets_branch_last_segment() {
        let d = make_repo();
        let main = d.path().join("main");
        git(&main, &["worktree", "add", "-b", "feature/auth", "../wt-auth"]);
        assert_eq!(worktree_suffix(&d.path().join("wt-auth")).as_deref(), Some("auth"));
    }

    #[test]
    fn linked_worktree_on_main_branch_unsuffixed() {
        let d = make_repo();
        let main = d.path().join("main");
        git(&main, &["worktree", "add", "../wt-main"]); // detached HEAD
        assert_eq!(worktree_suffix(&d.path().join("wt-main")), None);
    }
}
```

- [ ] **Step 2: Implement**

```rust
// src/worktree.rs
use crate::utils::sanitize_label;
use std::path::{Path, PathBuf};
use std::process::Command;

fn git_out(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).current_dir(cwd).output().ok()?;
    if !out.status.success() { return None; }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Suffix for linked worktrees: sanitized last segment of the branch.
/// None for: not a repo / single worktree / main checkout / main|master / detached.
pub fn worktree_suffix(cwd: &Path) -> Option<String> {
    if let Some(s) = via_git_cli(cwd) { return s; }
    via_dotgit_file(cwd)
}

fn via_git_cli(cwd: &Path) -> Option<Option<String>> {
    let list = git_out(cwd, &["worktree", "list", "--porcelain"])?;
    let count = list.lines().filter(|l| l.starts_with("worktree ")).count();
    if count <= 1 { return Some(None); }
    let git_dir = git_out(cwd, &["rev-parse", "--path-format=absolute", "--git-dir"])?;
    let common = git_out(cwd, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    if git_dir == common { return Some(None); } // main checkout
    let branch = git_out(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    Some(branch_to_suffix(&branch))
}

fn branch_to_suffix(branch: &str) -> Option<String> {
    if branch == "HEAD" || branch == "main" || branch == "master" { return None; }
    let seg = branch.rsplit('/').next()?;
    let s = sanitize_label(seg);
    if s.is_empty() { None } else { Some(s) }
}

/// Fallback when git CLI is unavailable: parse the `.git` *file* of a linked
/// worktree (`gitdir: .../worktrees/<x>`; submodules point to `/modules/`).
fn via_dotgit_file(cwd: &Path) -> Option<String> {
    let mut dir = Some(cwd);
    let dotgit = loop {
        let d = dir?;
        let p = d.join(".git");
        if p.exists() { break p; }
        dir = d.parent();
    };
    if !dotgit.is_file() { return None; } // real dir => main checkout
    let content = std::fs::read_to_string(&dotgit).ok()?;
    let gitdir = content.strip_prefix("gitdir:")?.trim();
    if !gitdir.contains("/worktrees/") { return None; } // submodule
    let head = std::fs::read_to_string(PathBuf::from(gitdir).join("HEAD")).ok()?;
    let branch = head.trim().strip_prefix("ref: refs/heads/")?;
    branch_to_suffix(branch)
}
```

- [ ] **Step 3: `cargo test` pass. Commit `feat: git worktree suffix detection`**

---

### Task 6: Reverse proxy core (`src/proxy.rs`) — routing, headers, errors, hop limit

**Files:** Create `src/proxy.rs`.

- [ ] **Step 1: Failing integration test `tests/proxy_test.rs`**

```rust
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use std::net::SocketAddr;

async fn spawn_backend() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    // hyper server echoing method, path, and selected request headers as JSON
    // (full helper code in test file; binds 127.0.0.1:0)
    todo!()
}

#[tokio::test]
async fn routes_by_first_label_and_sets_forwarded_headers() {
    // 1. tempdir state, write routes.json [{hostname:"app", port:<backend>, pid:0}]
    // 2. spawn proxy on 127.0.0.1:0
    // 3. GET / with Host: app.example.com  -> 200, X-Portproxy: 1 on response,
    //    backend saw Host: localhost:<port>, X-Forwarded-Host: app.example.com
    // 4. Host: nope.example.com -> 404
}

#[tokio::test]
async fn hop_limit_508() {
    // request with x-portproxy-hops: 5 -> 508
}

#[tokio::test]
async fn idle_exit_after_grace() {
    // run_proxy with grace 200ms / idle 300ms and empty routes; expect the
    // returned future to resolve (proxy exits) within ~2s
}
```

- [ ] **Step 2: Implement `src/proxy.rs`**

```rust
// src/proxy.rs
use crate::routes::RouteStore;
use crate::types::Route;
use crate::utils::{host_label, MAX_HOPS};
use anyhow::Result;
use http_body_util::{combinators::BoxBody, BodyExt, Empty, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response, StatusCode};
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
    pub grace: Duration,        // default 10s
    pub idle_delay: Duration,   // default 5s
}

type RouteMap = Arc<tokio::sync::RwLock<HashMap<String, Route>>>;
type Body = BoxBody<Bytes, hyper::Error>;

pub async fn run_proxy(store: RouteStore, opts: ProxyOptions) -> Result<()> {
    let listener = TcpListener::bind(opts.listen).await?;
    let routes: RouteMap = Default::default();
    let (tx, rx) = watch::channel(false);

    // reloader: routes.json IS the IPC
    {
        let routes = routes.clone();
        tokio::spawn(async move {
            loop {
                let live = store.load();
                let _ = tx.send(!live.is_empty());
                *routes.write().await =
                    live.into_iter().map(|r| (r.hostname.clone(), r)).collect();
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
    }

    let client: Client<_, Incoming> =
        Client::builder(TokioExecutor::new()).build_http();

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
        #[allow(unreachable_code)] anyhow::Ok(())
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
            if *rx.borrow() { still_idle = false; break; }
        }
        if still_idle { return; } // caller exits process
    }
}

async fn handle(
    mut req: Request<Incoming>,
    routes: RouteMap,
    client: Client<hyper_util::client::legacy::connect::HttpConnector, Incoming>,
    peer: SocketAddr,
) -> Result<Response<Body>, hyper::Error> {
    // hop guard
    let hops = req.headers().get("x-portproxy-hops")
        .and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0);
    if hops >= MAX_HOPS {
        return Ok(stamp(text(StatusCode::LOOP_DETECTED, "loop detected")));
    }

    let host = req.headers().get(hyper::header::HOST)
        .and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let Some(label) = host_label(&host) else {
        return Ok(stamp(text(StatusCode::BAD_REQUEST, "missing Host header")));
    };
    let Some(route) = routes.read().await.get(&label).cloned() else {
        let names: Vec<String> = routes.read().await.keys().cloned().collect();
        return Ok(stamp(not_found(&label, &names)));
    };

    // websocket upgrade?
    let is_upgrade = req.headers().get(hyper::header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .map_or(false, |v| v.to_lowercase().contains("upgrade"))
        && req.headers().contains_key(hyper::header::UPGRADE);
    if is_upgrade {
        return Ok(stamp(websocket_tunnel(req, route.port, &host).await));
    }

    // rewrite request for backend
    let path = req.uri().path_and_query()
        .map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
    let uri: hyper::Uri = format!("http://127.0.0.1:{}{}", route.port, path)
        .parse().expect("valid backend uri");
    *req.uri_mut() = uri;
    set_forward_headers(req.headers_mut(), &host, peer, route.port);

    match client.request(req).await {
        Ok(resp) => Ok(stamp(resp.map(|b| b.boxed()))),
        Err(_) => Ok(stamp(text(StatusCode::BAD_GATEWAY,
            &format!("backend for \"{label}\" (port {}) is not responding", route.port)))),
    }
}

fn set_forward_headers(h: &mut hyper::HeaderMap, host: &str, peer: SocketAddr, port: u16) {
    use hyper::header::{HeaderValue, HOST};
    let hops = h.get("x-portproxy-hops")
        .and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
    h.insert("x-portproxy-hops", HeaderValue::from_str(&(hops + 1).to_string()).unwrap());
    h.insert(HOST, HeaderValue::from_str(&format!("localhost:{port}")).unwrap());
    // append, preserving values set by Caddy
    let xff = match h.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        Some(prev) => format!("{prev}, {}", peer.ip()),
        None => peer.ip().to_string(),
    };
    h.insert("x-forwarded-for", HeaderValue::from_str(&xff).unwrap());
    if !h.contains_key("x-forwarded-proto") {
        h.insert("x-forwarded-proto", HeaderValue::from_static("http"));
    }
    if !h.contains_key("x-forwarded-host") {
        h.insert("x-forwarded-host", HeaderValue::from_str(host).unwrap_or(HeaderValue::from_static("")));
    }
}

fn stamp(mut resp: Response<Body>) -> Response<Body> {
    resp.headers_mut().insert("x-portproxy", hyper::header::HeaderValue::from_static("1"));
    resp
}

fn text(status: StatusCode, msg: &str) -> Response<Body> {
    Response::builder().status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(msg.to_string())).map_err(|e| match e {}).boxed())
        .unwrap()
}

fn not_found(label: &str, names: &[String]) -> Response<Body> {
    let list = if names.is_empty() { "<li><em>none</em></li>".to_string() }
        else { names.iter().map(|n| format!("<li><code>{n}</code></li>")).collect() };
    let html = format!(
        "<!doctype html><meta charset=utf-8><title>portproxy: not found</title>\
         <body style=\"font-family:system-ui;max-width:40rem;margin:4rem auto\">\
         <h1>No app named <code>{label}</code></h1>\
         <p>Active routes:</p><ul>{list}</ul></body>");
    Response::builder().status(StatusCode::NOT_FOUND)
        .header("content-type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(html)).map_err(|e| match e {}).boxed())
        .unwrap()
}
```

(`websocket_tunnel` is Task 7; for this task it returns 502 `text(...)` placeholder that Task 7 replaces — the function exists so the file compiles.)

- [ ] **Step 3: tests pass. Commit `feat: host-label reverse proxy with idle exit`**

---

### Task 7: WebSocket tunnel

**Files:** Modify `src/proxy.rs`; extend `tests/proxy_test.rs`.

- [ ] **Step 1: Failing test** — backend that performs a minimal WS echo over raw TCP (accept upgrade with computed `Sec-WebSocket-Accept`, echo frames verbatim); client connects through proxy with `Host: app.x`, sends a masked text frame, expects echo.

- [ ] **Step 2: Implement**

```rust
async fn websocket_tunnel(req: Request<Incoming>, port: u16, host: &str) -> Response<Body> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let backend = match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
        Ok(s) => s,
        Err(_) => match tokio::net::TcpStream::connect(("::1", port)).await {
            Ok(s) => s,
            Err(_) => return text(StatusCode::BAD_GATEWAY, "backend unreachable"),
        },
    };
    let mut backend = backend;

    // raw handshake to backend
    let path = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/").to_string();
    let mut raw = format!("{} {} HTTP/1.1\r\n", req.method(), path);
    raw.push_str(&format!("Host: localhost:{port}\r\n"));
    for (k, v) in req.headers() {
        if k == hyper::header::HOST { continue; }
        if let Ok(v) = v.to_str() { raw.push_str(&format!("{k}: {v}\r\n")); }
    }
    raw.push_str(&format!("X-Forwarded-Host: {host}\r\n\r\n"));
    if backend.write_all(raw.as_bytes()).await.is_err() {
        return text(StatusCode::BAD_GATEWAY, "backend handshake write failed");
    }

    // read backend response headers byte-by-byte (no overshoot into frames)
    let mut head = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 16 * 1024 || backend.read_exact(&mut byte).await.is_err() {
            return text(StatusCode::BAD_GATEWAY, "backend handshake read failed");
        }
        head.push(byte[0]);
    }
    let head_str = String::from_utf8_lossy(&head);
    let mut lines = head_str.split("\r\n");
    let status: u16 = lines.next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok()).unwrap_or(502);
    let mut builder = Response::builder().status(status);
    for line in lines {
        if let Some((k, v)) = line.split_once(": ") {
            builder = builder.header(k, v);
        }
    }
    let resp = builder
        .body(Empty::<Bytes>::new().map_err(|e| match e {}).boxed())
        .unwrap();

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
```

- [ ] **Step 3: tests pass. Commit `feat: websocket tunneling`**

---

### Task 8: CLI — run command, daemonize, signals, cleanup (`src/main.rs`)

**Files:** Rewrite `src/main.rs`.

- [ ] **Step 1: Manual CLI parse + clap subcommands**

Reserved first-args: `run proxy list get alias prune clean help`. Dispatch:
`portproxy run <cmd...> [--name N] [--force] [--app-port P]`,
`portproxy <name> <cmd...>` (name = non-reserved first arg, cmd non-empty),
plus subcommands below. `PORTPROXY=0` → exec directly.

```rust
// src/main.rs (structure)
use anyhow::{bail, Context, Result};
use portproxy::{config::GlobalConfig, naming, ports, proxy, routes::RouteStore, utils, worktree};

const RESERVED: &[&str] = &["run", "proxy", "list", "get", "alias", "prune", "clean",
                            "help", "-h", "--help", "-V", "--version"];

#[tokio::main]
async fn main() -> std::process::ExitCode { /* dispatch, map Result -> exit code */ }

struct RunArgs {
    name: Option<String>,
    force: bool,
    app_port: Option<u16>,
    cmd: Vec<String>,
}

async fn cmd_run(args: RunArgs) -> Result<i32> {
    if std::env::var("PORTPROXY").map_or(false, |v| v == "0" || v == "skip") {
        return exec_passthrough(&args.cmd).await;
    }
    let cwd = std::env::current_dir()?;
    let base = args.name.clone()
        .map(|n| utils::sanitize_label(&n))
        .unwrap_or_else(|| naming::infer_name(&cwd));
    let label = match worktree::worktree_suffix(&cwd) {
        Some(sfx) => utils::sanitize_label(&format!("{base}-{sfx}")),
        None => base,
    };
    if label.is_empty() { bail!("could not infer a name; pass --name"); }

    let state = utils::state_dir();
    let cfg = GlobalConfig::load(&state);
    ensure_proxy(&state, &cfg).await?;

    let port = match args.app_port {
        Some(p) => p,
        None => ports::find_free_port().context("no free port in 4000-4999")?,
    };
    let store = RouteStore::new(state.clone());
    store.add_route(&label, port, std::process::id(), args.force)?;

    let final_cmd = ports::inject_framework_flags(&args.cmd, port);
    let url = cfg.url_for(&label);
    eprintln!("portproxy: {} -> 127.0.0.1:{}{}", label, port,
        url.as_deref().map(|u| format!("  ({u})")).unwrap_or_default());

    let code = run_child(&final_cmd, port, &label, url.as_deref()).await;

    store.remove_route(&label)?;
    shutdown_proxy_if_idle(&state, &store);
    code
}
```

- [ ] **Step 2: Child spawn + signal forwarding (tokio)**

```rust
async fn run_child(cmd: &[String], port: u16, label: &str, url: Option<&str>) -> Result<i32> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut c = tokio::process::Command::new(&cmd[0]);
    c.args(&cmd[1..])
        .env("PORT", port.to_string())
        .env("HOST", "127.0.0.1")
        .env("PORTPROXY_NAME", label)
        .env("__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS", ".localhost");
    if let Some(u) = url { c.env("PORTPROXY_URL", u); }
    unsafe { c.pre_exec(|| { libc::setpgid(0, 0); Ok(()) }); } // own process group
    let mut child = c.spawn().with_context(|| format!("failed to run {:?}", cmd[0]))?;
    let pgid = child.id().map(|p| p as i32);

    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    loop {
        tokio::select! {
            status = child.wait() => {
                let status = status?;
                return Ok(exit_code(status));
            }
            _ = sigint.recv() => forward(pgid, nix::sys::signal::Signal::SIGINT),
            _ = sigterm.recv() => forward(pgid, nix::sys::signal::Signal::SIGTERM),
        }
    }
}

fn forward(pgid: Option<i32>, sig: nix::sys::signal::Signal) {
    if let Some(p) = pgid {
        let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(p), sig);
    }
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
}
```

- [ ] **Step 3: ensure_proxy / daemonize / idle stop**

```rust
async fn ensure_proxy(state: &std::path::Path, cfg: &GlobalConfig) -> Result<()> {
    if utils::is_proxy_running(&cfg.listen) { return Ok(()); }
    std::fs::create_dir_all(state)?;
    let log = std::fs::OpenOptions::new().create(true).append(true)
        .open(state.join("proxy.log"))?;
    let exe = std::env::current_exe()?;
    let mut c = std::process::Command::new(exe);
    c.args(["proxy", "start", "--foreground", "--listen", &cfg.listen])
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?).stderr(log);
    unsafe { use std::os::unix::process::CommandExt;
        c.pre_exec(|| { libc::setsid(); Ok(()) }); }
    c.spawn()?;
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        if utils::is_proxy_running(&cfg.listen) { return Ok(()); }
    }
    bail!("proxy failed to start; see {}", state.join("proxy.log").display());
}

fn shutdown_proxy_if_idle(state: &std::path::Path, store: &RouteStore) {
    if !store.load().is_empty() { return; }
    if let Ok(pid) = std::fs::read_to_string(state.join("proxy.pid")) {
        if let Ok(pid) = pid.trim().parse::<i32>() {
            let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid),
                                           nix::sys::signal::Signal::SIGTERM);
        }
        let _ = std::fs::remove_file(state.join("proxy.pid"));
        let _ = std::fs::remove_file(state.join("proxy.port"));
    }
}
```

`proxy start --foreground`: write `proxy.pid` (own PID) + `proxy.port`, install SIGTERM handler that removes them, `proxy::run_proxy(...)` then cleanup + exit 0 (idle exit path also cleans pid files).

- [ ] **Step 4: Build, manual smoke test:**

```
PORTPROXY_STATE_DIR=/tmp/ppx cargo run -- demo python3 -m http.server -- 0   # registers "demo"
curl -H 'Host: demo.example.com' 127.0.0.1:1355/   # -> directory listing
```

- [ ] **Step 5: Commit `feat: run command, daemonize, signal forwarding`**

---

### Task 9: Subcommands — proxy start/stop, list, get, alias, prune, clean

**Files:** Modify `src/main.rs`.

- [ ] **Step 1: Implement**

```rust
// proxy start [--foreground] [-l/--listen ADDR]  : foreground runs run_proxy;
//   without --foreground, daemonize via ensure_proxy.
// proxy stop  : read proxy.pid -> SIGTERM, remove pid/port files.
// list        : store.load(); print "<label>  127.0.0.1:<port>  pid <pid>  <url?>"
//               (colored; url only when base_domain set)
// get <name>  : find route; print cfg.url_for(name) or bail
//               "base_domain not set in ~/.portproxy/config.toml"
// alias <name> <port> [--remove] [--force] : add_route(name, port, 0, force) /
//               remove_route(name)
// prune [--force] : for each load_raw() entry with pid!=0 && !pid_alive(pid):
//               find pids via `lsof -ti tcp:<port>`; SIGTERM (or SIGKILL with
//               --force); remove entry. Report counts.
// clean       : proxy stop best-effort, then remove_dir_all(state_dir).
```

Full code mirrors patterns from Task 8 (RouteStore + GlobalConfig already exist; each subcommand is <30 lines).

- [ ] **Step 2: Smoke test each command. Commit `feat: management subcommands`**

---

### Task 10: End-to-end verification + README

**Files:** Create `README.md`.

- [ ] **Step 1: E2E scenario (manual, scripted in `scripts/e2e.sh`)**

```bash
export PORTPROXY_STATE_DIR=$(mktemp -d)
cargo build
# 1. start two apps with the same inferred-name machinery
./target/debug/portproxy app1 python3 -m http.server &
./target/debug/portproxy app2 python3 -m http.server &
sleep 2
curl -fsS -H 'Host: app1.x.test' 127.0.0.1:1355/ >/dev/null
curl -fsS -H 'Host: app2.x.test' 127.0.0.1:1355/ >/dev/null
./target/debug/portproxy list
# 2. conflict
./target/debug/portproxy app1 python3 -m http.server && echo "BUG: should conflict"
# 3. kill both, expect proxy self-exit within ~6s
kill %1 %2; sleep 7
curl -s -m 1 127.0.0.1:1355/ && echo "BUG: proxy still up"
```

- [ ] **Step 2: README** — what/why, Caddy snippet (`*.dev.example.test { reverse_proxy host.docker.internal:1355 }`), CLI reference, config.toml reference, name inference + worktree rules table.

- [ ] **Step 3: `cargo clippy -- -D warnings`, `cargo fmt`, full `cargo test`. Commit `docs: README + e2e script`**
