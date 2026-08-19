# Reverse proxy, TLS and ACME

`cl-proxy` terminates HTTP and HTTPS for every route declared under a
service's `x-clouduler.proxy` (see `docs/stacks.md`), forwards to the
container backend over the overlay network, and issues/renews the
certificates those routes need via ACME DNS-01 through `cl-dns`.

## Ports and configuration

```yaml
proxy:
  enabled: true
  http_port: 80
  https_port: 443
  acme:
    enabled: false
    directory_url: https://acme-staging-v02.api.letsencrypt.org/directory
    contact_email: ops@example.com
```

Both listeners are bound on every node, on all addresses, because a route's
hostname resolves to whichever node publishes it and that changes as
containers move. Neither may collide with `web.port`, which the api holds on
the same addresses; the config validator refuses that outright rather than
leaving a listener that silently failed to bind. This is why `web.port`
defaults to 8443 and not 443: the standard ports belong to the services the
cluster publishes, and the admin ui takes the odd one.

## The dashboard

The dashboard is published as a route in the same table as everything else,
controlled by `web.through_proxy` (default on):

```yaml
web:
  address: cloud
  network: tailscale
  port: 8443           # direct https listener, always up
  through_proxy: true  # also publish through the reverse proxy
  backend_port: 8444   # loopback port the proxy forwards to
```

The point is that it then gets the Let's Encrypt certificate and the
Cloudflare record from the machinery that already runs for every service,
rather than needing its own path for each. On the standard HTTPS port it
serves a real certificate; the direct `web.port` listener falls back to a
self-signed one until ACME has issued.

Three things about this are deliberate:

- **The direct listener on `web.port` stays up regardless.** It is the way
  back in when the proxy cannot bind its ports (they are below 1024 and need
  root) or when a config edit breaks routing. Locking the operator out of the
  ui that fixes the proxy would be the worst available failure mode.
- **The backend is this node's own api over loopback**, not a load balanced
  pool across the cluster. The proxy speaks plain HTTP to its backends, and
  that hop carries session cookies while the overlay is not encrypted, so it
  must not leave the machine. A non-leader api already redirects to the
  leader, so spreading the load would only add a redirect anyway.
- **The route sets `preserve_host` and no timeouts.** The api derives its
  OIDC redirect URL from the request host, and `/api/events` plus the log and
  stats streams are server-sent events that must not be capped or timed out.

`backend_port` may not collide with `web.port` or either proxy port; the
validator refuses all three.

Routes and certificates are rebuilt from replicated state every five seconds.
A route with no running backend is still published and answers 502, since a
redeploy briefly having nothing behind it is not the same as the hostname not
being configured, and a 404 is what a client would cache.

## Routing

A route is built from one `ProxyRoute` entry (`host`, `network`, `port`,
`path_prefix`, `tls`, `middlewares`, `sticky`) plus the overlay address of
every running replica of the service instance. Per-route behaviour that is
about neither matching nor backend choice — `max_body_size`, `websockets`,
`preserve_host`, `response_timeout` — is carried separately in
`RouteOptions`, so adding a knob does not touch every `Route::new` call
site. The routing table
(`RouteTable`) is rebuilt wholesale on every placement change and swapped in
atomically (`arc_swap`), so request handling never blocks on a reload and
never sees a half-updated table.

### Matching precedence

Given a request's `Host` header (port suffix ignored) and path:

1. **Exact host beats wildcard host.** If any route is registered for the
   exact host, only routes for that host are considered, even if a wildcard
   route would also match.
2. **Wildcard hosts** are written `*.example.com` and match exactly one extra
   label: `foo.example.com` matches, `example.com` and `a.b.example.com` do
   not. When multiple wildcard bases match a host (e.g. `*.example.com` and
   `*.internal.example.com` both matching `x.internal.example.com`), the
   longest base wins.
3. **Longest `path_prefix` wins** among the routes that matched the host. A
   route with no `path_prefix` matches every path under its host, and counts
   as prefix length zero, so it only wins when nothing more specific matches.

No match returns 404.

### Backends and load balancing

