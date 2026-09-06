# Stack files

A stack file describes the services clouduler schedules. It is a Docker
Swarm / Compose v3 file: the subset clouduler understands, plus an
`x-clouduler` extension block for the parts compose has no concept of
(the reverse proxy, drives, node pinning). See `examples/stacks/gitlab.yaml`
and `examples/stacks/simple.yaml` for full examples, and `examples/cluster.yaml`
for the cluster model referenced below.

Any key clouduler doesn't understand is a parse error, not a silent no-op.
If a compose file uses `build:`, `logging:`, or any other unsupported key,
loading the stack fails naming that key, rather than pretending it worked.

Parsing (`StackFile::parse`) only checks the file is internally consistent
and can be done without a cluster on hand (useful for linting). Checking a
stack against a specific cluster (`StackFile::validate`) additionally
confirms every network, drive and node it references actually exists there.

## Top level

```yaml
version: "3.8"   # accepted, ignored
services: {...}  # required
networks: {...}
volumes: {...}
configs: {...}
secrets: {...}
x-clouduler: {...}
```

- `version` is accepted and ignored.
- `networks` declares which cluster networks this stack's services attach
  to. Clouduler does not create custom docker networks: each name here must
  match a network declared in the cluster's `networks:` section. Entries take
  no fields (`wan: {}` or `wan:`).
  A service may name at most one network of the cluster's `egress` kind
  alongside its usual (non-egress) network — see `docs/networking.md`'s
  "Egress networks" section. It gets a second, NAT'd attachment forcing its
  outbound traffic out that network's host interface, and the scheduler
  restricts it to nodes that implement that network automatically; no
  `deploy.placement.constraints` entry is needed for that part.
- `volumes` is accepted for compose compatibility but not used: a service
  references drives directly through its own `volumes:` list (see below).
- `configs`/`secrets` declare cluster objects. An entry with `file:` bundles
  that file's content with the stack at deploy time (or, from the web ui,
  whatever you type or upload into the matching field); one with no `file:`
  refers to an object that must already exist. Either way, a service mounts
  one by name through its own `configs:`/`secrets:` list — see
  `docs/secrets.md`.

## Services

```yaml
services:
  gitlab:
    image: gitlab/gitlab-ce:16.9.1-ce.0
    hostname: gitlab-host
    read_only: false
    command: ["gitlab-ctl", "reconfigure"]
    entrypoint: /bin/sh
    environment:
      FOO: bar
    env_file: .env
    labels:
      team: infra
    volumes:
      - fast/gitlab-data:/var/opt/gitlab
    ports:
      - "8080:80"
    networks: [wan]
    depends_on: [db]
    healthcheck:
      test: ["CMD", "curl", "-f", "http://localhost/-/health"]
      interval: 30s
    restart: unless-stopped
    user: "1000:1000"
    working_dir: /app
    cap_add: [NET_ADMIN]
    cap_drop: [ALL]
    sysctls:
      net.core.somaxconn: "1024"
    stop_grace_period: 30s
    stop_signal: SIGINT
    deploy: {...}
    x-clouduler: {...}
```

A service name must be a valid DNS label (lowercase letters, digits and
hyphens, not starting or ending with one) because it becomes the service's
overlay hostname.

- `image` is required; there is no `build:` support.
- `hostname` overrides the container's hostname. Independent of the service
  name, which is what other services on the same stack use to reach it over
  the overlay.
- `read_only` (default `false`) makes the container's root filesystem
  read-only. A service that writes outside its declared volumes fails loudly
  instead of quietly accumulating state nothing backs up.
- `command`/`entrypoint` accept either a shell string or an exec array.
- `environment` accepts a mapping or a `KEY=value` list.
- `env_file` accepts a single path or a list of paths.
- `labels` accepts a mapping or a `key=value` list.
- `networks` entries must be declared in the stack's own top-level
  `networks:` section.
- `depends_on` accepts a list of service names; every name must be another
  service in the same stack.
- `sysctls` accepts a mapping or a `key=value` list.
- `stop_grace_period` and all `deploy:` durations use humantime syntax
  (`30s`, `2m`, `1h30m`).
- `stop_signal` overrides the signal sent on a graceful stop (e.g. `SIGINT`
  for a database that treats `SIGTERM` as a slow, wait-for-clients shutdown).
  Left unset, the image's own `STOPSIGNAL` applies (usually `SIGTERM`). Must
  be a `SIG*` name or a plain signal number.
- `restart` is one of `no`, `always`, `on-failure`, `on-failure:N`,
  `unless-stopped`.

