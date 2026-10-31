# Reverse proxy, TLS and ACME

`cl-proxy` terminates HTTP and HTTPS for every route declared under a
service's `x-clouduler.proxy` (see `docs/stacks.md`), forwards to the
container backend (directly when this node runs it, through the owning
node's relay otherwise, see "Reaching a container on another node from a
host" in `docs/networking.md`), and issues/renews the certificates those
routes need via ACME DNS-01 through `cl-dns`.

## Ports and configuration

```yaml
proxy:
  enabled: true
  http_port: 80
  https_port: 443
  dns_publish: all-nodes # or owning-node
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

### Host names

A bare `host` (`blog`) is qualified with the network's `domain`; a dotted one
is taken as a full name. `host: "@"` publishes the apex, the network's domain
itself. Quote it: a bare `@` is reserved in YAML. DNS needs an A/AAAA record
at the apex, or a CNAME that the DNS provider flattens (Cloudflare does).

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
a stack's proxy route. An optional `node` pins the host's DNS records to that
one node's address, so requests are routed through it (useful when only that
node can reach the backend); left unset, every node is published.

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

### Login (`authenticated`)

Protects an app that has no login of its own, optionally for some groups only.

```yaml
# cluster.yaml
proxy:
  auth:
    host: login            # portal label, one hostname per network below
    networks: [wan, lan]   # networks that offer the login (login.<wan domain>, login.<lan domain>)
    issuer: https://sso.example.com/application/o/apps/
    client_id: clouduler-apps
    groups_claim: groups   # default
    session_ttl: 7d        # how long a login lasts (default 12h)
    title: Sign in to continue
    palette: grape         # login page colours: grape, sunset, ocean, forest
    accent: "#e5603c"      # optional: overrides the palette's main colour
    remember_ip: [public]  # networks whose login page offers "remember this IP address" (default: none)
    remember_ip_ttl: 30d   # how long a saved address skips the login (default 30d)

# stack file
proxy:
  - host: wiki
    network: wan
    port: 80
    middlewares:
      - authenticated:               # bare `authenticated` = any user of the provider
          groups: [family, admins]   # at least one of these
          allow_internal: true       # default: containers skip the login
```

This is a separate identity provider client and group namespace from
`auth.oidc` (which logs operators into the dashboard). Create the client with
one redirect URI per listed network, `https://<portal host>/callback`, then
store its secret (also under Settings, Service login in the dashboard):
`PUT /api/proxy-auth/client-secret` with `{"client_secret": "…"}` (admin; write
only, like `/api/oidc/client-secret`). The portal hostnames get DNS and a
certificate like any proxied host.

**Per network.** A login is once per network: each listed network has its own
portal and its own sessions, so signing in on `lan` does not sign anyone in on
`wan`. A route may only use `authenticated` on a network listed in
`proxy.auth.networks`; a stack asking for a login on any other network (or with
no `proxy.auth` at all) fails validation.

**Flow.** A request with no valid `cl_proxy_auth` cookie gets a 302 to
`https://<portal>/login?callback=<url>`. If the browser already has a portal
session, the portal immediately 302s back to
`https://<app>/.well-known/clouduler-cookie?token=…&redirect=…`, which spends
the token, sets the app's own cookie and 302s to the page originally asked
for: three redirects, no javascript, no click. With no portal session the
portal shows a sign-in page, runs the OIDC login, then does the same.
Non-navigation requests (non-GET, or an `Accept` without `text/html`) get a
401 instead of a redirect. A signed-in user outside the route's `groups` gets
a 403 page with a "use another account" link (portal logout).

**State.** Sessions and tokens are in raft, so any node accepts any login and
logins survive restarts; only a hash of each cookie/token is stored. A token
is valid for 60 seconds and can be spent once, cluster-wide. Cookies are
host-only and bound to their hostname; an app cookie never outlives the portal
session that produced it (`session_ttl`, fixed at login).

**Remembering an IP address.** On the networks listed in `remember_ip`, the
login page shows a "Remember this IP address" checkbox, and ticking it asks for
confirmation before logging in. The checkbox only appears when the visitor's
TCP peer address belongs to that network: inside its subnet for an internal or
private network, a globally routable address for a public one (never a header,
so it cannot be spoofed). A network not listed offers nothing, and a saved
address stops working if its network is removed from the list. The saved
address skips the login on that network for `remember_ip_ttl`, independent of
`session_ttl`, with the groups the user had at that login. Signing out at the
portal forgets the address. Anyone behind the same address, for example a
router doing NAT, is let in too, so turn it on only for networks where that is
acceptable. Groups are read at
login: a change at the provider applies from the next login. Signing out
(the portal's `/logout`, or `/.well-known/clouduler-logout` on any protected
host) removes the portal session and every app session handed out from it, so
all those cookies stop working at once.
The proxy strips its cookies before forwarding, so backends never see them.

**Callbacks.** The portal only redirects to a hostname whose route has
`authenticated`, so it is not an open redirect. The reserved paths
`/.well-known/clouduler-*` are answered by the proxy on protected hosts.

**Containers skip the login** by default (`allow_internal`): the check is on
the real TCP peer, which must be in a container overlay or nat subnet, or be a
node's internal address. Set `allow_internal: false` to require a login from
them too. Caveat: a host process on a node counts as internal.

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

`proxy.dns_publish` decides what a stack route's record actually says:

- **`all-nodes`** (default): since every node runs the reverse proxy and
  forwards internally, a hostname's records point at **every** node's
  address on the declared network, not just the node a stack's containers
  happen to be placed on — `n1` failing does not need `blog.example.com`'s
  DNS to change for `n2` to keep answering it. This is Docker Swarm's
  routing-mesh model: reaching any node reaches the service.
- **`owning-node`**: DNS does the routing instead of the proxy. Only the
  node(s) actually running a service's containers (`placement::service_nodes`)
  are published, and a stale record left on any other node is deleted
  through the Cloudflare API the next time `plan`/`apply` runs. Static
  backends and redirects aren't backed by a container, so they still
  publish every node's address in this mode. Because a hostname can only
  point at one place, the scheduler enforces an implicit placement
  constraint in this mode: every instance whose `x-clouduler.proxy` routes
  publish the same hostname (say, one service serving `/` and another
  serving `/admin` on the same host) is co-located onto one node
  (`cl_sched::place`, via `Desired::proxy_hosts`) — a placement request that
  cannot be satisfied is reported unplaceable rather than silently split
  across nodes.

A network can opt out of this with `publish_dns: false` on the network
(internal and private networks only). Its hostnames are then never sent to
Cloudflare, and records already published for it are deleted on the next
pass. They are answered by the client DNS server below, or by whatever DNS
the operator runs. Certificates for those hostnames are unaffected: DNS-01
only writes a challenge TXT record.

### Client DNS server

`proxy.dns_server` makes every node answer DNS itself, split-horizon style:

```yaml
proxy:
  dns_server:
    enabled: true      # default false; port 53 on node addresses is left alone otherwise
    networks: [lan]    # empty = every internal/private network the node has an address on
    port: 53
```

Each node listens on its own address on those networks
(`cl_daemon::client_dns`). The network a query comes from is decided by the
client's source address (`ClusterConfig::client_network`): the subnetted
network whose subnet holds it, else the public network for a globally
routable address. Answers follow:

- A hostname on a **public** network is answered to everyone. A client inside
  a private network gets each serving node's address *on that network* (a LAN
  client gets the node's LAN address, so it skips the router and the proxy
  sees its real address, which `proxy.auth.remember_ip` relies on). A node
  with no address on the client's network falls back to its address on the
  route's network.
