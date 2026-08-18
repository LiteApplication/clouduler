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

# Second interface, qemu's user-mode network. It exists only so the host can
# reach this node's api through the forwarded ports, and slirp hands out one
# fixed address, so there is no reason to run a dhcp client for it. Without an
# address here the daemon still listens on 0.0.0.0 and slirp still accepts the
# host's connection, but it has nowhere to deliver it and every request from
# the host hangs and then fails.
HOST_IFACE="eth1"
HOST_ADDR="10.0.2.15/24"
HOST_ADDR_IP="10.0.2.15"
HOST_GW="10.0.2.2"

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

if [ -e "/sys/class/net/$HOST_IFACE" ]; then
  ip link set "$HOST_IFACE" up
  ip addr add "$HOST_ADDR" dev "$HOST_IFACE"
  # Default route through slirp, so docker image pulls work too.
  ip route add default via "$HOST_GW" dev "$HOST_IFACE" 2>/dev/null || true
  echo "nameserver 10.0.2.3" > /etc/resolv.conf
fi

# The daemon binds its node-to-node rpc to the cluster address only, on purpose:
# that channel is unencrypted and belongs on the trusted network. But the
# harness runs the clouduler cli on the host, which can only reach this guest
# through qemu's user network, and slirp delivers to 10.0.2.15. So redirect it
# here rather than widening the daemon's bind address just for tests.
if [ -n "$ADDR" ] && [ -e "/sys/class/net/$HOST_IFACE" ]; then
  CLUSTER_IP="${ADDR%%/*}"
  modprobe -q nf_nat 2>/dev/null || true
  modprobe -q iptable_nat 2>/dev/null || true
  if iptables -t nat -A PREROUTING -d "$HOST_ADDR_IP" -p tcp --dport 7373 \
       -j DNAT --to-destination "$CLUSTER_IP:7373" 2>/dev/null; then
    # The reply has to come back through the same path, or the host sees a
    # source address it never connected to and drops it.
    iptables -t nat -A POSTROUTING -d "$CLUSTER_IP" -p tcp --dport 7373 \
      -j SNAT --to-source "$HOST_ADDR_IP" 2>/dev/null || true
    echo "init-clouduler: rpc reachable from the host via $HOST_ADDR_IP:7373"
  else
    echo "init-clouduler: WARNING could not redirect rpc, the cli will not reach this node"
  fi
fi

# Printed to the serial log so a failed run shows what the guest's networking
# actually looked like, rather than leaving it to be guessed at.
echo "init-clouduler: interfaces:"
ip -o addr show 2>/dev/null | sed 's/^/init-clouduler:   /'
ip route show 2>/dev/null | sed 's/^/init-clouduler:   route /'

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