### Volumes

Compose host binds and named volumes are replaced by clouduler's drive
syntax, so every persistent volume lives on a drive the cluster can
replicate and migrate between nodes:

```yaml
volumes:
  - fast/gitlab-data:/var/opt/gitlab       # drive/volume:/container/path
  - fast/gitlab-config:/etc/gitlab:ro      # read-only
  - /tmp/scratch                           # anonymous, no drive backing
  - type: tmpfs                            # in-memory, no drive backing
    target: /cache
    tmpfs:
      size: 64m
```

`drive/volume:/container/path[:ro]` parses into
`VolumeMount { drive, volume, target, read_only }`. A plain container path
with no source is accepted as an anonymous volume (`drive`/`volume` are
`None`). A host path on the left (anything starting with `/`) is rejected:
volumes must live on a drive so they can be replicated and migrated, and a
host bind ties the container to one machine's filesystem.

A `type: tmpfs` long-form entry is an in-memory volume backed by no drive at
all — its contents don't survive a container restart and are never
replicated. `tmpfs.size` is optional (a bare number or `64m`/`64MiB`); left
unset, docker's own default (half the host's RAM) applies. `type: bind` is
rejected for the same reason a bare host path is: volumes must either live on
a drive or be explicitly in-memory, never tied to one machine's filesystem.

### Ports

The compose short and long forms are both supported:

```yaml
ports:
  - "8080:80"                    # published:target, tcp
  - "127.0.0.1:8080:80/tcp"      # host_ip:published:target/protocol
  - "8080-8090:80-90"            # ranges
  - target: 80
    published: 8080
    protocol: udp
    mode: host                   # optional, and the only supported value
```

A published port is always bound directly on the node running the container
— clouduler has no swarm-style ingress mesh, so there is nothing for a
request landing on a *different* node to be relayed through. `mode: host`
(the long form only; the short string syntax has no room for it) says this
explicitly and matches what happens by default. `mode: ingress` is accepted
syntax, so a stack file copied from real swarm doesn't fail to parse on the
field alone, but is rejected as a value: there is no mesh to back it. For a
raw port that should be reachable on whichever node currently runs the
container, forwarded over the overlay, use `x-clouduler.expose` instead.

## `deploy`

```yaml
deploy:
  replicas: 2
  mode: replicated            # or global
  placement:
    constraints:
      - "node.hostname==n1"
  resources:
    limits: { cpus: "1.0", memory: 512M }
    reservations: { memory: 128M }
  restart_policy:
    condition: on-failure
    delay: 5s
    max_attempts: 3
    window: 30s
  update_config:
    parallelism: 1
    delay: 10s
    order: start-first
    failure_action: rollback
```

A `node.hostname==<name>` or `node==<name>` placement constraint is checked
against the cluster's node names when validating against a cluster; other
constraint forms are accepted but not interpreted.

## `x-clouduler`

Clouduler's own extensions. Can be set per service, and once at the top
level of the stack for stack-wide defaults (`auto_update`, `node`,
`volume_sync`) that services inherit unless they set the same field
themselves.

```yaml
x-clouduler:
  auto_update: false
  node: n1

services:
  gitlab:
    x-clouduler:
      proxy: [...]
      expose: [...]
      volume_sync: {...}
      auto_update: true   # overrides the stack default for this service
      node: n2             # overrides the stack default for this service
```

### `proxy`

HTTP routes published through the reverse proxy.

```yaml
proxy:
  - host: gitlab            # bare label gets the network's domain appended
    network: wan             # which declared network publishes this route
    port: 80                 # container port
    path_prefix: /git        # optional
    tls: true                # default true
    middlewares: [redirect_https, strip_prefix]
    sticky: true              # optional

    # Reverse proxy behaviour, all optional
    max_body_size: 10MB       # 413 above this; unset means no limit
    websockets: true          # default true
    preserve_host: false      # default false
    response_timeout: 30s     # unset waits forever
```

Fields: `host` (a bare label like `gitlab` is qualified with the network's
domain, e.g. `gitlab.example.com`; a name containing a dot is used as is;
a name ending in a dot, e.g. `gitlab.example.net.` — the DNS zone-file
convention for "this is already absolute" — is also used as is, with the dot
itself stripped, and additionally marks the hostname as one that may not
belong to any configured Cloudflare zone: see "Certificates for a domain
outside Cloudflare" in `docs/proxy.md`), `network` (must be declared in the
stack and have a domain in the cluster: an HTTP route needs somewhere to
qualify its hostname against), `port`, and the optional `path_prefix`, `tls`
(defaults to `true`), `middlewares` and `sticky`.

