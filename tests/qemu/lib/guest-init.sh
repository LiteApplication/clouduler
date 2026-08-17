#!/bin/sh
# PID 1 inside the test VM (booted with init=/sbin/init-clouduler). Brings up
# the minimum needed to run clouduler: proc/sys/dev, a static network address,
# and dockerd, then execs the clouduler daemon so it inherits PID 1's console.
#
# Node identity and network address are read from the kernel command line:
#   clnode=<name> clip=<addr>/<prefix>
# both written by lib/mkdisk.sh into the per-node QEMU -append string.
set -e

# the kernel mounts root read-only before handing off to init; alpine's own
# init normally remounts rw as part of its boot sequence, which we skip.
mount -o remount,rw /

mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev 2>/dev/null || true
mkdir -p /dev/pts /dev/shm
mount -t devpts devpts /dev/pts 2>/dev/null || true

CMDLINE="$(cat /proc/cmdline)"
get_arg() {
  # shellcheck disable=SC2086
  for tok in $CMDLINE; do
    case "$tok" in
      "$1"=*) echo "${tok#"$1"=}"; return 0 ;;
    esac
  done
}

NODE="$(get_arg clnode)"
ADDR="$(get_arg clip)"
IFACE="eth0"

echo "init-clouduler: node=$NODE addr=$ADDR"
hostname "${NODE:-unknown}"

# bring up loopback and the cluster network interface
ip link set lo up
if [ -n "$ADDR" ]; then
  # wait briefly for virtio-net to enumerate
  for _ in $(seq 1 20); do
    [ -e "/sys/class/net/$IFACE" ] && break
    sleep 0.2
  done
  ip link set "$IFACE" up
  ip addr add "$ADDR" dev "$IFACE"
fi

mkdir -p /var/run
# dockerd needs cgroups; mount the essentials it wants under cgroup2.
mount -t cgroup2 none /sys/fs/cgroup 2>/dev/null || true

dockerd --host=unix:///var/run/docker.sock >/var/log/dockerd.log 2>&1 &

echo "init-clouduler: waiting for docker socket"
for _ in $(seq 1 50); do
  [ -S /var/run/docker.sock ] && break
  sleep 0.2
done

echo "init-clouduler: starting clouduler daemon"
# inherit stdio (the serial console) so the host captures clouduler's own
# log output directly via -serial file:..., no guest-side fetch needed.
exec /usr/local/bin/clouduler \
  -c /etc/clouduler/cluster.yaml \
  --data-dir /var/lib/clouduler \
  daemon --node "$NODE"
