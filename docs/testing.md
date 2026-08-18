# Testing

Four layers, cheapest first. Run the first three on every change; the fourth
before anything that touches failover, replication or scheduling.

```
cargo test --workspace          unit and in-process cluster tests
cd ui && npx vitest run          ui component and reducer tests
cd ui && npm run build           typechecks the ui against the api types
tests/qemu/run.sh --all          real VMs, real docker, real network faults
```

## Unit and in-process tests

Every crate carries its own. The ones worth knowing about:

- `cl-raft`: state machine transitions, and `tests/cluster.rs`, which stands up
  a 3-node openraft cluster in one process to cover election, forwarding,
  membership change and config replication.
- `cl-config`: parsing and validation, including the rules that would otherwise
  only fail at runtime (fence ordering, addresses inside their subnet, drives
  nobody implements).
- `cl-drive`: the delta engine against large files, sparse edits, renames and
  deletions.
- `clouduler`: `init`'s planning and prompting, plus `init/tests.rs`, which
  serves a real HTTPS api and runs the join client against it end to end.

That last one is worth copying the shape of. It caught a bug nothing else
could: rustls had no crypto provider installed, so the daemon started fine and
panicked at the first TLS handshake, which is to say the first time anyone
opened the web ui.

## The qemu suite

`tests/qemu` boots real Linux VMs with real docker, wires them into a private
network, and drives the cluster through its http api and cli exactly as an
operator would. It is the only layer that can test a node losing power, a
network partition, or a kernel that will not mount what you told it to.

```
tests/qemu/build-image.sh        # once, or after changing the base image
tests/qemu/run.sh --list
tests/qemu/run.sh formation      # one scenario
tests/qemu/run.sh --all          # everything
```

Needs `qemu-system-x86_64`, `qemu-img`, `mke2fs`, `unshare`, `jq`, `python3`
with PyYAML, and either a musl toolchain or docker (see below). No root: the
rootfs is built and packed inside a user namespace, and the networking is
qemu's own, not host bridges.

Without `/dev/kvm` it falls back to TCG emulation. That works and is roughly ten
times slower; the runner warns rather than failing.

### How a run is put together

```
build-image.sh   →  cache/rootfs      Alpine + docker, built once, reused
build-binary.sh  →  a musl clouduler  rebuilt from the working tree each run
lib/mkdisk.sh    →  work/<node>.img   rootfs + binary + this run's cluster.yaml
lib/scenario-run.sh                   boots them, runs the steps, tears down
```

The base image is cached because installing Alpine and docker takes minutes.
The clouduler binary is not, so a run always exercises the tree you are sitting
on.

### The binary has to be musl

The guest rootfs is Alpine. A plain `cargo build --release` on a normal
machine produces a glibc binary, and it will not start there: `gcompat` covers
much of glibc but not `libgcc_s.so.1`, not the `_Unwind_*` family, and not
modern glibc's `__isoc23_*` symbols. The symptom is a VM that boots and then
scrolls `Error relocating` up the serial console.

`build-binary.sh` handles it, preferring in order:

1. `$CLOUDULER_BIN`, if you already have a musl binary you want used;
2. `musl-gcc` on `PATH` (`musl-tools` on Debian and Ubuntu, `musl` on Arch),
   which is the fast path;
3. docker, building inside a small `rust:alpine` image.

The rust musl target on its own is not enough, because `ring` and `aws-lc-rs`
have C in them and need a C compiler that targets musl.

### Networking

Each VM gets two interfaces:

- **eth0**, the cluster network. A qemu socket mesh (`-netdev socket,mcast=...`)
  shared by every node in the run, addressed statically from the kernel command
  line. This is what raft, drive sync and the overlay use, and what
  `partition`/`heal` steps cut and restore through QMP `set_link`.
- **eth1**, qemu user-mode networking. Exists so the host can reach each node's
  api on a forwarded `127.0.0.<n>:<port>`, and so image pulls can reach a
  registry. Statically addressed too: slirp hands out one fixed address, so
  there is no reason to run a dhcp client.

Four ports are forwarded per node: the api, the rpc channel, the reverse
proxy's plain http listener, and port 2222, which is the published port the
`publishing` scenario declares. The last two are how an assertion made from the
host can check that a service is actually reachable from outside the cluster
network, which is the entire point of publishing it.

The daemon binds its node-to-node rpc to the cluster address only, deliberately:
that channel is unencrypted and belongs on the trusted network. Since the cli
runs on the host and can only arrive via slirp, the guest's init adds a DNAT
rule redirecting the forwarded port to the cluster address. Widening the
daemon's bind address to make testing easier would have been the wrong trade.

Every node gets its own `127.0.0.<n>` on the host rather than sharing
`127.0.0.1` with different ports, because the cli reads a real cluster config
and a config with two nodes on one address is rejected, correctly.

### Writing a scenario

A scenario is yaml: a topology, optional failover timings, and a list of steps.