#### Reverse proxy settings

Every one of these is optional, and the default is the behaviour that existed
before it was configurable, so adding them to an existing stack changes
nothing until you set one.

| Field | Default | What it does |
| --- | --- | --- |
| `max_body_size` | unlimited | Largest request body to forward. A declared `Content-Length` over the cap is refused with 413 before the body is read at all; a chunked body is cut off mid-stream once it passes the cap. Accepts `10MB`, `10MiB`, `10M` or a plain byte count. |
| `websockets` | `true` | Whether `Upgrade` requests are proxied. Turning it off answers them with 400, which is worth doing on a route that should never see one. |
| `preserve_host` | `false` | Forward the client's `Host` instead of rewriting it to the backend's address. Needed by anything that builds absolute URLs or virtual hosts on the header. |
| `response_timeout` | none | How long the backend has to return response headers, connection time included. Answers 504 when it runs out. Do not set this on a route serving an event stream or a long download — it applies to headers, but a backend that is slow to *start* answering will be cut off. |

Byte sizes take decimal (`kb`, `mb`, `gb`) or binary (`kib`, `mib`, `gib`)
suffixes; a bare `k`/`m`/`g` is binary, and a plain number is bytes.

#### Middlewares

A middleware that needs no settings is named as a plain string. One that does
takes a single key map:

```yaml
middlewares:
  - redirect_https
  - strip_prefix
  - basic_auth:
      users:
        alice: "$2y$05$..."     # bcrypt hash, as in an htpasswd file
      realm: "internal"          # optional, defaults to "clouduler"
  - headers:
      request:                   # set on the way to the container
        X-Real-Source: clouduler
      response:                  # set on the way back to the client
        Cache-Control: no-store
      remove_request: [X-Debug]
      remove_response: [Server]
  - cors:
      origins: ["https://app.example.com"]   # or ["*"] for any origin
      methods: [GET, POST]                    # default: GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS
      headers: []                             # empty echoes the browser's requested headers
      allow_credentials: false
      max_age: 10m
  - forward_auth:
      address: https://auth.example.com/verify
      auth_response_headers: [X-User, X-Email]   # copied from the auth response onto the forwarded request
      timeout: 5s                                 # optional, unset waits forever
  - redirect_regex:
      regex: "^https://old\\.example\\.com/(.*)"
      replacement: "https://new.example.com/$1"
      permanent: true                             # default true: 301/308 vs 302/307
```

- `redirect_https`: answer plain HTTP with a redirect to the same URL over TLS.
- `strip_prefix`: remove the route's `path_prefix` before forwarding.
- `basic_auth`: HTTP basic auth. Passwords are given as hashes only; a
  plaintext password here would end up in the cluster database and in shell
  history.
- `headers`: header rewriting in both directions.
- `cors`: cross-origin access control. An `OPTIONS` preflight is answered
  directly, without reaching the container; a real request from an allowed
  origin gets `Access-Control-Allow-*` added to the response. `origins:
  ["*"]` cannot be combined with `allow_credentials: true` — browsers reject
  that pairing, so it is rejected here instead of failing confusingly at
  request time.
- `forward_auth`: delegates authentication to an external HTTP endpoint
  before the request reaches the container. The endpoint is called with the
  request's `Cookie`/`Authorization` headers plus `X-Forwarded-Method` and
  `X-Forwarded-Uri`; a 2xx response lets the request through, with any header
  named in `auth_response_headers` copied from the auth response onto the
  request forwarded to the backend (so the backend can see the identity the
  auth endpoint established); anything else is relayed straight back to the
  client, and the backend is never contacted. `address` must start with
  `http://` or `https://`.
- `redirect_regex`: rewrites the request's URL via regex and redirects to the
  result, instead of proxying to a backend. Matched against the *full*
  request URL (`scheme://host/path?query`, not just the path), so a rewrite
  can change the host too. `replacement` may use `$1`, `$2`, ... from the
  regex's capture groups. A request that doesn't match falls through to
  normal proxying. `permanent` (default `true`) picks 301/308 vs 302/307.

Naming `basic_auth`, `headers`, `cors`, `forward_auth` or `redirect_regex` as
a bare string is an error, since none of them do anything useful without
settings.

### `expose`

Non-HTTP ports routed over the overlay network rather than the reverse
proxy, for protocols like SSH that aren't HTTP.

