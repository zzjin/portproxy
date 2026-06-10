# portproxy — Design Spec

Date: 2026-06-10
Status: Approved

## Goal

Rust CLI that gives local dev servers stable named URLs behind an existing TLS-terminating
reverse proxy (Caddy/Nginx). Combines Vercel `portless` UX (auto naming, worktree
discovery, monorepo-grade name inference) with `portless-rs` architecture (single small
binary, spawn-on-demand proxy, idle self-exit, no resident daemon, file-based state).

Out of scope (handled by upstream Caddy/Nginx): TLS, certificates, DNS, `/etc/hosts`,
custom TLDs, mDNS/LAN, tailscale/ngrok sharing, OS service install.

## Architecture

Single binary `portproxy`, two modes:

1. **Wrapper mode** — `portproxy [name] <cmd...>`: ensure proxy running (probe; spawn
   self as `proxy start --foreground` detached via `setsid`, stdout/err →
   `~/.portproxy/proxy.log`), allocate free port, register route, spawn child command
   with injected env, forward SIGINT/SIGTERM, deregister on exit, SIGTERM proxy if no
   routes remain.
2. **Proxy mode** — `portproxy proxy start --foreground`: hyper HTTP/1.1 reverse proxy.
   Reload `routes.json` every 100 ms into in-memory cache (file IS the IPC). Idle
   self-shutdown: 10 s startup grace, then 5 s of zero live routes → `exit(0)`.

Stack: `tokio`, `hyper` 1.x + `hyper-util`, `http-body-util`, `clap` (derive),
`serde`/`serde_json`, `toml`, `dirs`, `rand`, `nix` (signal/process), `anyhow`,
`colored`. No TLS deps. Unix only (Linux/macOS).

## Routing

- Route key = **first DNS label** of `Host` header (port stripped, lowercased):
  `sample-web-auth.dev.example.test` → `sample-web-auth`. Rest of domain ignored —
  proxy is domain-agnostic; Caddy decides what to forward.
- Exact match against route table. Miss → styled 404 HTML listing active routes.
  Backend connect failure → 502.
- Upstream request: connect `127.0.0.1:<port>` (fallback `::1`), rewrite
  `Host: localhost:<port>` (Vite host-check), append `X-Forwarded-For/Proto/Host/Port`
  preserving existing values set by Caddy. Response stamped `X-Portproxy: 1`
  (health-probe beacon). `x-portproxy-hops` counter, max 5 → 508 loop detection.
- WebSocket: detect upgrade, raw TCP handshake to backend, relay 101, then
  `tokio::io::copy_bidirectional`.
- Listen address: default `127.0.0.1:1355`; configurable (`0.0.0.0`, docker bridge IP)
  via config/flag for dockerized Caddy.

## State (`~/.portproxy/`, override `PORTPROXY_STATE_DIR`)

- `routes.json` — array `{hostname, port, pid}`; `hostname` is the single label;
  `pid` = wrapper PID; `pid == 0` = static alias (never auto-pruned).
- `routes.lock` — lock directory (atomic `mkdir`), retries w/ backoff, stale after 10 s.
- Dead-PID routes filtered on every load (`kill(pid, 0)`), persisted when under lock.
- `proxy.pid`, `proxy.port`, `proxy.log`.
- `config.toml` (optional): `listen = "127.0.0.1:1355"`, `base_domain = "dev.example.test"`,
  `scheme = "https"` (default; base_domain/scheme used only by `get`/`list` to print
  full URLs — the proxy itself never needs them).

## Name auto-discovery (priority high → low; same files/order as Vercel portless)

1. `--name` flag
2. `portproxy.json` in cwd (`{"name": ...}`; cwd only, no walk-up)
3. `package.json` `"portproxy"` key in cwd (string shorthand or `{name}`; cwd only)
4. `package.json` `"name"` — walk up dirs, strip `@scope/`
5. Git repo root basename (`git rev-parse --show-toplevel`; filesystem fallback:
   walk up looking for `.git`)
6. cwd basename

A source whose value sanitizes to an empty label falls through to the next one.

