# portproxy

Stable named URLs for local development servers behind a TLS-terminating reverse
proxy such as Caddy or Nginx.

`portproxy` lets tools and AI agents refer to local apps by predictable HTTPS
names instead of changing port numbers:

```text
browser --https--> Caddy/Nginx (*.dev.example.test) --http--> portproxy --> 127.0.0.1:4xxx
```

The npm package installs the prebuilt `portproxy` binary for Linux and macOS.

## Install

```sh
npm install -g @zzjin/portproxy
```

## Usage

```sh
portproxy run <cmd...>             # run with auto-inferred name
portproxy <name> <cmd...>          # run with explicit name
portproxy proxy start              # start proxy manually
portproxy list                     # show active routes
portproxy get <name>               # print URL
portproxy alias <name> <port>      # static route for Docker etc.
portproxy prune                    # kill orphaned dev servers
portproxy clean                    # stop proxy and remove all state
```

Example:

```sh
cd ~/code/sample-web
portproxy run pnpm dev
# portproxy: sample-web -> 127.0.0.1:4123  (https://sample-web.dev.example.test)
```

`portproxy` deliberately does not manage TLS, DNS, certificates, or
`/etc/hosts`. Put Caddy, Nginx, or another TLS-terminating reverse proxy in
front of it.

Full documentation: https://github.com/zzjin/portproxy
