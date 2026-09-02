# portproxy Absolute-Form Proxy Plan

Date: 2026-09-02
Status: Implemented. Core Caddy runtime integration was validated with Caddy
v2.11.4; full Vite/HMR acceptance coverage remains pending.

## Goal

Support two request-scoped HTTP modes without adding route fields, CLI flags,
configuration keys, hostname conventions, or custom headers:

- ordinary requests use the existing reverse-proxy behavior and normalize the
  backend `Host` to `localhost:<backend-port>`;
- absolute-form HTTP requests preserve their URI authority as the backend
  `Host`.

Caddy selects the mode by choosing how it sends the request. portproxy infers
the mode only from the standard HTTP request-target form.

## Scope

In scope:

- HTTP/1.1 ordinary request targets such as `/path?query`;
- HTTP/1.1 absolute-form targets such as `http://app.example/path`;
- HTTP requests and WebSocket Upgrade requests in both modes;
- route lookup, header forwarding, hop detection, and existing error handling;
- Caddy integration and end-to-end validation.

Out of scope:

- `CONNECT` and TCP tunneling;
- HTTPS absolute-form targets;
- general-purpose forward proxying;
- external DNS resolution;
- proxy authentication;
- custom mode-selection headers;
- route or persistent-state schema changes.

## Layer Boundaries

### Caddy

Caddy owns:

- public hostnames and hostname conventions;
- TLS termination;
- ingress matching and access control;
- choosing ordinary reverse proxying or HTTP forward-proxy transport;
- public request metadata in `X-Forwarded-*` headers.

Caddy expresses the selected mode through the request-target form. It does not
send a portproxy-specific control header.

### portproxy

portproxy owns:

- distinguishing ordinary and absolute-form requests;
- extracting the route label from the mode's authoritative host source;
- resolving the label through the live route registry;
- connecting to the registered loopback backend;
- converting absolute-form targets to origin-form before backend delivery;
- applying the mode-specific backend `Host`;
- forwarding HTTP bodies and WebSocket traffic;
- forwarding metadata, hop detection, and proxy-generated errors.

portproxy does not:

- interpret Caddy hostname conventions;
- decide which public hostname requires preserved Host behavior;
- store a Host mode in a route;
- resolve or dial the authority supplied by an absolute URI;
- terminate TLS.

### Application backend

The backend always receives an origin-server request target:

```http
GET /path?query HTTP/1.1
```

Its `Host` depends on the mode:

- ordinary mode: `localhost:<backend-port>`;
- absolute-form mode: the absolute URI authority selected by Caddy.

The backend does not receive an absolute-form request target or a mode-control
header.

## Mode Selection

### Ordinary mode

Input:

```http
GET /path?query HTTP/1.1
Host: app.dev.example.test
```

Behavior:

1. Use `Host` as the route authority.
2. Extract its first DNS label: `app`.
3. Resolve `app` through `routes.json`.
4. Connect to the route's registered loopback port.
5. Forward the original path and query.
6. Set backend `Host` to `localhost:<backend-port>`.

Backend request:

```http
GET /path?query HTTP/1.1
Host: localhost:4173
X-Forwarded-Host: app.dev.example.test
```

An asterisk-form request such as `OPTIONS *` follows ordinary mode: it routes
by `Host` and preserves `*` as the backend request target.

### Absolute-form mode

Input:

```http
GET http://app.dev.example.test/path?query HTTP/1.1
Host: app.dev.example.test
```

Behavior:

1. Recognize the absolute-form request target.
2. Accept only the `http` scheme.
3. Use the absolute URI authority as the sole route and Host authority.
4. Ignore the received `Host` value for routing and backend Host generation.
5. Extract the authority's first DNS label: `app`.
6. Resolve `app` through `routes.json`.
7. Connect to the route's registered loopback port.
8. Convert the target to origin-form `/path?query`.
9. Set backend `Host` from the absolute URI authority.
10. Preserve the remaining end-to-end request data.

Backend request:

```http
GET /path?query HTTP/1.1
Host: app.dev.example.test
X-Forwarded-Host: app.dev.example.test
```

No comparison between the received `Host` and the URI authority is required.
The URI authority is authoritative for an absolute-form request.

## Route and Connection Contract

Both modes use the existing route registry:

```json
[
  { "hostname": "app", "port": 4173, "pid": 12345 }
]
```

Connection rules:

1. Normalize the selected authority's first DNS label.
2. Require an exact match in the live route map.
3. Obtain the backend port exclusively from the matched route.
4. Connect to `127.0.0.1:<route.port>`, with the existing `::1` fallback.
5. Never resolve the selected authority through DNS.
6. Never use an authority-supplied port as the TCP destination.
7. Never connect to a destination not represented by a live route.

An explicit authority port remains part of the backend-visible Host in
absolute-form mode, but it does not affect the backend connection.

## Header Contract

Both modes share one forwarding-header path.

| Header | Ordinary mode | Absolute-form mode |
|---|---|---|
| `Host` | `localhost:<backend-port>` | Absolute URI authority |
| `X-Forwarded-Host` | Preserve upstream value; otherwise original public Host | Preserve upstream value; otherwise absolute URI authority |
| `X-Forwarded-Proto` | Preserve upstream value; otherwise `http` | Preserve upstream value; otherwise `http` |
| `X-Forwarded-For` | Append immediate peer | Append immediate peer |
| `X-Forwarded-Port` | Preserve or derive from public authority/protocol | Preserve or derive from public authority/protocol |
| `X-Portproxy-Hops` | Increment and enforce the existing limit | Increment and enforce the existing limit |

