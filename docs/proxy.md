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

Routes and certificates are rebuilt from replicated state every five seconds.
A route with no running backend is still published and answers 502, since a
redeploy briefly having nothing behind it is not the same as the hostname not
being configured, and a 404 is what a client would cache.

## Routing

A route is built from one `ProxyRoute` entry (`host`, `network`, `port`,
`path_prefix`, `tls`, `middlewares`, `sticky`) plus the overlay address of
every running replica of the service instance. The routing table
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

`basic_auth` credentials and the `headers` additions/removals are not yet
modeled in `cl-config` (`Middleware` there is a bare enum tag with no
per-route configuration payload). The pure functions that implement both
(`check_basic_auth`, `apply_headers`, plus their response helpers) are fully
implemented and unit tested in `middleware.rs`, but until a config channel
for their parameters exists, the proxy wires them as no-ops: a route that
lists `basic_auth` or `headers` is accepted but currently enforces nothing
extra. This is a known gap, not an oversight, tracked against the day
`ProxyRoute` grows per-middleware config.

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
3. `cl_dns::await_propagation` polls Cloudflare's own API until that record
   is visible, then the challenge is marked ready.
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
