# Container overlay networking

`cl-net` builds the container overlay on top of the cluster's single
`internal` network (see `docs/config.md` / `examples/cluster.yaml`). One
overlay per cluster, one VXLAN segment per stack network, one Linux bridge
per segment, docker containers attached to that bridge.

## Trust model

The internal network is explicitly unencrypted (`NetworkKind::Internal` in
`cl-config`), on the assumption that the operator already trusts it: a
private LAN, a WireGuard/Tailscale mesh, a datacenter backplane. The overlay
inherits that assumption. **Nothing in this crate encrypts or authenticates
overlay traffic.** Anyone with access to the internal network can read or
spoof container-to-container packets. Don't put the internal network over
anything you don't already trust; if you need encryption, put it under the
internal network (a WireGuard mesh, for example), not on top of the overlay.

## Why point to point FDB instead of multicast

Kernel VXLAN normally floods unknown-destination traffic to a multicast
group. Multicast routing is frequently unavailable between cloud hosts,
across a WireGuard mesh, or through a NAT boundary, so clouduler never
relies on it. Instead, each VXLAN device's forwarding database (FDB) is
populated with one static entry per peer node, using the all-zero wildcard
MAC (`00:00:00:00:00:00`) as the destination — the same trick `bridge fdb
append 00:00:00:00:00:00 dst <peer> dev vxlan0` uses on the command line.
With the wildcard MAC and no multicast group, any packet with an unknown
destination MAC gets flooded to every peer's entry, which reproduces
multicast's head-end replication behaviour without needing multicast. The
cluster's peer set is already known from the config, so building this table
is just "every other internal-network node."

Static FDB also means `learning` is turned off on every VXLAN device: the
kernel never adds entries on its own from observed traffic, so the FDB is
always exactly what the reconcile loop put there, and reconciling it is a
pure diff, not "trust whatever's there."

## Interface naming

Every stack network gets two interfaces:

- a bridge, named `clbr-<network>`
- a VXLAN device, named `clvx-<network>`, enslaved to the bridge

Linux interface names are capped at 15 usable characters (`IFNAMSIZ - 1`).
A short network name fits directly (`clbr-gitlab`). A longer one is
truncated and a 4 hex character suffix, derived from an FNV-1a hash of the
full name, is appended so two names that truncate to the same prefix don't
collide (`clbr-` + 6 chars of the name + 4 hash chars = 15). The scheme is
deterministic: every node computes the same interface names for the same
network name without coordinating.

## Single-node overlays

A public network's per-node address is optional (a node just omits the
`network.<name>` entry to not implement it), so the scheduler never places a
service needing it anywhere but a node that declares one
(`NodeState::implements_network`). When at most one node in the whole
cluster declares a given public network, that network's fallback overlay
(see "Replication: who allocates" below for when a stack gets one) can never
have a second replica land anywhere else, so nothing will ever need to reach
it over a VXLAN tunnel.

`overlay::is_single_node_network` detects this and `ensure_network` skips
building the VXLAN device and FDB for it entirely — just the bridge, docker
network and address, same as any other overlay otherwise. This also means it
needs no address on the cluster's `internal` network at all: previously a
node hosting only such a network but no `internal` address (or one not
actually bound to a local interface) failed with "no interface holds the
internal address ...", loudly and every reconcile tick, for a tunnel that
was never going to carry any traffic.

## VNI derivation

The VXLAN Network Identifier (VNI) is a 24 bit space. It is derived
deterministically from the network name with the same FNV-1a hash used for
interface naming, taken modulo 2^24, with VNI 0 (reserved by convention)
treated as a collision. Every node computes the same VNI for the same
network name independently — no coordination or allocation round trip
needed.

Collisions (two network names hashing to the same VNI, or landing on 0) are
resolved by perturbing the hash input with a counter (`<name>#1`,
`<name>#2`, ...) and rehashing until a VNI outside the cluster's currently
taken set is found. With 2^24 VNIs and a handful of stack networks per
cluster this never happens in practice, but the reconcile loop has to behave
correctly if it ever does, so the collision path is implemented and tested
rather than assumed away.

## MTU

VXLAN encapsulates each frame inside outer IP + UDP + VXLAN + inner Ethernet
headers. The overlay MTU is the underlay link's MTU minus that overhead:

- **50 bytes** over an IPv4 underlay (20 outer IP + 8 outer UDP + 8 VXLAN + 14 inner Ethernet)
- **70 bytes** over an IPv6 underlay (40 outer IP, everything else the same)

