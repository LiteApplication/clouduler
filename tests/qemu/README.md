# QEMU integration harness

Boots real, separately-networked VMs to exercise clouduler's failover,
replication and scheduling behavior end to end. See `../../docs/testing.md`
for the full writeup; this file is a quick orientation for people already
in the directory.

## Layout

```
tests/qemu/
  build-image.sh      builds the Alpine test rootfs + injects the clouduler binary
  run.sh               entry point: run one scenario or the whole suite
  lib/                 shared bash: vm lifecycle, network wiring, http/cli assertions
  scenarios/           one file per scenario (bash, sourcing lib/harness.sh)
  build-binary.sh      builds the musl clouduler binary the VMs run
  cache/               cached base image (git-ignored)
  work/                per-run scratch: disks, sockets, logs, pids (git-ignored)
```

## Quick start

```
tests/qemu/build-image.sh        # once, or after changing the base image
tests/qemu/run.sh formation      # one scenario
tests/qemu/run.sh --all          # the whole suite
```

Requires `qemu-system-x86_64`, `qemu-img`, `mke2fs`, `unshare`, `jq`, and
`python3` with PyYAML. No root: the rootfs is built and packed inside a user
namespace and the networking is qemu's own. The binary the VMs run has to be
linked against musl, which needs either `musl-gcc` on PATH or docker;
`build-binary.sh` sorts that out and explains itself. Works without KVM (TCG)
but is much slower; the runner detects `/dev/kvm` and warns rather than failing
when it's absent.