Preserve end-to-end and browser security headers without mode-specific
rewriting, including:

- `Origin`;
- `Referer`;
- `Cookie`;
- `Authorization`;
- `Sec-Fetch-*`;
- `Sec-WebSocket-*`.

Remove immediate-proxy headers before backend delivery:

- `Proxy-Authorization`;
- `Proxy-Connection`.

## WebSocket Contract

WebSocket requests use the same mode selection and routing rules as ordinary
HTTP requests.

Ordinary mode:

- route by `Host`;
- send `Host: localhost:<backend-port>` in the backend handshake.

Absolute-form mode:

- route by absolute URI authority;
- convert the backend handshake target to origin-form;
- send the absolute URI authority as backend `Host`.

Both modes preserve `Origin` and WebSocket negotiation headers, apply the same
forwarding metadata and hop limit, validate the backend `101` response, and
then relay bytes bidirectionally.

## Caddy Integration

### Normalized Host

Caddy uses ordinary reverse proxying to portproxy:

```text
client
  -> Caddy reverse_proxy
  -> origin-form request
  -> portproxy ordinary mode
  -> backend Host normalized to localhost
```

### Preserved Host

Caddy uses portproxy as the HTTP forward proxy for the selected HTTP authority:

```text
client
  -> Caddy reverse_proxy HTTP transport
  -> absolute-form request through portproxy
  -> portproxy absolute-form mode
  -> backend Host preserved from URI authority
```

Caddy owns the matcher that selects either path. portproxy contains no Caddy
hostname rule and no Caddy-specific mode configuration.

## Errors and Security

| Condition | Result |
|---|---|
| Missing or invalid ordinary-mode `Host` | `400 Bad Request` |
| Malformed absolute URI or missing authority | `400 Bad Request` |
| Absolute URI scheme other than `http` | `400 Bad Request` |
| `CONNECT` request | `405 Method Not Allowed` |
| Unknown route label | Existing styled `404 Not Found` |
| Hop limit reached | `508 Loop Detected` |
| Registered backend unavailable | `502 Bad Gateway` |

Security invariants:

- absolute-form mode is a route-aware local proxy, not an open forward proxy;
- the URI authority selects only a route label, never a network destination;
- requested authority ports never control backend dialing;
- backend connections remain restricted to registered loopback ports;
- `CONNECT` and HTTPS forward targets are rejected;
- any peer that can directly reach portproxy can submit either request form;
  network access remains controlled by the configured listeners and ingress.

## Implementation Plan

### 1. Classify and normalize request targets

- [x] Distinguish absolute-form from existing ordinary requests.
- [x] Parse and validate an absolute HTTP authority.
- [x] Treat the absolute URI authority as authoritative without comparing it to
      the received `Host`.
- [x] Reject malformed targets, non-HTTP schemes, and `CONNECT`.
- [x] Keep asterisk-form as an ordinary-mode compatibility case.

### 2. Prepare and route backend requests

- [x] Route ordinary requests from `Host`.
- [x] Route absolute-form requests from URI authority.
- [x] Convert absolute-form targets to origin-form.
- [x] Apply the correct backend `Host` for each mode.
- [x] Guarantee that only the registered route port controls backend dialing.
- [x] Preserve existing bodies, streaming, 404, 502, and response stamping.

### 3. Unify forwarding metadata

- [x] Use one header preparation path for HTTP and WebSocket in both modes.
- [x] Preserve or append `X-Forwarded-*` values consistently.
- [x] Add or verify `X-Forwarded-Port` handling.
- [x] Increment `X-Portproxy-Hops` in both modes.
- [x] Strip immediate-proxy headers.
- [x] Verify browser security and application headers remain unchanged.

### 4. Support absolute-form WebSocket Upgrade

- [x] Route the Upgrade from absolute URI authority.
- [x] Convert the backend handshake target to origin-form.
- [x] Preserve the authority as backend `Host`.
- [x] Apply the shared headers and hop limit.
- [x] Validate `101` and bidirectional relay behavior.

### 5. Validate Caddy integration

- [x] Verify ordinary Caddy reverse proxying keeps normalized Host behavior.
- [x] Verify Caddy's HTTP forward-proxy transport produces absolute-form
      requests accepted by portproxy.
- [x] Verify the same route works concurrently through both modes.
- [ ] Verify request bodies, query strings, streaming, redirects, and errors.
- [ ] Verify a real Vite page and HMR WebSocket through both modes.
- [x] Document both Caddy integration forms in `README.md` and
      `docs/MIGRATION.md`.

### 6. Validate routing security and regressions

- [x] Prove unknown absolute authorities return 404 without DNS or external
      connection attempts.
- [x] Prove an authority-supplied port cannot change the backend connection.
- [x] Prove the received `Host` does not alter absolute-form routing.
- [x] Prove non-HTTP absolute targets and CONNECT are rejected.
- [x] Prove hop detection works in both modes.
- [x] Run unit, integration, existing E2E, and browser-level checks.

## Acceptance Criteria

- Existing ordinary requests retain the current Host-normalization behavior.
- Absolute-form HTTP requests use URI authority for route lookup and backend
  Host without consulting the received `Host`.
- Backend applications always receive origin-form request targets.
- The same route supports both modes without state or configuration changes.
- Caddy selects the mode through standard HTTP transport behavior.
- portproxy never resolves or dials a requested public authority.
- HTTP bodies, streaming, forwarding metadata, hop detection, and WebSocket HMR
  work in both modes.
- Existing route files require no migration.
