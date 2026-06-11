# portproxy

Stable named URLs for local dev servers running **behind a TLS-terminating
reverse proxy** (Caddy, Nginx, ...). Inspired by
[vercel-labs/portless](https://github.com/vercel-labs/portless) (auto naming,
worktree discovery) and [portless-rs](https://github.com/portless-rs/portless)
(small single binary, spawn-on-demand proxy, no resident daemon).

```
browser ──https──> Caddy/Nginx (TLS, *.dev.example.test) ──http──> portproxy ──> 127.0.0.1:4xxx
```

portproxy deliberately does **no** TLS, DNS, certificates or `/etc/hosts`
management — your upstream proxy already owns that. It only looks at the
**first DNS label** of the `Host` header (`sample-web-auth.dev.example.test` →
route `sample-web-auth`) and forwards to the registered local port. Everything
else is passed through transparently; `X-Forwarded-*` headers set by the
upstream proxy are preserved.

## How it works

- `portproxy run <cmd>` picks a free port (4000–4999), exports it as `$PORT`,
  registers `<name> -> port` in `~/.portproxy/routes.json`, and runs your
  command.
- The reverse proxy is spawned on demand (detached via `setsid`), reloads
  `routes.json` every 100 ms, and **exits by itself** ~5 s after the last
  route disappears. No daemon to install, nothing running while you don't
  develop.
- Routes owned by dead processes are pruned automatically (PID liveness
  check), so crashes self-heal.
- WebSockets are tunneled; `x-portproxy-hops` guards against proxy loops
  (max 5 → 508).

## Install

Via npm (downloads the prebuilt binary for your platform from GitHub Releases
on postinstall; Linux x64/arm64 and macOS x64/arm64):

```sh
npm install -g portproxy
```

Or build from source:

```sh
cargo build --release          # target/release/portproxy, single binary
```

## Usage

```
portproxy run <cmd...>             run with auto-inferred name
portproxy <name> <cmd...>          run with explicit name
portproxy proxy start [--foreground] [-l ADDR]
portproxy proxy stop
portproxy list                     show active routes
portproxy get <name>               print URL (needs base_domain in config)
portproxy alias <name> <port> [--remove] [--force]   static route (Docker etc.)
portproxy prune [--force]          kill orphaned dev servers
portproxy clean                    stop proxy and remove all state
```

Run flags: `--name <n>` (override inferred name), `--force` (take over a name
owned by a live process — that process gets SIGTERM), `--app-port <p>`.

Examples:

```sh
cd ~/code/sample-web && portproxy run pnpm dev
# portproxy: sample-web -> 127.0.0.1:4123  (https://sample-web.dev.example.test)

portproxy api cargo run            # explicit name "api"
portproxy alias dashboard 3000       # route a Docker container
PORTPROXY=0 pnpm dev               # bypass portproxy entirely
```

## Name inference

Highest priority first (same files and read order as Vercel portless):

1. `--name` flag
2. `portproxy.json` in cwd: `{ "name": "..." }` (cwd only, no walk-up)
3. `package.json` `"portproxy"` key in cwd (string shorthand or `{ "name": ... }`,
   cwd only)
4. nearest `package.json` `"name"` walking up directories (`@scope/` stripped)
5. git repository root directory name (`git rev-parse --show-toplevel`; walks
   up looking for `.git` when the git CLI is unavailable)
6. current directory name

Names are sanitized to a DNS label (lowercase, `[a-z0-9-]`, max 63 chars with
a hash suffix on truncation); a source that sanitizes to empty falls through
to the next one.

## Monorepos

Workspace-aware, same behavior as Vercel portless (adapted to single-label
names):

- Workspace discovery via `pnpm-workspace.yaml` or package.json `workspaces`
  (npm/yarn/bun), walking up from the current directory.
- **Project name** = root `portproxy.json` `name` → root package.json
  `portproxy` key → most common npm scope across packages (`@example/web` +
  `@example/api` → `example`) → plain inference on the root.
- Running inside a member package names it `<project>-<pkg>`
  (`example-web`); a package whose short name equals the project name gets the
  bare project name.
- **`portproxy run` at the workspace root starts every package's `dev`
  script**, each with its own port and route. Build-only dev scripts (tsc,
  tsup, esbuild, rollup, webpack, `* build`, ...) run without a route.
  Ctrl-C / SIGTERM stops the whole fleet and cleans every route.
- Per-package overrides in root `portproxy.json`:
  `{ "name": "example", "apps": { "packages/web": { "name": "frontend" } } }`
- `portproxy run` inside a single package (no command) runs its `dev` script
  via the detected package manager (pnpm/yarn/bun/npm by lockfile).

## Git worktrees

Linked worktrees automatically get a branch suffix in the same label:

| checkout | branch | name |
|---|---|---|
| main clone | anything | `sample-web` |
| linked worktree | `feature/auth` | `sample-web-auth` |
| linked worktree | `main` / detached | `sample-web` |

Only *linked* worktrees are suffixed (detected via `git rev-parse --git-dir`
vs `--git-common-dir`, with a `.git`-file parsing fallback when the git CLI is
unavailable). The suffix is the sanitized last segment of the branch name.

## Configuration

`~/.portproxy/config.toml` (all optional):

```toml
listen = "127.0.0.1:1355"        # proxy listen address
base_domain = "dev.example.test"   # only used to print URLs (get/list/banner)
scheme = "https"                 # only used to print URLs
```

If Caddy runs in Docker without host networking, set `listen` to an address
the container can reach (e.g. the docker bridge gateway `172.17.0.1:1355` or
`0.0.0.0:1355` + firewall).

Per-project config — `portproxy.json` in the project directory, or the
package.json `"portproxy"` key (string shorthand sets the name):

```json
{
  "name": "myapp",
  "script": "dev",
  "appPort": 4123,
  "proxy": true,
  "apps": { "packages/web": { "name": "frontend", "script": "serve" } }
}
```

- `script` — package.json script used by bare `portproxy run` (default `dev`,
  `--script` flag wins)
- `appPort` — fixed backend port instead of random 4000–4999
- `proxy` — `false`: run without a route; `true`: always route (skips
  build-command auto-detection); absent: auto
- `apps` — per-package overrides at a workspace root (same keys), keyed by
  root-relative path

Environment: `PORTPROXY_STATE_DIR` (state location, default `~/.portproxy`),
`PORTPROXY_LISTEN` (proxy listen address, beats config.toml),
`PORTPROXY_APP_PORT` (fixed app port), `PORTPROXY=0` (bypass).

### Caddy

```caddyfile
*.dev.example.test {
    reverse_proxy host.docker.internal:1355   # or 172.17.0.1:1355
}
```

### Nginx

```nginx
server {
    server_name ~^.+\.ubl6\.zzjin\.net$;
    location / {
        proxy_pass http://127.0.0.1:1355;
        proxy_set_header Host $host;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection $connection_upgrade;
    }
}
```

## Child process environment

| var | value |
|---|---|
| `PORT` | allocated port |
| `HOST` | `127.0.0.1` |
| `PORTPROXY_NAME` | resolved label |
| `PORTPROXY_URL` | full URL (only when `base_domain` is configured) |

For tools that ignore `$PORT`, flags are appended automatically:
vite / react-router / rsbuild get `--port N --strictPort --host 127.0.0.1`;
astro / ng get `--port N --host 127.0.0.1` (also through npx / pnpm dlx /
yarn dlx / bunx). Next.js, Nuxt, Express etc. honor `$PORT` directly.

## Agent skill

`skills/portproxy/SKILL.md` teaches AI agents (Claude Code etc.) how to use
portproxy: discover URLs via `list`/`get`, never hardcode ports, start dev
servers through the wrapper. Symlink or copy it into your agent's skills
directory.

## Design decisions — what we deliberately don't do

portproxy assumes a TLS-terminating reverse proxy (Caddy/Nginx) in front of
it. Everything that proxy already does better is out of scope, on purpose:

- **TLS / local CA / port 443 / `trust`** — Caddy terminates TLS with real
  certificates on the public domain. A local CA would add state, sudo
  prompts, and trust-store mutation for zero benefit here.
- **DNS, `/etc/hosts` sync, custom TLDs, mDNS/LAN** — the upstream proxy's
  (sub)domains are real DNS names; nothing to resolve locally.
- **HTTP/2** — portless uses h2 to dodge the browser's 6-connections-per-host
  HTTP/1.1 limit. That limit applies browser-side, where Caddy already
  speaks h2/h3; the Caddy→portproxy loopback hop has no such limit.
- **Subdomain / `--wildcard` multi-tenant routing** — multi-level wildcard
  hostnames (`tenant1.myapp.example.com`) need multi-level wildcard
  certificates, which is upstream territory. Configure extra hostnames in
  Caddy and point them at the same app with `portproxy alias` if needed.
- **`service install` / resident daemon** — the proxy spawns on demand and
  exits when idle (portless-rs philosophy). After a reboot the next
  `portproxy run` brings everything back. Corner case: alias-only setups
  (pid 0 routes keep the proxy alive but nothing respawns it after reboot)
  — run `portproxy proxy start` once, or wrap it in your own systemd unit.
- **Tailscale / ngrok / funnel sharing** — your Caddy domain *is* the share
  URL; exposure policy belongs to the proxy/firewall layer.
- **Expo / React Native flag injection** — those flows are LAN-device
  debugging on a laptop; meaningless on a headless server.
- **Turborepo-specific integration** — `portproxy run` at a workspace root
  spawns each package directly; if you prefer turbo's orchestration,
  `portproxy run turbo dev` still works as a single wrapped command (one
  route, turbo's own port management inside).

## Development

```sh
cargo test            # unit + integration tests
./scripts/e2e.sh      # full lifecycle: routing, conflict, idle self-exit
```

## License

MIT