Both the bridge and the VXLAN device are set to this MTU. Getting this wrong
is a classic silent failure: packets under the overlay MTU work fine, ARP
and small requests succeed, and then anything that needs a full-size frame
(large HTTP responses, bulk transfer) just stalls with no ICMP and no reset,
because the underlay silently drops or fragments-and-drops what the overlay
told the guest kernel was a valid frame size. Always compute the overlay MTU
from the underlay's actual MTU, never assume 1500.

## IPAM

`cl-net::ipam::Allocations` is pure, replicated state: no netlink, no IO,
fully unit tested and serde round-trippable so it can travel through raft.

- Stack networks are carved out of a configurable overlay pool, `/24` by
  default, itself defaulting to `10.244.0.0/16`. A network's subnet is
  chosen by walking the pool's `/24`s in order and skipping any that
  overlap a subnet another stack network already claimed, or a subnet a
  declared cluster network (internal or private) already uses — the overlay
  can never shadow cluster traffic.
- Within a network's subnet, `.1` is reserved for the bridge's gateway
  address; every other address is available to instances.
- Allocating an address for an instance that already has one returns the
  same address (idempotent), so a restart or a reconcile replay never
  reassigns addresses out from under a running container.
- Releasing an address returns it to a cooldown queue rather than the
  available pool directly. A fresh, never-used address is always preferred
  over a cooled-down one, so a container that restarts quickly is unlikely
  to land on an address some peer's ARP cache or conntrack table still
  associates with the previous instance. Only once every fresh address is
  exhausted does allocation fall back to the oldest released address.

## Replication: who allocates

`Allocations` is the payload of one replicated value, `ClusterState.overlay`,
held as opaque json so the address book can change shape without every node
having to agree on it byte for byte the way a state machine command does.

The leader is the only node that writes it. On every pass of the overlay loop
(`clouduler::overlay`) it gives each stack network a subnet, each instance on
that network an address, and returns the addresses of instances that no longer
have an assignment; if anything changed it submits the whole book back through
`Request::SetOverlayAllocations`. Every node, leader or not, then reads it:

- the overlay loop, to know which subnet and gateway to build the bridge with;
- the scheduler, to pin the container's address at create time rather than
  letting docker pick one;
- internal DNS, published ports and the reverse proxy, to name containers
  running on other nodes.

That last group is why addresses are allocated centrally at all. A node has no
way to ask another node what address its containers ended up with, so an
address docker chose locally could never be published cluster wide.

A container is not started until its address exists. The scheduler skips an
instance whose network has no allocation yet and picks it up on the next pass,
which costs a few seconds on a first deploy and avoids a container coming up on
an address nothing else in the cluster knows about.

## DNS

`cl-net::dns::InternalDns` runs a `hickory-server` resolver on the bridge's
gateway address, handed to containers as their DNS server. Names available
inside the overlay:

- `<service>.<stack>` — every running instance of that service, as multiple
  A records. An ordinary resolver doing ordinary round robin over multiple
  answers reaches a live replica either way.
- `tasks.<service>.<stack>` — the same set, under the name people already
  know from Docker Swarm.