A route carries one backend address per running replica. Requests round-robin
across them. When `sticky` is set, the proxy remembers the chosen backend in
a `cl_sticky` cookie (the backend's socket address, so pinning survives a
table reload as long as that backend is still in the pool); a request
carrying a valid `cl_sticky` cookie goes straight to that backend without
consuming a round-robin slot. If the cookied backend is no longer in the
pool, the proxy falls back to round robin and issues a new cookie.

## Static/external backends

Most routes come from a stack's `x-clouduler.proxy` and follow its containers
automatically. A route for something clouduler does not run at all — another
box on the network, a third party endpoint — is managed entirely from the web
ui instead, not `cluster.yaml`: like a redirect, one of these can be added or
changed without a daemon restart, since it lives in replicated state under a
reserved config name (see `cl_api::static_routes`) rather than the config
file every daemon has to be restarted to pick up. It is exposed at
`/api/static-routes` (`GET`/`POST`/`PUT /{id}`/`DELETE /{id}`, admin only to
write).

A rule is a `host` (optionally with a `path_prefix`) on a given `network`, and
a `backend` (`host:port`); `tls`, `middlewares`, `max_body_size`,
`websockets`, `preserve_host` and `response_timeout` all work the same as on
a stack's proxy route.

`backend`'s host is resolved fresh on every reconcile pass (every 5s), so a
DNS change on the backend takes effect without editing anything, and every
address it resolves to becomes a backend, round robined the same way multiple
replicas of a service are.

## Network gating

Both listeners bind `0.0.0.0` on every node, since a route's hostname can
resolve to any node. That means a route published on a `private` (or
`internal`) network would otherwise be reachable from any interface a node
has, including a public one. To prevent that, every route built from a
network with a subnet (`internal` or `private`) only accepts a connection
whose real TCP peer address falls inside that subnet; anything else gets
`403`. A route on a `public` network has no subnet and so no restriction —
it is meant to be reachable from anywhere.

This is a check on the actual socket peer, not a header, so it cannot be
spoofed by a client claiming a different address. It applies uniformly to
stack routes, static backends, redirects and the dashboard. There is no
per-route opt-out and no forward-auth layer yet; both are deliberately out of
scope for now.

## Redirects

Simple host/URL redirects are managed entirely from the web ui, not
`cluster.yaml` — they are expected to change often enough that editing the
config file for each one would be annoying. A redirect rule is a source
`host` (optionally with a `path_prefix`) on a given `network`, a destination
`target` URL, whether to append the original path and query
(`preserve_path`), and a status code (`301`/`302`/`307`/`308`). A matching
request gets a `Location` response built from the rule and never reaches a
backend at all. Rules are stored in replicated state under a reserved config
name (see `cl_api::redirects`) and exposed at `/api/redirects`
(`GET`/`POST`/`PUT /{id}`/`DELETE /{id}`, admin only to write).

## Middlewares

Applied in this fixed order, regardless of the order they're listed in the
stack file:

1. **`redirect_https`** — not actually part of the per-request middleware
   chain. Driven directly by `route.tls`: the plain HTTP listener 308-redirects
   any request for a `tls: true` route to the same host and path over https,
   before matching against `middlewares` at all. 308 preserves the method and
   body, unlike 301/302.
2. **`strip_prefix`** — removes the route's own `path_prefix` from the request
   path before it reaches the backend. `/api/things` on a route with
   `path_prefix: /api` becomes `/things`.
3. **`basic_auth`** — challenges the request with HTTP basic auth, short
   circuiting with 401 on failure.
4. **`headers`** — adds or removes headers on the proxied request.
5. **`cors`** — see below. Answers a preflight directly; adds
   `Access-Control-Allow-*` to a real request's response when its `Origin` is
   allowed.

`basic_auth` credentials and the `headers` additions/removals are not yet
modeled in `cl-config` (`Middleware` there is a bare enum tag with no
per-route configuration payload). The pure functions that implement both
(`check_basic_auth`, `apply_headers`, plus their response helpers) are fully
implemented and unit tested in `middleware.rs`, but until a config channel
for their parameters exists, the proxy wires them as no-ops: a route that
lists `basic_auth` or `headers` is accepted but currently enforces nothing
extra. This is a known gap, not an oversight, tracked against the day
`ProxyRoute` grows per-middleware config.

