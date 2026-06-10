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

Highest priority first:

1. `--name` flag
2. `portproxy.toml` in cwd: `name = "..."`
3. nearest `package.json` walking up: `"portproxy"` key (string or `{ "name": ... }`)
4. nearest `package.json` `"name"` (with `@scope/` stripped)
5. nearest `Cargo.toml` `[package] name`
6. git main-repository root directory name
7. current directory name

Names are sanitized to a DNS label (lowercase, `[a-z0-9-]`, max 63 chars with
a hash suffix on truncation).

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

Environment: `PORTPROXY_STATE_DIR` (state location, default `~/.portproxy`),
`PORTPROXY=0` (bypass).

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

## Development

```sh
cargo test            # unit + integration tests
./scripts/e2e.sh      # full lifecycle: routing, conflict, idle self-exit
```

## License

MIT