```yaml
expose:
  - port: 22            # container port
    published: 2222      # port published on the network
    protocol: tcp         # tcp or udp, default tcp
    network: tailscale     # which declared network publishes it
    proxy_protocol: false   # announce the real client, default false
```

#### `proxy_protocol`

A published port is forwarded **in userspace**, so the backend sees a
connection from the node's own address on the container bridge — never the
client's. For most services that only makes the access log less useful. For
anything that decides per client address (rate limits, fail2ban, allowlists)
it is dangerous: every client looks identical, so one abusive client gets the
*relay* banned and takes every other client down with it.

Setting `proxy_protocol: true` prepends a [PROXY protocol
v2](https://www.haproxy.org/download/2.8/doc/proxy-protocol.txt) header to
each backend connection, carrying the real client and the address it
connected to.

TCP only — the header goes at the head of a stream, and UDP has none, so the
combination is rejected at validation rather than ignored.

**Enable it on the backend first.** The header is extra bytes ahead of the
protocol, so a backend that is not expecting one sees a malformed request.
Servers that support it usually gate on a trusted-proxy list, and typically
reject a header arriving from an address not on that list — which means
turning this on before the backend trusts the node takes the service down
completely. Turn it on in the backend, confirm it still works without a
header (most implementations accept both from a trusted address), then set
this.

### `volume_sync`

Per-volume override of the drive's default replication interval.

```yaml
volume_sync:
  gitlab-data:
    interval: 5m
    enabled: true
```

`StackFile::sync_interval(service, volume, cluster)` resolves, in order: the
service's own `volume_sync` entry for that volume, then the stack-wide
`x-clouduler.volume_sync` entry for that volume, then the backing drive's own
`sync_interval` in the cluster config.

### `auto_update`

```yaml
auto_update: true
```

Whether a `:latest` image tag is followed automatically when a new image is
pushed, rather than requiring an explicit stack redeploy.

### `node`

```yaml
node: n1
```

Pins the service to a specific node by name, instead of letting the
scheduler place it. Must name a node declared in the cluster.

### `docker_socket`

```yaml
docker_socket:
  target: /var/run/docker.sock   # path inside the container
  read_only: false                # default false
```

Bind-mounts the host's docker socket into the container, so it can control
docker itself — build images, launch sibling containers, and so on. This is
the one deliberate bind-mount escape hatch in clouduler: every other mount
has to live on a drive so it can be replicated and migrated (see
[Volumes](#volumes)), but the docker socket is neither data nor something a
drive could back.

`target` is required; `read_only` defaults to `false` since most docker
clients need to create and start containers, not just inspect what's
running.

Granting this is equivalent to giving the container root on the host:
anything with write access to the socket can launch a privileged container
and mount the host filesystem into it. Only set it on services you already
trust with root.

### `colocate`

```yaml
colocate: [worker, cache]
```

Other services in the same stack that must always land on the same node as
this one. Transitively closed: if `a` colocates with `b`, and `b` with `c`,
all three end up on the same node even though `a` never named `c` directly.
Every name must be a service declared in the same stack, and a service
cannot name itself.

Uses the same placement mechanism as `x-clouduler.proxy` hostname
colocation: a node already hosting one member of the group anchors the
rest, an empty cluster picks the least loaded eligible node for the whole
group, and a colocated instance never takes part in rebalancing (moving one
member alone would break the group). Conflicting constraints across a group
(e.g. two members pinned to different nodes) leave the group unplaceable
rather than silently splitting it across nodes.

## Validation

`StackFile::parse` runs syntax-only checks that don't need a cluster:

- service names are valid DNS labels
- `depends_on` targets exist in the same stack
- `networks` referenced by a service are declared in the stack's own
  `networks:` section
- no two services publish the same `protocol`/port, across both `ports:`
  and `x-clouduler.expose`
- `x-clouduler.colocate` targets name a real service in the same stack, and
  a service does not name itself

`StackFile::validate(&cluster)` runs the above plus cross-checks against a
`ClusterConfig`:

- every declared stack network exists in the cluster
- every drive referenced by a volume exists in the cluster
- `x-clouduler.proxy[].network`/`x-clouduler.expose[].network` name a
  declared cluster network; a proxy route on a network with no domain is
  an error
- `node.hostname==<name>`/`node==<name>` placement constraints, and
  `x-clouduler.node` pins (service level and stack level), name real
  cluster nodes

Every error names the config path that failed, e.g.
`services.gitlab.x-clouduler.proxy[0].network`.