### CORS

Unlike `basic_auth`/`headers`, `cors` does carry its settings and is fully
enforced, since cross-origin access control is a property of the stack's own
application. Declared on a service's proxy route in the stack file:

```yaml
x-clouduler:
  proxy:
    - host: api
      network: wan
      port: 80
      middlewares:
        - cors:
            origins: ["https://app.example.com"]
            methods: [GET, POST]        # default: GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS
            headers: []                 # empty echoes the browser's Access-Control-Request-Headers
            allow_credentials: false
            max_age: 10m
```

An `OPTIONS` preflight (one carrying `Access-Control-Request-Method`) is
answered directly with the allow headers and never reaches the backend. A
real request gets the same headers added to whatever the backend returned,
when its `Origin` matches (`origins: ["*"]` matches any). `allow_credentials`
cannot be combined with a wildcard origin — browsers reject that pairing, so
it is rejected at stack validation time instead of failing confusingly at
request time.

## Forwarded headers

Every proxied request gets:

- `X-Forwarded-For` — the connecting peer's IP, appended to any existing
  value already on the request (so a chain of proxies accumulates correctly).
- `X-Forwarded-Proto` — `http` or `https`, whichever listener accepted the
  connection.
- `X-Forwarded-Host` — the original `Host` header the client sent, before it
  gets overwritten with the backend's address for the outbound request.

Hop-by-hop headers (`Connection`, `Keep-Alive`, `Proxy-Authenticate`,
`Proxy-Authorization`, `TE`, `Trailer`, `Transfer-Encoding`, `Upgrade`, plus
anything named in an incoming `Connection` header) are stripped in both
directions, except on a websocket (or other) upgrade request, where
`Connection`/`Upgrade` are preserved so the handshake can complete.

## WebSocket and other upgrades

A request with `Connection: Upgrade` and an `Upgrade` header is proxied
differently: the proxy opens its own HTTP/1.1 connection to the backend,
sends the request with the upgrade headers intact, and if the backend
responds `101 Switching Protocols`, takes both the client and backend
connections out of HTTP handling (`hyper::upgrade::on`) and splices the two
raw byte streams together with `tokio::io::copy_bidirectional`. If the
backend does not upgrade, its normal response is relayed as-is.

## DNS records

Every hostname the reverse proxy answers for — stack routes, static
backends, redirects — plus the web ui's own address needs an actual DNS
record pointing somewhere, or nothing but a certificate exists for it.
`clouduler::proxy::run_dns_reconcile` publishes these automatically: every 60
seconds, on the leader only, it computes the desired record set from
`ClusterConfig` and replicated state (`cl_dns::records_for`/`plan`/`apply`),
diffs it against each Cloudflare zone's live records, and converges. Gated
only on a `cloudflare` section with a readable token being present — unlike
certificate issuance, it does not additionally require `proxy.acme.enabled`,
since a plain A record has nothing to do with TLS.

Because every node runs the reverse proxy and forwards internally, a
hostname's records point at **every** node's address on the declared
network, not just the node a stack's containers happen to be placed on —
`n1.example.com` failing does not need `blog.example.com`'s DNS to change
for `n2` to keep answering it. Records are tagged with an ownership TXT
record the same way `cl_dns::reconcile` always has, so a zone can be shared
with records clouduler doesn't manage.

Applied changes are also written to replicated state
(`Request::UpsertDnsRecord`/`RemoveDnsRecord`), which is what the ui's
Certificates & DNS page reads (`GET /api/dns`) — that page was reading real
state from day one, but nothing was ever writing to it until this loop
existed to do so.

## Certificates

`CertStore` holds one parsed `rustls::sign::CertifiedKey` per fqdn (exact or
`*.domain` wildcard), loaded from `cl_raft::CertificateState` records and
swapped in atomically on every raft state change. Certificates are
distributed through raft, not issued per node, so any node can terminate TLS
for any route regardless of which node holds the raft leadership at a given
moment.

