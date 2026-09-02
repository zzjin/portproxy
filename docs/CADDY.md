# Caddy Integration

This guide documents the Caddy configuration used to expose one portproxy
route through two HTTP modes:

| Public URL | Caddy to portproxy | Backend `Host` |
|---|---|---|
| `https://NAME.dev.example.com:54699` | ordinary reverse proxy | `localhost:<app-port>` |
| `https://NAME.origin.dev.example.com:54699` | HTTP absolute-form through `network_proxy` | `NAME.origin.dev.example.com:54699` |

Both URLs resolve `NAME` through the same live portproxy route. The `origin`
label belongs to Caddy's public hostname convention; it is not stored in
portproxy state and does not require a route option or custom header.

## Requirements

- Caddy with the `caddy.network_proxy.url` module. Check with
  `caddy list-modules | grep caddy.network_proxy.url`.
- A DNS provider module when Caddy should obtain public wildcard certificates.
- DNS for both wildcard namespaces pointing to the Caddy entry point.
- Caddy network access to portproxy's HTTP listener, normally port `1355`.

The example below uses the Cloudflare DNS module and a non-standard public TLS
port. Replace the domain, email, token variable, and port for the deployment.

## Complete dual-mode configuration

```caddyfile
{
    email admin@example.com
    auto_https disable_redirects

    servers :54699 {
        listener_wrappers {
            http_redirect
            tls
        }
    }
}

dev.example.com:54699,
*.dev.example.com:54699,
*.origin.dev.example.com:54699 {
    tls {
        dns cloudflare {
            api_token {env.CF_API_TOKEN}
        }
        propagation_delay 30s
        propagation_timeout 3m
        resolvers 1.1.1.1 8.8.8.8
    }

    # Optional direct-port form: https://4123.dev.example.com:54699
    @port {
        header_regexp port Host ^(\d+)\.dev\.example\.com(?::54699)?$
    }

    # Preserved Host: https://NAME.origin.dev.example.com:54699
    @origin_name {
        header_regexp origin_name Host ^[a-z0-9-]+\.origin\.dev\.example\.com(?::54699)?$
    }

    # Normalized Host: https://NAME.dev.example.com:54699
    @name {
        header_regexp name Host ^[a-z0-9-]+(?:\.[a-z0-9-]+)*\.dev\.example\.com(?::54699)?$
    }

    route {
        handle @port {
            reverse_proxy 127.0.0.1:{http.regexp.port.1} {
                header_up X-Forwarded-Host {host}
            }
        }

        # Keep this before @name because @name also accepts nested labels.
        handle @origin_name {
            reverse_proxy {host}:54699 {
                transport http {
                    network_proxy url http://127.0.0.1:1355
                }
            }
        }

        handle @name {
            reverse_proxy http://127.0.0.1:1355
        }

        handle {
            respond "Caddy proxy alive" 200
        }
    }
}
```

The example assumes Caddy runs directly on the host or in a container using
host networking. If Caddy uses a bridge network, replace `127.0.0.1:1355` with
an address that reaches the host, such as `host.docker.internal:1355`, and bind
portproxy to an address reachable from that network.

## Why the origin label is a separate DNS level

portproxy routes an absolute-form request by the first label of its URI
authority. These two hosts therefore select the same `sample-web` route:

```text
sample-web.dev.example.com
sample-web.origin.dev.example.com
^^^^^^^^^^
route label
```

Avoid a form such as `sample-web-origin.dev.example.com` unless the registered
route is actually named `sample-web-origin`.

The existing `*.dev.example.com` certificate cannot cover
`sample-web.origin.dev.example.com`: a WebPKI wildcard covers exactly one
left-most label. Listing `*.origin.dev.example.com` in the Caddy site address
causes Caddy to manage the additional wildcard certificate through the
configured DNS challenge.

## Why the dynamic upstream has no scheme

Caddy does not allow runtime placeholders in an upstream URL that contains a
scheme, so this is invalid:

```caddyfile
reverse_proxy http://{host}
```

Use a dynamic network address with an explicit port instead:

```caddyfile
reverse_proxy {host}:54699 {
    transport http {
        network_proxy url http://127.0.0.1:1355
    }
}
```

The HTTP transport sends an absolute-form target to portproxy. portproxy uses
only the first authority label to select a registered loopback route; it never
resolves the public hostname or uses the authority's port as the backend
connection port. Before delivery to the app, it converts the target to
origin-form and preserves the absolute authority as `Host`.

## Validation

Validate and hot-reload a mounted Caddyfile with:

```bash
caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile
caddy reload --config /etc/caddy/Caddyfile --adapter caddyfile
```

For one registered route, request both URLs and verify that they reach the same
app. A diagnostic backend which echoes `Host` should observe:

```text
# https://sample-web.dev.example.com:54699
localhost:<app-port>

# https://sample-web.origin.dev.example.com:54699
sample-web.origin.dev.example.com:54699
```

Request bodies, response streaming, and WebSocket upgrades use the same mode
selection and route lookup paths. No Caddy-specific mode header is required.
