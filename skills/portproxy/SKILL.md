---
name: portproxy
description: Run local dev servers behind a TLS-terminating reverse proxy (Caddy/Nginx) with stable named URLs (e.g. https://myapp.dev.example.test instead of http://localhost:3000). Use when starting dev servers on this machine, wiring services to each other, configuring app names, working with monorepos or git worktrees, or troubleshooting port/proxy issues.
---

# portproxy

## Core Purpose

portproxy replaces port numbers with stable named URLs for dev servers running
behind an existing reverse proxy. The upstream proxy (Caddy/Nginx) owns TLS and
the wildcard domain; portproxy routes by the **first DNS label** of the Host
header to a per-app local port. Never hardcode ports — names are stable, ports
are not.

## When to Use

- Starting any dev server on this machine (`npm run dev`, `cargo run`, ...)
- "What URL is app X on?" / wiring one service to another
- Port conflicts (EADDRINUSE) between projects
- Running a monorepo's apps together
- Working in git worktrees (each gets its own URL automatically)

## Key Commands

```bash
portproxy run pnpm dev          # auto-inferred name -> https://<name>.<base_domain>
portproxy myapp pnpm dev        # explicit name
portproxy run                   # package dir: run its "dev" script
                                # workspace root: start ALL packages (monorepo)
portproxy run --script start    # choose a different package.json script
portproxy list                  # active routes + URLs  <- USE THIS for discovery
portproxy get <name>            # one app's URL; cwd worktree suffix auto-applied,
                                # works before the app starts (--no-worktree to skip)
portproxy alias dashboard 3000    # static route for non-wrapped process (Docker)
portproxy prune                 # kill orphaned dev servers after crashes
portproxy proxy start|stop      # manual proxy control (rarely needed)
```

Run flags: `--name <n>`, `--force` (take over a live name; old process gets
SIGTERM), `--app-port <p>`, `--script <s>`.

## How It Works

1. `portproxy run <cmd>` picks a free port (4000–4999), exports `PORT`,
   registers `<name> -> port` in `~/.portproxy/routes.json`, runs the command.
2. The proxy spawns on demand and **exits by itself** ~5 s after the last
   route disappears — no daemon, nothing to install or keep running.
3. Upstream Caddy forwards `*.<base_domain>` to the proxy; the proxy matches
   the first Host label and forwards to `127.0.0.1:<port>`.
4. Dead-process routes are pruned automatically (PID liveness).

## Name Inference (priority order)

1. `--name` flag
2. `portproxy.json` `name` (cwd only)
3. package.json `"portproxy"` key (cwd only; string or `{ "name": ... }`)
4. nearest package.json `"name"` walking up (`@scope/` stripped)
5. git repo root directory name
6. cwd basename

Monorepo: workspace members are `<project>-<pkg>` (project = root config name
or majority npm scope). Git linked worktrees append the branch's last segment:
`myapp-auth` for branch `feature/auth`.

## Configuration

`portproxy.json` (project dir) or package.json `"portproxy"` key:

```json
{
  "name": "myapp",
  "script": "dev",
  "appPort": 4123,
  "proxy": true,
  "apps": { "packages/web": { "name": "frontend" } }
}
```

- `script` — package.json script for bare `portproxy run` (default `dev`)
- `appPort` — fixed port instead of auto-assignment
- `proxy: false` — run without route; `true` — always route (skips
  build-command auto-detection); absent — auto
- `apps` — per-package overrides at a workspace root, keyed by relative path

Global `~/.portproxy/config.toml`: `listen` (proxy address — string or array;
default dual-stack loopback `["127.0.0.1:1355", "[::1]:1355"]` so server-side
`http://name.localhost:1355` requests, which resolve to `::1` per RFC 6761,
work alongside Caddy's `127.0.0.1`), `base_domain` + `scheme` (URL printing
only; unset falls back to `http://<name>.localhost:<listen port>`, which
routes through the proxy with zero configuration).

## Environment Variables

| Variable | Purpose |
|---|---|
| `PORTPROXY=0` | Bypass: run the command directly |
| `PORTPROXY_LISTEN` | Override proxy listen address |
| `PORTPROXY_APP_PORT` | Fixed app port (same as --app-port) |
| `PORTPROXY_STATE_DIR` | State directory (default ~/.portproxy) |

Injected into child processes: `PORT`, `HOST=127.0.0.1`, `PORTPROXY_NAME`,
`PORTPROXY_URL` (`.localhost` fallback when base_domain unset).

## Troubleshooting

- **502 from the proxy** — app not listening on its assigned `PORT`. Most
  tools honor `$PORT`; vite/react-router/rsbuild/astro/ng get `--port` flags
  injected automatically. Other tools may need manual port wiring.
- **404 page** — name not registered; the page lists all active apps with
  clickable links. Check `portproxy list`.
- **"already registered by a running process"** — name taken; use `--force`
  to take over, or pick another `--name`.
- **508 Loop Detected** — app proxies back into portproxy; fix the app's
  upstream URL.
- **Orphaned port after a crash** — `portproxy prune`.
- **Proxy not reachable from Docker (Caddy)** — set `listen` to a
  non-loopback address in `~/.portproxy/config.toml`.

## Do / Don't

✅ Do:
- Use `portproxy list` / `portproxy get <name>` to discover URLs
- Let portproxy assign ports; reference apps by name
- Use `portproxy alias` for Docker containers and other unwrapped processes
- Use `PORTPROXY=0 <cmd>` when you genuinely need a bare run

❌ Don't:
- Hardcode `localhost:<port>` in configs — ports are ephemeral
- Start dev servers without portproxy on this machine (no route = no URL)
- Run `portproxy proxy start` manually in normal flows — it spawns on demand
- Kill the proxy process directly; it manages its own lifecycle