- A hostname on a **subnetted** network is answered only to clients inside
  that subnet, exactly the proxy's network gate. Anyone else gets `REFUSED`.
- Which nodes are named follows `proxy.dns_publish`, and the web ui hostname
  names the leader alone, as in the published records.
- Names the cluster does not own are forwarded to the host's resolvers for
  clients inside a subnetted network, and `REFUSED` for everyone else, so
  listing a public network never makes the node an open resolver. A public
  address is only bound if this node actually holds it (often it sits on the
  router), otherwise the bind is retried quietly.

Config changes take effect without a restart. This is separate from the
container-facing resolver on the overlay gateways (`internal_dns`).

Records are tagged with an ownership TXT record the same way
`cl_dns::reconcile` always has, so a zone can be shared with records
clouduler doesn't manage. A record apply failure (e.g. a name colliding with
something else in the zone) is attributed to the specific hostname(s) it
names, not blanket-applied to every hostname sharing that Cloudflare zone —
one stack's DNS problem no longer shows up on an unrelated stack's records.

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

Cloudflare zone ids are resolved from `cloudflare.zones` at the start of every
issuance/renewal pass, not cached across ticks: the token is a replicated
cluster secret rather than a file read once at startup, so it can be set or
rotated from the web ui while the daemon is already running. The longest
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

