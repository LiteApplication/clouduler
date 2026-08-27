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

# /run is a tmpfs on any real system, and has to be one here too (/var/run is
# a symlink to it). containerd keeps a directory per running container under
# /run/docker, and this image has no boot sequence that clears them: after a
# hard power off the stale ones survive on the ext4 root, and dockerd then
# refuses to start those containers again with
#   failed to start shim: mkdir .../io.containerd.runtime.v2.task/moby/<id>:
#   file exists
# which leaves a restarted node stuck with its containers created but down.
mount -t tmpfs -o mode=0755,nosuid,nodev tmpfs /run

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

# Third interface, present only for the egress-network scenario: a segment
# isolated from both the cluster network and slirp, standing in for a VPN
# tunnel. Only brought up when the scenario asked for it (clvpnip set); left
# alone otherwise so every other scenario's boot is unaffected.
VPN_IFACE="eth2"
VPN_ADDR="$(get_arg clvpnip)"
if [ -n "$VPN_ADDR" ]; then
  for _ in $(seq 1 20); do
    [ -e "/sys/class/net/$VPN_IFACE" ] && break
    sleep 0.2
  done
  if [ -e "/sys/class/net/$VPN_IFACE" ]; then
    ip link set "$VPN_IFACE" up
    ip addr add "$VPN_ADDR" dev "$VPN_IFACE"
    echo "init-clouduler: vpn segment interface $VPN_IFACE up at $VPN_ADDR"
  else
    echo "init-clouduler: WARNING vpn segment interface $VPN_IFACE never appeared"
  fi
fi

# The canary: a fixed-response TCP listener standing in for something only
# reachable over the vpn segment, so a test container can prove which
# interface its traffic actually left through by whether it can reach this.
if [ -n "$(get_arg clvpncanary)" ]; then
  (
    while true; do
      { printf 'HTTP/1.1 200 OK\r\nContent-Length: 13\r\nConnection: close\r\n\r\nvpn-canary-ok'; } \
        | nc -l -p 8080 >/dev/null 2>&1
    done
  ) &
  echo "init-clouduler: vpn canary listening on :8080"
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

# The overlay needs these two, and nothing else loads them: linux-virt builds
# both as modules, and the kernel only autoloads on an interface type it
# already knows. Without vxlan, creating the device fails with ENOTSUP and
# every container on a stack network stays down.
for module in bridge vxlan; do
  modprobe "$module" 2>&1 | sed "s/^/init-clouduler: modprobe $module: /"
  if [ -d "/sys/module/$module" ]; then
    echo "init-clouduler: module $module loaded"
  else
    echo "init-clouduler: WARNING module $module NOT loaded"
  fi
done

# Served drives export over NFSv4. clouduler itself only ever calls
# `exportfs`; the kernel server module and `rpc.mountd` (which handles
# `exportfs`'s export-table upcalls) are this image's job to bring up, the
# same way dockerd is: an external service the daemon depends on and checks
# for, not one it spawns.
modprobe nfsd 2>&1 | sed 's/^/init-clouduler: modprobe nfsd: /'
if [ -d /sys/module/nfsd ]; then
  mountpoint -q /proc/fs/nfsd || mount -t nfsd nfsd /proc/fs/nfsd
  # v4-only: no rpcbind, no rpc.statd/lockd, no separate MOUNT protocol.
  rpc.mountd --no-nfs-version 3 >/var/log/rpc.mountd.log 2>&1 &
  echo "init-clouduler: nfsd module loaded, rpc.mountd started"
else
  echo "init-clouduler: WARNING module nfsd NOT loaded, served drives will not work"
fi

mkdir -p /var/run
# dockerd needs cgroups; mount the essentials it wants under cgroup2.
mount -t cgroup2 none /sys/fs/cgroup 2>/dev/null || true

dockerd --host=unix:///var/run/docker.sock >/var/log/dockerd.log 2>&1 &

echo "init-clouduler: waiting for docker socket"
for _ in $(seq 1 150); do
  [ -S /var/run/docker.sock ] && break
  sleep 0.2
done

# Without docker the node runs nothing, so a failure here has to be visible on
# the console rather than buried in a log file inside a disk image nobody
# mounts. This is the second boot of a node killed mid-write, so it is the
# most likely place for the filesystem or docker's own state to complain.
if [ -S /var/run/docker.sock ]; then
  echo "init-clouduler: docker socket up"
else
  echo "init-clouduler: WARNING docker never came up, last lines of its log:"
  tail -n 30 /var/log/dockerd.log 2>/dev/null | sed 's/^/init-clouduler:   dockerd: /'
fi

echo "init-clouduler: starting clouduler daemon"
# inherit stdio (the serial console) so the host captures clouduler's own
# log output directly via -serial file:..., no guest-side fetch needed.
exec /usr/local/bin/clouduler \
  -c /etc/clouduler/cluster.yaml \
  --data-dir /var/lib/clouduler \
  daemon --node "$NODE"
