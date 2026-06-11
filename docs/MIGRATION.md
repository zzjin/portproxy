# portproxy Migration Guide

> For projects moving from [vercel-labs/portless](https://github.com/vercel-labs/portless)
> (Node.js) or [portless-rs](https://github.com/portless-rs/portless) (Rust).

## What It Is

portproxy assigns stable, named URLs to local development servers that sit behind an
existing reverse proxy such as Caddy or Nginx. Instead of remembering ports, you run:

```sh
cd ~/code/sample-web
portproxy run pnpm dev
# portproxy: sample-web -> 127.0.0.1:4123  (https://sample-web.dev.example.test)
```

It follows the same general idea as the portless family, but with a narrower scope:
portless handles TLS, certificates, and `.localhost` names itself; portproxy leaves
that layer to the upstream Caddy/Nginx proxy and only routes by `Host`.

```text
Browser --https--> Caddy/Nginx (TLS, *.dev.example.test wildcard)
        --http--> portproxy:1355
        --> 127.0.0.1:4xxx
```

Runtime model:

- `portproxy run <cmd>` picks an open port in 4000-4999, injects `$PORT`, writes
  `name -> port` to `~/.portproxy/routes.json`, then runs the command.
- The reverse proxy process starts on demand, has no permanent daemon or systemd
  unit, reloads `routes.json` every 100ms, and exits about 5 seconds after the last
  route disappears.
- Routes are cleaned up by PID liveness, so crashed dev servers recover naturally.
- WebSocket tunneling is supported. `x-portproxy-hops` prevents proxy loops
  (`>5` returns `508`).

## Usage

The binary is installed at `~/.local/bin/portproxy` by default. It is a single
small binary with no runtime dependency.

```text
portproxy run <cmd...>                Run with an inferred name
portproxy <name> <cmd...>             Run with an explicit name
portproxy list                        Show active routes
portproxy get <name> [--no-worktree]  Print a URL without requiring a live route
portproxy alias <name> <port>         Add a static route; --remove deletes it
portproxy prune [--force]             Clean orphaned dev servers
portproxy proxy start|stop            Manually control the proxy
portproxy clean                       Stop proxy and remove all state
```

Run flags:

- `--name <n>` overrides name inference.
- `--force` takes over a live route with the same name and sends `SIGTERM` to the
  old process.
- `--app-port <p>` uses a fixed backend port.

Global config lives at `~/.portproxy/config.toml`; every field is optional:

```toml
# Default: dual-stack loopback (string or array accepted).
# For Docker-hosted Caddy use "[::]:1355" (all interfaces, both stacks)
# or the bridge gateway address.
listen = ["127.0.0.1:1355", "[::1]:1355"]
base_domain = "dev.example.test"     # Only used when printing URLs.
scheme = "https"                     # Only used when printing URLs.
```

With `base_domain` unset, printed URLs fall back to
`http://<name>.localhost:<listen port>`, which already routes through the
proxy — configure a domain only when a fronting Caddy/Nginx serves one.

The dual-stack default exists because `*.localhost` resolves to `::1`
(RFC 6761): server-side fetches to `http://name.localhost:1355` arrive over
IPv6, while Caddy and probes use `127.0.0.1`. With both loopbacks bound,
internal services can keep using `name.localhost:1355` URLs directly — no
`/etc/hosts` entries, no Host-header tricks required.

Example Caddy configuration:

```caddyfile
*.dev.example.test {
    reverse_proxy host.docker.internal:1355   # Or 172.17.0.1:1355.
}
```

### Name Inference

Name inference matches Vercel portless file choices and order:

1. `--name` flag
2. `portproxy.json` in cwd: `{ "name": "..." }`; cwd only, no upward search
3. `package.json` `"portproxy"` key in cwd; string shorthand or `{ "name": ... }`
4. Nearest parent `package.json` `"name"` with any npm scope removed
5. Git repository root directory name from `git rev-parse --show-toplevel`
6. Current directory name

### Monorepos

- Workspaces are discovered from `pnpm-workspace.yaml` or `package.json`
  `workspaces`.
- The project name comes from root `portproxy.json` `name`, then root
  `package.json` `portproxy`, then the majority npm scope across packages
  (`@example/*` -> `example`), then normal root-directory inference.
- A package route is named `<project>-<package-short-name>` such as `example-web`.
  If the package short name already equals the project name, it is not repeated.
- Running bare `portproxy run` at the workspace root starts every package `dev`
  script with independent ports and routes. Build-style dev scripts such as
  `tsc`, `tsup`, `esbuild`, or `* build` run without route registration.
- Root `portproxy.json` can override package names by relative path:
  `{ "apps": { "packages/web": { "name": "frontend" } } }`
- Running bare `portproxy run` inside a package uses the detected package manager
  from lockfiles and runs that package's `dev` script.

### Git Worktree Suffixes

Linked worktrees append the final branch segment inside the same label. This is
not a subdomain prefix.

| Checkout | Branch | Final name |
|---|---|---|
| Main clone | Any branch | `sample-web` |
| Linked worktree | `feature/auth` | `sample-web-auth` |
| Linked worktree | `main` or detached | `sample-web` |

`portproxy get` and `run --name` use the same worktree resolution. Inside a
worktree, `portproxy get sample-web` returns the suffixed URL for that worktree;
the main checkout can run the same base name without collision. `--no-worktree`
skips suffixing. `get` does not require an existing route, so startup scripts can
obtain service URLs before the target service has started.

## Cross-Service Dev Proxies

This is the main migration footgun for projects whose frontend dev server proxies
to another local service. A common example is Vite `server.proxy` forwarding
`/api` to a backend service. Older setups may point to
`http://<backend-name>.localhost:1355` and rely on the operating system resolving
`<backend-name>.localhost`.

portproxy does not modify `/etc/hosts` and does not provide DNS. That layer belongs
to the upstream Caddy/Nginx proxy, but it only helps browser traffic; it does not
affect server-side fetches made by the dev server process. Server-side resolution
of `*.localhost` can therefore fail or hang.

Use the loopback IP as the TCP target and carry the backend route name in the
`Host` header. portproxy routes by the first label of `Host`, so a request to
`127.0.0.1:1355` with `Host: example-api` reaches the `example-api` route without any
DNS lookup.

```ts
// vite.config.ts
export default defineConfig({
  server: {
    proxy: {
      '/api': {
        target: 'http://127.0.0.1:1355',
        changeOrigin: false,
        headers: { host: 'example-api' },
      },
      '/thread-gateway': {
        target: 'http://127.0.0.1:1355',
        ws: true,
        changeOrigin: false,
        headers: { host: 'example-api' },
      },
    },
  },
})
```

Replace `example-api` with the name that portproxy prints for the backend. It is
also visible in `portproxy list`.

In worktrees, backend names can include branch suffixes such as `example-api-auth`.
Avoid hard-coding those names. Use `portproxy get` in the startup script so the
current worktree suffix is applied consistently, even before the backend route is
live:

```jsonc
// package.json
"scripts": {
  "dev": "PROXY_API_HOST=$(portproxy get example-api | sed -E 's#^https?://([^.]+).*#\\1#') portproxy run vite"
}
```

```ts
// vite.config.ts
headers: { host: process.env.PROXY_API_HOST || 'example-api' }
```

Browser access, HMR, and frontend-only projects without cross-service `/api`
proxies do not need this change.

## Migrating From portless

1. Stop the old proxy first: `portless proxy stop`. If a system service was
   installed, also run `portless service uninstall`. Both tools use port `1355` by
   default, so they conflict.
2. Rename project config files where present:
   - `portless.json` -> `portproxy.json`; `name` and `apps` path overrides are
     supported, but turbo-specific fields are not.
   - `package.json` `"portless"` key -> `"portproxy"` key; string shorthand and
     object forms are both supported.
3. Update commands:
   - `portless <cmd>` or `portless run <cmd>` -> `portproxy run <cmd>`
   - Explicit-name form stays the same shape:
     `portless myapp next dev` -> `portproxy myapp next dev`
   - portproxy does not support bare `portproxy <cmd>` because it conflicts with
     the explicit-name form; inferred names must use `run`.
4. Update URLs:
   - `https://myapp.localhost` -> `https://myapp.<base_domain>`
   - Worktree URLs change from `auth.myapp.localhost` to
     `myapp-auth.<base_domain>`.
   - Scripts and agent configs with hard-coded old URLs must be updated.
5. Update environment variables:

   | portless | portproxy |
   |---|---|
   | `PORTLESS=0` | `PORTPROXY=0` |
   | `PORTLESS_STATE_DIR` | `PORTPROXY_STATE_DIR`; defaults to `~/.portproxy` |
   | `PORTLESS_URL` | `PORTPROXY_URL`; `.localhost` fallback URL when `base_domain` is unset |
   | No equivalent | `PORTPROXY_NAME`; final label injected into the child process |
   | `PORTLESS_PORT`, `PORTLESS_TLD`, and other TLS/domain settings | No equivalent; upstream proxy handles this layer |

6. Optionally remove old state with `rm -rf ~/.portless` and remove the old local
   CA from the system trust store if needed. `portless clean` can help with the
   old tool's cleanup.

## Differences From Upstream Projects

| Capability | Vercel portless | portless-rs | portproxy |
|---|---|---|---|
| TLS, local CA, port 443 | Built in | HTTP only | Deliberately delegated to Caddy/Nginx |
| Domains | `*.localhost` plus hosts sync, custom TLD, and mDNS | `*.localhost` | Domain-agnostic; matches only the first `Host` label |
| Long-running daemon | Permanent daemon with optional service install | On demand, exits when idle | Same on-demand model as portless-rs |
| Name inference | Yes | No, name is required | Yes, same file order as Vercel portless |
| Worktree suffixes | Subdomain prefix such as `auth.myapp.localhost` | No | Same-label suffix such as `myapp-auth` |
| Monorepo support | Yes | No | Yes, except turbo-specific integration |
| `alias`, `prune`, `clean` | Yes | No | Yes |
| HTTP/2 | Yes | No | No; browser-facing Caddy can still serve h2/h3 |
| Tailscale, ngrok, LAN sharing | Yes | No | Delegated to the upstream layer |
| Duplicate names | Error, `--force` can take over | Overwrites | Error, `--force` can take over |
| Framework flag injection | Broad set including Expo/RN | vite, react-router, astro, ng | vite, react-router, rsbuild, astro, ng, including npx/pnpm dlx wrappers |
| `X-Forwarded-*` | Generated by portless | Generated by portless-rs | Preserves upstream Caddy values and appends |
| Platforms | macOS, Linux, Windows | Unix | Unix: Linux and macOS |
| Size | Node.js runtime, version 24 or newer | About 1 MB | About 1.3 MB single binary |
| Health probe header | `X-Portless: 1` | `X-Portless: 1` | `X-Portproxy: 1` |

In short: portproxy keeps the small, on-demand implementation style of
portless-rs, adds Vercel portless-style smart naming and worktree discovery, and
removes TLS/domain ownership so it fits environments that already have a
Caddy/Nginx entry point.