- `<node>` — a node's address on the internal network, so a container can
  reach a node directly (for example, to talk to a drive's sync endpoint)
  without knowing the cluster topology.
- anything else is forwarded to the host's configured upstream resolvers
  (read from `/etc/resolv.conf`), so ordinary internet names still resolve.

The zone is hot-swappable: placement changes rebuild the record set and
swap it into the running server without a restart or dropping in-flight
queries, so a service that moves to another node picks up its new address
on the next lookup rather than after a TTL expires or a resolver restart.

### How containers reach it

The daemon binds the resolver on port 53 of every overlay network's gateway
address, which is an address on this node's own bridge and so belongs to
clouduler alone; port 53 on the node's real addresses is left for whatever the
operator runs there. Each container is created with that gateway as its
resolver and two search domains: its own stack under the zone first (e.g.
`web.int.example.com` for a container in the `web` stack), then the bare zone.
That ordering means a bare service name — `headscale`, say, written into a
config file before it ever ran under clouduler — resolves to the sibling
service in the same stack (`headscale.headscale.int.example.com`) without the
file needing to spell out `service.stack`; a fully qualified `service.stack`
name (for reaching a sibling in *another* stack) still resolves too, via the
bare-zone entry, exactly as before.

It is also created with `ndots:2`. Docker's own resolver hands a container
`ndots:0`, which treats any name containing a dot as absolute and skips the
search list: `web.blog` then comes back NXDOMAIN from upstream while
`web.blog.int.example.com` resolves perfectly well. Two is the smallest value
that sends a `service.stack` name through the search list, and it leaves
ordinary two-dot internet names resolving directly.

The zone itself is the internal network's `domain` when it declares one, so a
name means the same thing inside a container and outside it. With no domain
declared the zone is `cluster.local`, which only resolves inside the cluster.

Records are rebuilt from replicated state every five seconds, matching the
record TTL, and are built the same way on every node: a node answers for
containers it is not running as readily as for its own.

## Non-HTTP published ports

`x-clouduler.expose` (the motivating case is publishing gitlab's ssh on port
22) is not proxied at the HTTP layer, since it isn't HTTP. Instead the
published port is bound on **every** node, and `cl-net::expose` forwards
accepted connections over the overlay to whichever node currently runs the
owning instance:

- TCP: a straightforward `tokio::io::copy_bidirectional` splice between the
  accepted connection and a connection dialed to the current backend.
- UDP: a per-client-address session, each with its own socket connected to
  the backend, forwarding datagrams in both directions until the session
  goes idle.

Forwarding in userspace means the backend sees the connection coming from
**this node's address on the container bridge**, not the client's. That is
inherent to a relay: it terminates one connection and opens another. For a
service that keys anything on the client address it is worse than a cosmetic
problem — every client is indistinguishable, so a per-IP rate limit or
fail2ban ends up acting on the relay, and one abusive client locks out all of
them. `proxy_protocol: true` on the `expose` entry fixes it for backends that
speak PROXY protocol v2; see docs/stacks.md for the ordering it requires.

Routing is driven by a table the reconciler updates as placement changes.
Forwarders don't resolve the backend once and forget it: each connection or
session watches its route for the rest of its life, so if the instance
moves to another node while a connection is open, the connection is closed
immediately rather than left hanging against a backend that's no longer
being served — the client sees a clean disconnect, not a stall.

A service with several replicas is forwarded to one of them, the lowest
instance index, rather than balanced across them. There is no request boundary
in a raw TCP or UDP stream to spread work over, and picking by index means
every node forwards to the same replica, so a client that reconnects through a
different node does not land somewhere else for no reason.

A published port that no running instance backs stays bound and refuses
connections rather than disappearing, so a redeploy shows up as a connection
refused rather than as the port belonging to nobody.

## Egress networks

Every other network kind (`internal`, `private`, `public`) is about how a
container is *reached*. An `egress` network is about how one *leaves*: a
service attached to it gets its outbound traffic forced out a named host
interface, and nowhere else, instead of the node's normal route.

Unlike most of this daemon's other subsystems, which only see a config
change once restarted (`watch_cluster_config`, `crates/clouduler/src/daemon.rs`,
literally logs "restart the daemon to apply it" when it writes a changed
config to disk), the egress reconcile loop re-reads the config from raft
every tick (`RECONCILE_INTERVAL`, currently 10s). Add a new `egress`
network, point a node at an interface for it, or attach a service to it, and
the bridge, kill-switch and docker network come up on the next tick — no
restart needed. A node still has to be restarted to notice most *other*
kinds of config change (a new drive, a new `public` network address, and so
on); this loop is the one exception.

```yaml
networks:
  vpn:
    type: egress

nodes:
  n1:
    network:
      vpn: wg0   # bare string: a host interface name, not an address
```

A node's address on an egress network is a host interface name
(`NodeAddress::Interface`), not an IP or a domain — the third form
`nodes.<name>.network` can take, alongside a bare IP on a subnetted network
and an address block on a public one. Only a node with such an address
**implements** the network; a service naming it in a stack file's
`networks:` is automatically restricted to those nodes by the scheduler
(`NodeState::implements_egress_network`, checked in `cl_sched::place`
exactly like a drive requirement), and can also be pinned there explicitly
with `x-clouduler.node`.

**Clouduler does not manage the interface.** `wg0` above is expected to
already exist and be up — brought up by `wg-quick`, Tailscale, or whatever
the operator uses, entirely outside clouduler, matching the trust-model note
above about running a VPN mesh underneath the cluster rather than clouduler
implementing one. All clouduler needs is the interface's name.

A service may attach to one normal network and one egress network at once
(`networks: [home, vpn]`): the overlay network stays the container's
primary (create-time) attachment exactly as it would without an egress
network, so nothing about DNS, the reverse proxy or its fixed address
changes. The egress network is attached second, after creation
(`Docker::connect_network`).

Docker only ever gives the *primary* attachment the default route, and a
network connected afterward never gets it — not even with the container's
own `NET_ADMIN`, since routing changes from inside the container would
depend on how the workload's image is built. So `Docker::set_default_route`
does it from the host instead: find the container's PID, and inside its own
network namespace (`nsenter -t <pid> -n`, needing only the daemon's own
privileges, not a capability grant on the workload) replace the default
route with the egress network's gateway. This is deliberately not docker's
newer `GwPriority` endpoint field (a way to mark a secondary attachment as
the default-route winner) — that field is silently ignored by older
daemons, so it cannot be relied on, where explicitly replacing the route
works on any docker version.

### The bridge and the kill-switch

Unlike the overlay, an egress network is node-local: it is never a VXLAN
segment spanning nodes, so it needs no cluster-wide subnet allocation. Each
node implementing it derives its own `/24` deterministically from the
network's name (`cl_net::egress::egress_subnet`) and builds a plain Linux
bridge for it (`clex-<network>`, `cl_net::egress::ensure_egress_bridge`) —
no VXLAN device, since nothing here needs to reach another node.

The only route out of that bridge is enforced with iptables, applied fresh
every reconcile pass (`cl_net::egress::apply_egress_iptables`):

- a `MASQUERADE` rule NATs the bridge's subnet out the named interface,
  and only that interface;
- a `FORWARD` rule accepts traffic from the bridge specifically when it's
  leaving via that interface (plus the matching return-traffic rule);
- a final `FORWARD` rule drops everything else from the bridge.

That order is the kill-switch: if the interface goes down, is renamed, or
was never brought up, there is no rule that accepts the bridge's traffic out
anything else, so it is dropped rather than silently falling back to the
node's normal WAN. Nothing has to notice the interface is down and react —
the ruleset simply never had another way out.

### Optional fallback interface

An egress-kind node address can name a second interface plus a
`health_check_ip`:

```yaml
nodes:
  n1:
    network:
      vpn:
        interface: wg0
        fallback:
          interface: end0
          health_check_ip: 1.1.1.1
```

Every reconcile tick (`crates/clouduler/src/egress.rs::effective_interface`),
clouduler sends a single ICMP echo to `health_check_ip` out `interface`
(`cl_net::egress::probe_reachable`, which shells out to `ping -I`, the same
"shell out to a real network tool" convention `iptables` and `ip` use
elsewhere in this crate). If it answers, the kill-switch ruleset above is
built exactly as it would be with no fallback configured. If it doesn't —
timeout, no route, a missing `ping` binary, anything — the ruleset is
rebuilt pointed at `fallback.interface` instead, on this tick and every one
after until the primary starts answering again. There's no separate "back
up" step: since `apply_egress_iptables` always flushes and repopulates its
chains from scratch, switching back is just the same rebuild picking the
other interface once the probe succeeds again.

This only applies to `egress`-kind addresses. A `wireguard`-kind address can
carry a `fallback` block (the schema is shared), but the reconcile loop
never probes it — a wireguard tunnel doesn't have a "second interface" to
fail over to the way a plain egress address does.

## NAT networks

A `nat`-kind network is `egress`'s opposite number: instead of forcing a
service's outbound traffic out one named interface, it lets the node's own
routing table decide, exactly like an ordinary NAT'd docker bridge network
would. Useful for reaching addresses only the host's routing table knows how
to get to — a Tailscale or other VPN mesh peer, say — which a single fixed
`egress` interface cannot do, since that interface's kill-switch drops
anything that doesn't leave through it.

```yaml
networks:
  home:
    type: nat

services:
  uptime-kuma:
    image: louislam/uptime-kuma
    networks: [proxy, home]
```

No node address is declared or needed — a `nat` network takes no entry
under `nodes.<name>.network` at all, and one is rejected as invalid if
given. Every node implements it identically, so unlike `egress` or
`wireguard` it never restricts which node a service attached to it can land
on: the scheduler treats membership in `ClusterConfig::nat_networks()` as an
automatic pass in `cl_sched::place::rejection`, the same way a served
drive's name in `served_drives` bypasses the "does this node have a local
copy" check for a drive. The placement table in the ui (`ConstraintTable`)
mirrors this the same way, so a nat network never shows as missing from a
node that legitimately has nothing to configure for it.

There is no manual bridge or iptables kill-switch either: clouduler asks
docker to build and own an ordinary bridge network
(`cl_net::egress::nat_docker_name`, `Docker::ensure_nat_network`), and
docker's own MASQUERADE rule — which doesn't pin to one interface, unlike
`egress`'s — does the rest. Brought up the same way `egress` networks are,
on the same 10 second reconcile loop
(`crates/clouduler/src/egress.rs::RECONCILE_INTERVAL`), so adding one or
attaching a service to it needs no daemon restart.

A service attaches to at most one network from `{egress, wireguard, nat}` at
once, alongside its normal overlay network, the same "one extra network,
attached after creation with its default route pointed at it" mechanism
`egress` uses (see above) — `nat` is simply a different kind entry in that
same set.