Sanitize to DNS label: lowercase, non-`[a-z0-9-]` → `-`, collapse/trim hyphens,
63-char cap with 6-hex sha256 suffix on truncation.

## Worktree detection

- `git worktree list --porcelain`; ≤1 worktree → no suffix.
- Only **linked** worktrees suffixed: `git rev-parse --git-dir` ≠ `--git-common-dir`.
  Main checkout never suffixed (feature branches in main clone stay bare).
- Branch via `git rev-parse --abbrev-ref HEAD`; `main`/`master`/detached → no suffix.
- Suffix = sanitized **last segment** of branch (`feature/auth` → `auth`), joined as
  `<name>-<seg>` (single label): `sample-web-auth`.
- No-git-CLI fallback: parse `.git` file `gitdir: .../worktrees/<x>` (distinguish from
  submodule `/modules/`), read branch from that gitdir's HEAD.
- `--name` overrides base name; worktree suffix still applied.

## Port allocation & child env

- Random free port in 4000–4999 (bind-test; 50 random tries then linear scan), or
  `--app-port` fixed.
- Child env: `PORT`, `HOST=127.0.0.1`, `PORTPROXY_NAME=<label>`,
  `PORTPROXY_URL=<url if base_domain configured>`,
  `__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS=.localhost`.
- Framework flag injection for PORT-ignoring tools (vite, react-router, astro, ng,
  rsbuild): append `--port <n>` (+ `--strictPort` where supported) `--host 127.0.0.1`;
  handle npx/pnpx/bunx/yarn-dlx/pnpm-dlx runners. `PORTPROXY=0` env → run command
  directly, no proxy.

## Name collision

Live-PID route with same hostname → error:
`"<name>" is already registered by a running process (PID n). Use --force to override.`
`--force` → SIGTERM old wrapper, replace route.

## CLI surface

```
portproxy run <cmd...>          # auto-inferred name (use --name to override)
portproxy <name> <cmd...>       # explicit name (name must not be a reserved subcommand)
# bare `portproxy <cmd...>` is NOT supported: ambiguous with the <name> form
portproxy proxy start|stop [--foreground] [-l listen]
portproxy list                  # active routes (+ URLs if base_domain set)
portproxy get <name>            # print URL (requires base_domain)
portproxy alias <name> <port> [--remove] [--force]   # static route, pid=0
portproxy prune [--force]       # kill orphaned dev servers (SIGTERM / SIGKILL)
portproxy clean                 # stop proxy, remove all state
```

Run flags: `--name`, `--force`, `--app-port`. Env: `PORTPROXY=0`, `PORTPROXY_PORT`,
`PORTPROXY_STATE_DIR`.

## Module layout

```
src/main.rs      — clap dispatch, run command, daemonize, signal forwarding, cleanup
src/proxy.rs     — hyper server, route cache + reloader, idle shutdown, WS tunnel
src/routes.rs    — RouteStore: JSON + dir lock + PID liveness
src/naming.rs    — name inference chain + sanitize
src/worktree.rs  — git worktree/branch detection + .git-file fallback
src/config.rs    — config.toml, portproxy.toml, package.json portproxy key
src/ports.rs     — free-port finder, framework flag injection
src/types.rs     — Route, shared types
src/utils.rs     — state dir, hostname validation, proxy probe
```

## Error handling

- Proxy unreachable after spawn (5 s poll) → abort with log-path hint.
- Route lock starvation → error after ~5 s of retries.
- Child exit code propagated (signal deaths → 128+n).
- Corrupt `routes.json` → treat as empty, rewrite (self-healing).

## Testing

- Unit: name sanitization, inference priority (tempdir fixtures), worktree parsing
  (fixture `.git` files + real `git worktree` in tempdir), route store
  lock/liveness/conflict, host-label parsing, port finder.
- Integration: spawn proxy in-process, register route to a dummy hyper backend,
  assert host routing / 404 / X-Forwarded headers / hop limit / WS echo / idle exit.
- E2E (manual): real `vite`/`next dev` behind local Caddy.