```yaml
name: worker-outage
description: a worker dies and its containers land somewhere else.

nodes:
  - { name: n1, role: manager, ip: 10.10.0.1 }
  - { name: n2, role: manager, ip: 10.10.0.2 }
  - { name: n3, role: manager, ip: 10.10.0.3 }
  - { name: n4, role: worker,  ip: 10.10.0.4, drives: { fast: {} } }

failover:
  probe_interval: 2s
  fence_timeout: 6s
  node_timeout: 12s

steps:
  - wait_quorum: { timeout: 40 }
  - deploy_stack: { file: stacks/simple.yaml, name: simple }
  - wait_running: { stack: simple, service: app, timeout: 30 }
  - kill_service_node: { service: app, save_as: victim }
  - assert_unreachable: "${victim}"
  - assert_rescheduled: { service: app, not_on: "${victim}" }
```

`save_as` writes a value a later step can read back as `${name}`, which is how
an assertion refers to whichever node a previous step happened to pick.

The failover timings are deliberately tightened from the defaults. A scenario
that waited 30 seconds for a real `node_timeout` would spend most of its runtime
asleep, and the ordering that matters (fence before reschedule) is preserved.

Step verbs live in `lib/steps.sh`, one `step_<verb>` function each. Adding one
is adding a function; the dispatcher finds it by name.

### Reading a failure

The runner prints the tail of every serial log on failure and leaves the whole
run directory behind:

```
tests/qemu/work/<scenario>-<pid>/
  cluster.yaml      what the nodes were given
  control.yaml      what the cli was given: the same cluster via forwarded ports
  topology.json     names, addresses and ports for this run
  <node>.serial.log the guest console, which is also the daemon's log
```

The daemon logs straight to the serial console, so `<node>.serial.log` is the
first thing to read. Kernel messages, the guest init's own output and the
daemon's tracing output are all in there in order.

## What passes today

`formation`, `leader-failover-write` and `worker-outage` pass. The last one is
the interesting one: it deploys a stack, waits for the container to run, kills
the node running it, and asserts the cluster notices, reschedules the work
elsewhere, and does not end up with two copies once the dead node comes back.

`publishing` is the cover for everything a stack exposes to the outside. It
deploys one stack carrying all three paths and asserts each of them:

- an http route answered through the reverse proxy, asked of **every** node
  rather than the one running the container, since a route that only works
  where the container landed defeats the point;
- a published port forwarded over the overlay, again from every node;
- a second container whose healthcheck fetches `http://web.publish/`, which
  passes only if the internal resolver answered the name and the overlay
  carried the request. That assertion cannot be made from the host: there is
  no exec channel into a guest, so the container's own healthcheck is the test
  and the api's health reporting is how the result gets back out.

ACME issuance is not covered end to end. It needs a Pebble server and a mock
Cloudflare api in the harness; until then only the pure parts (renewal window,
zone selection, leaf expiry parsing) are unit tested.

The remaining scenarios exercise partitions, fencing and volume migration and
are still being brought up. When one fails, the run directory it leaves behind
has every node's console in it; start there.

### Things this suite has already caught

Worth listing, because they are all bugs that unit tests passed straight
through and only a real cluster could show:

- The bootstrap api key was minted once at startup, before any leader existed,
  so the write had nowhere to go and a brand new cluster had **no way in at all**.
- The liveness probe had no timeout. The failure it exists to detect is a node
  vanishing without closing its sockets, where a connect neither succeeds nor
  fails, so the detector blocked for the OS TCP timeout and never marked
  anything unreachable.
- `/api/instances` emitted `running`, the ui read `status` and the harness read
  `state`. Three consumers, three names, no compile error.
- `/api/status` reported the leader as an internal raft id, which is a hash of
  the node name and useless to anyone acting on it.
- VXLAN FDB entries were added with `NLM_F_REPLACE`, which the vxlan driver
  refuses outright, and without `NTF_SELF`, which sends them to the bridge
  driver instead. Head-end replication needs `NLM_F_APPEND`: one entry per peer.
- The overlay's VXLAN device was created with `IFF_UP` set in the same message,
  which the kernel rejects because it opens the device before it is enslaved.
- Containers could not resolve `service.stack`, only the fully qualified
  `service.stack.<domain>`. Docker hands a container `ndots:0`, which makes any
  name containing a dot absolute and skips the search list, so the short name
  went straight upstream and came back NXDOMAIN. Nothing short of a container
  actually asking would have shown this.

## CI

`.github/workflows/ci.yml` runs clippy, the workspace tests, the ui build and
tests, and validates the shipped example configs on every push and pull request.
The qemu suite runs on pushes to the default branch only: it boots VMs, so it is
too slow for the pull request path, and it uploads serial logs as artifacts when
it fails. It runs the four scenarios documented above as passing rather than
`--all`: GitHub's runners have no kvm, so every VM is emulated, and there is no
sense spending that on scenarios that are still being brought up.

The runners also restrict unprivileged user namespaces through apparmor, which
the rootfs build needs (apk chroots for its post-install scripts). The workflow
clears `kernel.apparmor_restrict_unprivileged_userns` before building the
image; `build-image.sh` checks for it up front and says so rather than laying
down an empty rootfs.

Alpine's setuid helpers are installed mode 711, which their own owner cannot
read. Left that way, the cache action's `tar` fails on `bin/bbsuid` and the
rootfs is silently rebuilt every run, so `build-image.sh` makes them
owner-readable. Nothing in the image can use setuid regardless: every file is
owned by the host user, not root.

For examples of deploying *to* a cluster from your own pipeline, see
`examples/ci/`.