The HTTPS listener uses one shared `rustls::ServerConfig` whose
`ResolvesServerCert` implementation (`CertResolver`) picks the certificate by
SNI: exact fqdn first, then the longest matching wildcard base. Before
completing a handshake, the listener peeks the ClientHello (via
`tokio_rustls::LazyConfigAcceptor`) to read the SNI name and calls
`CertStore::resolve` for it. If nothing has been issued for that name yet,
`resolve` generates and caches a self-signed certificate on the spot (logged
at `warn` when generated, so a self-signed cert in front of real traffic is
never silent) and the handshake proceeds; `ResolvesServerCert::resolve`
itself only ever needs to do a synchronous lookup after that, since the
fallback is already cached.

### Issuance

`acme::issue_certificate` drives one ACME order to completion over DNS-01:

1. Submit a `NewOrder` for the given names (which may include a `*.domain`
   wildcard entry; instant-acme submits it to the CA as `*.domain` and the
   CA authorizes the base domain with a wildcard flag).
2. For each pending authorization, fetch the `dns-01` challenge, compute its
   key authorization digest, and call `cl_dns::set_challenge` to create the
   `_acme-challenge.<domain>` TXT record.
3. `cl_dns::await_propagation` confirms the record is visible two ways before
   the challenge is marked ready: first a quick poll of Cloudflare's own API
   (fails fast if the record was never created), then a poll of the zone's
   actual authoritative nameservers, resolved fresh and queried directly
   rather than through a possibly cache-poisoned recursive resolver. The
   first check alone used to be treated as "propagated," but a Cloudflare API
   write can land in their control plane several seconds before it's visible
   on the edge network the CA actually queries, which read as a real,
   repeatable "order not ready: Invalid" failure — Let's Encrypt had already
   attempted and failed the DNS-01 lookup by the time we asked it to.
4. Once every authorization is satisfied, poll the order to `Ready`,
   finalize it (which generates the certificate key and CSR internally via
   `instant-acme`), and poll for the issued chain.
5. Every TXT record created during the order is cleared via
   `cl_dns::clear_challenge`, whether issuance succeeded or failed, so a
   failed order never leaves stray challenge records behind.

The leaf certificate's `notAfter` is parsed out of the returned chain
(`x509-parser`) and stored alongside the PEM chain and key in a
`CertificateState`, written to raft via `Request::StoreCertificate`.

The ACME directory URL is configurable (`AcmeConfig::directory_url`).
`AcmeConfig::default()` points at the Let's Encrypt production directory;
`LETS_ENCRYPT_STAGING_URL` is exposed as a named constant for tests and the
qemu harness, which should point at a local Pebble server via a custom
`directory_url` instead of either Let's Encrypt endpoint.

### What is issued, and by whom

Issuance and renewal run only on the leader, and only when `proxy.acme.enabled`
is set and a Cloudflare token is readable. Several nodes ordering the same
certificate at once would burn the CA's rate limit and race each other writing
the result back. Every five minutes the leader:

1. orders a certificate for each `tls: true` route hostname, and for the web
   ui's own fqdn, that has none stored yet;
2. runs `renewal_tick` over everything that does have one.

Cloudflare zone ids are resolved once at startup from `cloudflare.zones`, since
`ZoneResolver` is synchronous and cannot look one up mid-order. The longest
matching zone wins, so a delegated subdomain zone is preferred over its parent.

The api serves the ui with the stored certificate for its own fqdn when there
is one, and generates a self-signed one when there is not, which is what makes
a brand new cluster reachable before anything has been issued.

### Account

`acme::load_or_create_account` persists ACME account credentials
(`instant_acme::AccountCredentials`, which embeds the account's private key
and directory URL) as JSON at a configured path, creating a new account only
when that file doesn't exist yet.

### Renewal

`acme::renewal_tick` runs one pass over the current raft leader's certificate
store: any certificate within `RENEWAL_WINDOW` (30 days) of `not_after` is
reissued and written back with `Request::StoreCertificate`. It is a no-op
on a follower, so it's safe to call unconditionally from every node's timer;
`acme::run_renewal_loop` does exactly that on a fixed interval. Zone
selection (which Cloudflare zone a given fqdn's challenge record belongs in)
is supplied by the caller through the `ZoneResolver` trait, since that
mapping lives in the cluster config, which this crate does not depend on.