### Certificates for a domain outside Cloudflare

Every hostname above assumes it sits under a zone listed in `cloudflare.zones`,
so its ACME challenge can be answered with a `_acme-challenge` TXT record.
That doesn't hold for a domain bought and pointed at the cluster without ever
being added to Cloudflare — `zones.zone_id_for` finds nothing for it, and by
default `issue_missing`/`renewal_tick` record
`ReconcileStatus::Error("no cloudflare zone covers this hostname")` and give
up rather than guess.

Writing the hostname in a stack's `x-clouduler.proxy` (or a static route)
with a trailing dot, e.g. `host: gitlab.example.net.`, opts that one name out
of that error and into ACME's other standard challenge, HTTP-01, instead:

1. `cl_config::qualify` strips the trailing dot from the fqdn actually used
   for routing/certificates, so the dot itself never reaches DNS. The raw,
   still-dotted `host` string is what `clouduler::proxy::tls_hosts` checks
   (`host.ends_with('.')`) to mark the resulting fqdn as HTTP-01-eligible —
   this happens in the daemon, not `cl-proxy`, since it's the caller that
   knows the raw stack config `qualify` already discarded the dot from.
2. `acme::issue_certificate_http01` orders the certificate the same way as
   `issue_certificate`, except each challenge's key authorization is written
   into an in-memory `Http01Store` (`token -> key_authorization`) instead of
   a DNS TXT record, and the challenge type requested is `Http01` not `Dns01`.
3. The plain-http listener (`server::handle_http`) answers
   `GET /.well-known/acme-challenge/<token>` straight out of that store,
   ahead of any routing decision and regardless of `Host` — the CA's
   validator has no reason to send a `Host` clouduler would recognize, and
   the hostname being validated may have no route yet on a first issuance.
   This means the domain's own DNS (however it's managed) must point at the
   cluster and its plain HTTP port must be reachable from the internet;
   nothing here publishes that A/AAAA record, since it isn't in a zone
   clouduler can write to.
4. The `Http01Store` is created once per proxy supervisor run and shared
   between the `listen` and `acme` task groups regardless of which one
   restarts on a config change — a token the issuance task hands out must
   still be answered by whichever listener instance is live when the
   validator asks for it.

Renewal recomputes the same `tls_hosts` allow-list from current config on
every tick rather than remembering how a certificate was originally issued,
so a certificate keeps renewing through HTTP-01 for as long as its stack
still declares the host with a trailing dot, and switches back to DNS-01
automatically if the domain is later added to a Cloudflare zone.

## Per-stack host lists

A stack can limit which hosts its containers ask the proxy for with
`allowed` on a network (`docs/stacks.md`, "Restricting what a stack can
reach"). The proxy identifies the caller by the TCP peer address, never a
header, against the overlay subnets of the stacks that declared a list
(`cl_proxy::PeerPolicies`, rebuilt from replicated state every reconcile pass).
When the route found for the request is published on a network the caller's
stack listed, and the request's `Host` is not on that list, the answer is a
`403` naming the stack, host and network. A route on a network the stack did
not list is unaffected, and a host with no route is still a `404`.

The check runs after the route lookup and the network gate. With no restricted
stack the table is empty and a request pays one atomic load. Otherwise it scans
the restricted subnets and does a hash lookup only for a caller inside one.
`host:port` entries never reach the proxy, the firewall enforces those.

A stack's own routes are exempt: a container that requests a hostname its own
stack publishes on that network passes without a list entry. Only hostnames
declared in the stack's own `x-clouduler.proxy` count, and only on the network
each is published on. Calls to sibling services by service name never reach the
proxy and need no entry.
