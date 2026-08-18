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
resolver and the internal zone as its search domain, so `web.blog` resolves
without the cluster's domain being written into the stack file.

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
