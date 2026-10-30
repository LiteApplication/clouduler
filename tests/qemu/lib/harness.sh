#!/usr/bin/env bash
# Shared functions for scenario runners: VM lifecycle, network fault
# injection, and HTTP/CLI assertions against a booted cluster. Sourced, not
# executed directly.

QEMU_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LIB_DIR="$QEMU_DIR/lib"
CACHE_DIR="${QEMU_TEST_CACHE:-$QEMU_DIR/cache}"
ROOTFS_CACHE="$CACHE_DIR/rootfs"

log()  { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"; }
die()  { log "FAIL: $*"; exit 1; }

# ---- accel / kvm detection ----------------------------------------------

qemu_accel() {
  if [[ -r /dev/kvm && -w /dev/kvm ]]; then
    echo "kvm"
  else
    log "WARNING: /dev/kvm not usable, falling back to TCG emulation (much slower)"
    echo "tcg"
  fi
}

# ---- boot ------------------------------------------------------------------

# Alpine's initramfs, not the kernel, is what mounts our root and hands off, so
# it needs telling three things the defaults get wrong:
#
#   modules=     ext4 and virtio_blk are modules in linux-virt, and nothing
#                loads them on its own. Without these the mount fails with
#                "No such file or directory" and boot drops to a rescue shell.
#   rootfstype=  what to pass to mount -t.
#   init=        our own PID 1 (lib/guest-init.sh), instead of openrc, which
#                the image deliberately does not install.
#
# clnode/clip are read back off /proc/cmdline by that init.
kernel_cmdline() {
  local node="$1" ip="$2" vpn_ip="${3:-}" vpn_canary="${4:-}"
  local extra=""
  [[ -n "$vpn_ip" ]] && extra="$extra clvpnip=$vpn_ip"
  [[ -n "$vpn_canary" ]] && extra="$extra clvpncanary=1"
  echo "console=ttyS0 root=/dev/vda rootfstype=ext4" \
       "modules=virtio_pci,virtio_blk,virtio_net,ext4" \
       "init=/sbin/init-clouduler ip=off clnode=$node clip=$ip/24$extra"
}

# ---- vm lifecycle ---------------------------------------------------------
# Node state lives in $RUN_DIR/<node>.{pid,qmp,serial.log,img,web_port,rpc_port,
# http_port,expose_port,host_ip}

# Guest side ports forwarded to the host on top of the api and the rpc channel:
# the reverse proxy's plain listener, and the published port the publishing
# scenario declares. Both are how a published service is reached from outside
# the cluster network, which is the whole point of them.
GUEST_HTTP_PORT=80
GUEST_EXPOSE_PORT=2222

declare -a VM_NODES=()

# The host address the VMs' multicast segments are bound to. Without it qemu
# lets the kernel pick the default-route interface, whose address can be one a
# host firewall drops: tailscale's ts-input chain discards anything sourced
# from 100.64.0.0/10 that does not arrive on tailscale0, which is exactly what
# a wifi interface on a CGNAT network sends, and the VMs then never see each
# other. Every VM runs on this host, so loopback is always enough.
MCAST_LOCALADDR="${QEMU_MCAST_LOCALADDR:-127.0.0.1}"

vm_start() {
  local node="$1" ip="$2" web_port="$3" rpc_port="$4" mcast="$5" idx="$6" host_ip="$7"
  local http_port="$8" expose_port="$9"
  # Optional, only set by scenarios using the vpn-segment (see
  # scenario_prep.py's "vpn_segment" handling): a third netdev standing in
  # for a VPN tunnel. Empty vpn_mcast means no third device at all, so every
  # other scenario's VM shape is unchanged.
  local vpn_ip="${10:-}" vpn_canary="${11:-}" vpn_mcast="${12:-}"
  local disk="$RUN_DIR/$node.img"
  local serial="$RUN_DIR/$node.serial.log"
  local pidfile="$RUN_DIR/$node.pid"
  local qmp="$RUN_DIR/$node.qmp"
  local accel
  accel="$(qemu_accel)"

  "$LIB_DIR/mkdisk.sh" "$ROOTFS_CACHE" "$node" "$RUN_DIR/cluster.yaml" "$CLOUDULER_BIN" "$disk" \
    "$RUN_DIR/secrets.key"

  local mac0 mac1 mac2
  mac0="$(printf '52:54:00:aa:00:%02x' "$idx")"
  mac1="$(printf '52:54:00:bb:00:%02x' "$idx")"
  mac2="$(printf '52:54:00:cc:00:%02x' "$idx")"

  local -a vpn_args=()
  if [[ -n "$vpn_mcast" ]]; then
    vpn_args=(-netdev "socket,mcast=$vpn_mcast:4321,localaddr=$MCAST_LOCALADDR,id=net2" -device "virtio-net-pci,netdev=net2,mac=$mac2")
  fi

  # shellcheck disable=SC2086
  qemu-system-x86_64 \
    -name "$node" \
    -m 512 -smp 1 \
    -accel "$accel" \
    -kernel "$CACHE_DIR/vmlinuz" -initrd "$CACHE_DIR/initramfs" \
    -append "$(kernel_cmdline "$node" "$ip" "$vpn_ip" "$vpn_canary")" \
    -drive file="$disk",if=virtio,format=raw \
    -netdev socket,mcast="$mcast":4321,localaddr="$MCAST_LOCALADDR",id=net0 -device virtio-net-pci,netdev=net0,mac="$mac0" \
    -netdev user,id=net1,hostfwd=tcp:"$host_ip":"$web_port"-:8443,hostfwd=tcp:"$host_ip":"$rpc_port"-:7373,hostfwd=tcp:"$host_ip":"$http_port"-:"$GUEST_HTTP_PORT",hostfwd=tcp:"$host_ip":"$expose_port"-:"$GUEST_EXPOSE_PORT" \
    -device virtio-net-pci,netdev=net1,mac="$mac1" \
    "${vpn_args[@]}" \
    -qmp unix:"$qmp",server,nowait \
    -serial file:"$serial" \
    -display none -no-reboot \
    -pidfile "$pidfile" -daemonize

  echo "$web_port" > "$RUN_DIR/$node.web_port"
  echo "$rpc_port" > "$RUN_DIR/$node.rpc_port"
  echo "$http_port" > "$RUN_DIR/$node.http_port"
  echo "$expose_port" > "$RUN_DIR/$node.expose_port"
  echo "$host_ip" > "$RUN_DIR/$node.host_ip"
  VM_NODES+=("$node")
  log "started $node (ip=$ip api=$host_ip:$web_port accel=$accel)"
}

vm_pid() { cat "$RUN_DIR/$1.pid" 2>/dev/null || true; }

vm_running() {
  local pid
  pid="$(vm_pid "$1")"
  [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null
}

# Hard kill: simulates a node dying outright (power loss), not a graceful stop.
vm_kill() {
  local node="$1" pid
  pid="$(vm_pid "$node")"
  if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
    kill -KILL "$pid"
    log "killed $node (pid $pid)"
  fi
}

# Restart a previously killed node from the same disk image, so its data
# directory and database survive, matching a real reboot.
vm_restart() {
  local node="$1" ip="$2" mcast="$3" idx="$4"
  local web_port rpc_port host_ip http_port expose_port
  web_port="$(cat "$RUN_DIR/$node.web_port")"
  rpc_port="$(cat "$RUN_DIR/$node.rpc_port")"
  http_port="$(cat "$RUN_DIR/$node.http_port")"
  expose_port="$(cat "$RUN_DIR/$node.expose_port")"
  host_ip="$(cat "$RUN_DIR/$node.host_ip")"
  local disk="$RUN_DIR/$node.img"
  local qmp="$RUN_DIR/$node.qmp"
  local pidfile="$RUN_DIR/$node.pid"
  local accel
  accel="$(qemu_accel)"
  local mac0 mac1
  mac0="$(printf '52:54:00:aa:00:%02x' "$idx")"
  mac1="$(printf '52:54:00:bb:00:%02x' "$idx")"

  rm -f "$qmp" "$pidfile"
  qemu-system-x86_64 \
    -name "$node" \
    -m 512 -smp 1 \
    -accel "$accel" \
    -kernel "$CACHE_DIR/vmlinuz" -initrd "$CACHE_DIR/initramfs" \
    -append "$(kernel_cmdline "$node" "$ip")" \
    -drive file="$disk",if=virtio,format=raw \
    -netdev socket,mcast="$mcast":4321,localaddr="$MCAST_LOCALADDR",id=net0 -device virtio-net-pci,netdev=net0,mac="$mac0" \
    -netdev user,id=net1,hostfwd=tcp:"$host_ip":"$web_port"-:8443,hostfwd=tcp:"$host_ip":"$rpc_port"-:7373,hostfwd=tcp:"$host_ip":"$http_port"-:"$GUEST_HTTP_PORT",hostfwd=tcp:"$host_ip":"$expose_port"-:"$GUEST_EXPOSE_PORT" \
    -device virtio-net-pci,netdev=net1,mac="$mac1" \
    -qmp unix:"$qmp",server,nowait \
    -serial file:"$RUN_DIR/$node.serial.log" \
    -display none -no-reboot \
    -pidfile "$pidfile" -daemonize
  log "restarted $node"
}

vm_teardown_all() {
  local n
  for n in "${VM_NODES[@]}"; do
    vm_kill "$n" 2>/dev/null || true
  done
}

# ---- network fault injection (QMP set_link) --------------------------------

vm_link_set() {
  local node="$1" up="$2" qmp="$RUN_DIR/$node.qmp"
  [[ -S "$qmp" ]] || { log "no qmp socket for $node, skipping link change"; return 0; }
  python3 "$LIB_DIR/qmp.py" "$qmp" \
    "{\"execute\":\"set_link\",\"arguments\":{\"name\":\"net0\",\"up\":$up}}" >/dev/null
}

partition_isolate() { vm_link_set "$1" false; log "isolated $1 from the cluster network"; }
partition_heal()    { vm_link_set "$1" true;  log "healed $1 back onto the cluster network"; }

# Same trick, targeting the vpn-segment netdev (net2) instead of the cluster
# network (net0): simulates the tunnel interface a node's egress network NATs
# through going down, without touching cluster membership at all.
vpn_link_set() {
  local node="$1" up="$2" qmp="$RUN_DIR/$node.qmp"
  [[ -S "$qmp" ]] || { log "no qmp socket for $node, skipping vpn link change"; return 0; }
  python3 "$LIB_DIR/qmp.py" "$qmp" \
    "{\"execute\":\"set_link\",\"arguments\":{\"name\":\"net2\",\"up\":$up}}" >/dev/null
}

vpn_down() { vpn_link_set "$1" false; log "brought down $1's vpn segment link"; }
vpn_up()   { vpn_link_set "$1" true;  log "brought up $1's vpn segment link"; }

# ---- waiting helpers --------------------------------------------------------

# Deadline based, not iteration based. Every check here makes http calls with
# their own timeouts, so counting loops counted something that was not seconds:
# a 40 second wait could sit there for ten minutes before giving up.
wait_for() {
  local desc="$1" timeout="$2" check="$3"
  local deadline=$(( SECONDS + timeout ))
  while (( SECONDS < deadline )); do
    if eval "$check"; then
      return 0
    fi
    sleep 1
  done
  die "timed out after ${timeout}s waiting for: $desc"
}

# ---- API / CLI --------------------------------------------------------------
# Auth: clouduler prints a bootstrap api key to the node's serial log once.
# We scrape it from whichever node's log has it first.

# The bootstrap key is printed to a node's serial log exactly once, and
# `-serial file:` truncates that log when the node restarts, so it has to be
# kept somewhere else the moment it is seen. A shell variable is not enough:
# most callers reach this from inside a command substitution, whose exports
# die with the subshell, and the next call would find the log already gone.
api_key() {
  local cached="$RUN_DIR/api_key"
  if [[ ! -s "$cached" ]]; then
    grep -ohE 'clk_[A-Za-z0-9_]+' "$RUN_DIR"/*.serial.log 2>/dev/null | head -1 > "$cached"
  fi
  cat "$cached"
}

# The host address a node's forwarded port answers on. `kind` is one of the
# port files written at boot: web_port, rpc_port, http_port, expose_port.
host_addr() {
  local node="$1" kind="$2"
  echo "$(cat "$RUN_DIR/$node.host_ip"):$(cat "$RUN_DIR/$node.$kind")"
}

# api_get <node> <path>
api_get() {
  local node="$1" path="$2" port host
  port="$(cat "$RUN_DIR/$node.web_port")"
  host="$(cat "$RUN_DIR/$node.host_ip")"
  curl -sk -m 5 -H "Authorization: Bearer $(api_key)" "https://$host:$port$path"
}

api_call() {
  local method="$1" node="$2" path="$3" body="${4:-}" port host
  port="$(cat "$RUN_DIR/$node.web_port")"
  host="$(cat "$RUN_DIR/$node.host_ip")"
  curl -sk -m 10 -X "$method" -H "Authorization: Bearer $(api_key)" \
    -H "Content-Type: application/json" \
    ${body:+-d "$body"} "https://$host:$port$path"
}

# Runs the clouduler CLI against a control-plane view of the cluster, where
# each node's rpc address is its own host-forwarded 127.0.0.<n>:<port> rather
# than its real cluster ip, so the CLI (run on the host, not inside a VM) can
# reach it. Regenerated once per run into $RUN_DIR/control.yaml.
cli() {
  "$CLOUDULER_BIN" --config "$RUN_DIR/control.yaml" --data-dir "$RUN_DIR/cli-scratch" "$@"
}

managers() {
  jq -r '.nodes[] | select(.role=="manager") | .name' "$RUN_DIR/topology.json"
}

# Every /api route except the auth ones answers 503 on a follower, so an
# assertion aimed at the wrong manager reads as "no leader yet" and eventually
# times out for the wrong reason. Find the node that actually leads.
leader_node() {
  local n
  for n in $(managers); do
    vm_running "$n" || continue
    if api_get "$n" /api/status 2>/dev/null | jq -e '.is_leader == true' >/dev/null 2>&1; then
      echo "$n"
      return 0
    fi
  done
  return 1
}

# The node to aim an api call at: the leader when there is one, otherwise any
# manager still up, so a caller waiting for an election has something to poll.
any_manager() {
  local n
  leader_node && return 0
  for n in $(managers); do
    vm_running "$n" && { echo "$n"; return 0; }
  done
  managers | head -1
}
