# portproxy 

>  [vercel-labs/portless](https://github.com/vercel-labs/portless)Node 
> [portless-rs](https://github.com/portless-rs/portless)Rust 

## 

portproxy **Caddy/Nginx dev server**  URL 


```sh
cd ~/code/sample-web
portproxy run pnpm dev
# portproxy: sample-web -> 127.0.0.1:4123  (https://sample-web.dev.example.test)
```

 portless **portless  TLS//.localhost 
portproxy  Caddy/Nginx** Host 

```
 ──https──> Caddy/NginxTLS*.dev.example.test ──http──> portproxy:1355 ──> 127.0.0.1:4xxx
```

 portless-rs 

- `portproxy run <cmd>`  4000–4999  `$PORT` 
  ` -> `  `~/.portproxy/routes.json`
- **** daemon systemd  100ms 
  routes.json 5 ****
-  PID 
-  WebSocket `x-portproxy-hops` >5 → 508

## 

 `~/.local/bin/portproxy`1.3M 

```
portproxy run <cmd...>             
portproxy <name> <cmd...>          
portproxy list                     
portproxy get <name> [--no-worktree]   URL worktree 
portproxy alias <name> <port>      Docker --remove 
portproxy prune [--force]           dev server
portproxy proxy start|stop         
portproxy clean                     + 
```

 flag`--name <n>``--force` SIGTERM
`--app-port <p>`

 `~/.portproxy/config.toml`

```toml
listen = "0.0.0.0:1355"          # Docker  Caddy  loopback
base_domain = "dev.example.test"   #  get/list  URL
scheme = "https"                 # 
```

Caddy  Caddy 

```caddyfile
*.dev.example.test {
    reverse_proxy host.docker.internal:1355   #  172.17.0.1:1355
}
```

###  Vercel portless 

1. `--name` flag
2. cwd  `portproxy.json``{ "name": "..." }` cwd
3. cwd  `package.json` `"portproxy"` key `{ "name": ... }` cwd
4.  `package.json` `"name"` `@scope/`
5. git `git rev-parse --show-toplevel`
6. 

### Monorepo Vercel portless 

-  workspace`pnpm-workspace.yaml` / package.json `workspaces`
-  =  `portproxy.json` `name` →  package.json `portproxy` key →
   npm scope `@example/*` → `example`→ 
-  `<>-<>``example-web`
- **workspace  `portproxy run`  `dev` script**
  build  dev scripttsc/tsup/esbuild/`* build` 
-  `portproxy.json` 
  `{ "apps": { "packages/web": { "name": "frontend" } } }`
-  `portproxy run` = lockfile  pnpm/yarn/bun/npm `dev` script

### Git worktree 

linked worktree  label ****

| checkout |  |  |
|---|---|---|
|  clone |  | `sample-web` |
| linked worktree | `feature/auth` | `sample-web-auth` |
| linked worktree | `main` / detached | `sample-web` |

##  portless 

1. ****`portless proxy stop` `portless service uninstall`
    1355
2. ****
   - `portless.json` → `portproxy.json` `name`  `apps` turbo 
   - `package.json`  `"portless"` key → `"portproxy"` key
3. ****`portless <cmd>` / `portless run <cmd>` → `portproxy run <cmd>`
    `portless myapp next dev` → `portproxy myapp next dev` 
   portproxy **** `portproxy <cmd>` `<name>`  `run`
4. **URL **`https://myapp.localhost` → `https://myapp.<base_domain>`
   worktree URL  `auth.myapp.localhost` `myapp-auth.<base_domain>`
    label  URL /agent 
5. ****

   | portless | portproxy |
   |---|---|
   | `PORTLESS=0` | `PORTPROXY=0` |
   | `PORTLESS_STATE_DIR` | `PORTPROXY_STATE_DIR` `~/.portproxy` |
   | `PORTLESS_URL` | `PORTPROXY_URL` base_domain  |
   | — | `PORTPROXY_NAME` label |
   | `PORTLESS_PORT` / `PORTLESS_TLD`  TLS/ |  |

6. ****`rm -rf ~/.portless` CA
   `portless clean` 

## 

|  | Vercel portless | portless-rs | **portproxy** |
|---|---|---|---|
| TLS /  CA / 443 | ✅  | ❌  HTTP | ❌  Caddy/Nginx  |
|  | `*.localhost`+hosts / TLD/mDNS | `*.localhost` | **** Host  label |
|  daemon |  +  |  |  portless-rs10s  + 5s  |
|  | ✅ | ❌ | ✅  Vercel / |
| worktree  | ✅  `auth.myapp.localhost` | ❌ | ✅  label  `myapp-auth` label  |
| monorepoworkspace /scope //build /apps  | ✅ | ❌ | ✅turbo  |
| alias / prune / clean | ✅ | ❌ | ✅ |
| HTTP/2 | ✅ | ❌ | ❌HTTP/1.1Caddy  h2/h3 |
| Tailscale / ngrok / LAN  | ✅ | ❌ | ❌ |
|  | `--force`  |  |  Vercel + `--force` |
|  flag  |  Expo/RN | vite/react-router/astro/ng | vite/react-router/rsbuild/astro/ng npx/pnpm dlx  |
| X-Forwarded-* |  |  | ** Caddy ** |
|  | mac/Linux/Windows | Unix | UnixLinux/macOS |
|  | Node ≥24  | ~1MB | 1.3M  |
|  | `X-Portless: 1` | `X-Portless: 1` | `X-Portproxy: 1` |

** portless-rs  Vercel portless 
worktree  TLS/" Caddy/Nginx"**
