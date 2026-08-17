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

# ---- vm lifecycle ---------------------------------------------------------
# Node state lives in $RUN_DIR/<node>.{pid,qmp,serial.log,disk.img,web_port,rpc_port}

declare -a VM_NODES=()

vm_start() {
  local node="$1" ip="$2" web_port="$3" rpc_port="$4" mcast="$5" idx="$6"
  local disk="$RUN_DIR/$node.img"
  local serial="$RUN_DIR/$node.serial.log"
  local pidfile="$RUN_DIR/$node.pid"
  local qmp="$RUN_DIR/$node.qmp"
  local accel
  accel="$(qemu_accel)"

  "$LIB_DIR/mkdisk.sh" "$ROOTFS_CACHE" "$node" "$RUN_DIR/cluster.yaml" "$CLOUDULER_BIN" "$disk"

  local mac0 mac1
  mac0="$(printf '52:54:00:aa:00:%02x' "$idx")"
  mac1="$(printf '52:54:00:bb:00:%02x' "$idx")"

  # shellcheck disable=SC2086
  qemu-system-x86_64 \
    -name "$node" \
    -m 512 -smp 1 \
    -accel "$accel" \
    -kernel "$CACHE_DIR/vmlinuz" -initrd "$CACHE_DIR/initramfs" \
    -append "console=ttyS0 root=/dev/vda rw ip=off clnode=$node clip=$ip/24" \
    -drive file="$disk",if=virtio,format=raw \
    -netdev socket,mcast="$mcast":4321,id=net0 -device virtio-net-pci,netdev=net0,mac="$mac0" \
    -netdev user,id=net1,hostfwd=tcp:127.0.0.1:"$web_port"-:443,hostfwd=tcp:127.0.0.1:"$rpc_port"-:7373 \
    -device virtio-net-pci,netdev=net1,mac="$mac1" \
    -qmp unix:"$qmp",server,nowait \
    -serial file:"$serial" \
    -display none -no-reboot \
    -pidfile "$pidfile" -daemonize

  echo "$web_port" > "$RUN_DIR/$node.web_port"
  echo "$rpc_port" > "$RUN_DIR/$node.rpc_port"
  VM_NODES+=("$node")
  log "started $node (ip=$ip web_port=$web_port accel=$accel)"
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
  local web_port rpc_port
  web_port="$(cat "$RUN_DIR/$node.web_port")"
  rpc_port="$(cat "$RUN_DIR/$node.rpc_port")"
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
    -append "console=ttyS0 root=/dev/vda rw ip=off clnode=$node clip=$ip/24" \
    -drive file="$disk",if=virtio,format=raw \
    -netdev socket,mcast="$mcast":4321,id=net0 -device virtio-net-pci,netdev=net0,mac="$mac0" \
    -netdev user,id=net1,hostfwd=tcp:127.0.0.1:"$web_port"-:443,hostfwd=tcp:127.0.0.1:"$rpc_port"-:7373 \
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

# ---- waiting helpers --------------------------------------------------------

wait_for() {
  local desc="$1" timeout="$2" check="$3"
  local waited=0
  while (( waited < timeout )); do
    if eval "$check"; then
      return 0
    fi
    sleep 1
    (( waited += 1 ))
  done
  die "timed out after ${timeout}s waiting for: $desc"
}

# ---- API / CLI --------------------------------------------------------------
# Auth: clouduler prints a bootstrap api key to the node's serial log once.
# We scrape it from whichever node's log has it first.

api_key() {
  if [[ -z "${API_KEY:-}" ]]; then
    API_KEY="$(grep -ohE 'clk_[A-Za-z0-9_]+' "$RUN_DIR"/*.serial.log 2>/dev/null | head -1)"
    export API_KEY
  fi
  echo "$API_KEY"
}

# api_get <node> <path>
api_get() {
  local node="$1" path="$2" port
  port="$(cat "$RUN_DIR/$node.web_port")"
  curl -sk -m 5 -H "Authorization: Bearer $(api_key)" "https://127.0.0.1:$port$path"
}

api_call() {
  local method="$1" node="$2" path="$3" body="${4:-}" port
  port="$(cat "$RUN_DIR/$node.web_port")"
  curl -sk -m 10 -X "$method" -H "Authorization: Bearer $(api_key)" \
    -H "Content-Type: application/json" \
    ${body:+-d "$body"} "https://127.0.0.1:$port$path"
}

# Runs the clouduler CLI against a control-plane view of the cluster, where
# each node's rpc address is the host-forwarded 127.0.0.1:<port> rather than
# its real cluster ip, so the CLI (run on the host, not inside a VM) can
# reach it. Regenerated once per run into $RUN_DIR/control.yaml.
cli() {
  "$CLOUDULER_BIN" -c "$RUN_DIR/control.yaml" --data-dir "$RUN_DIR/cli-scratch" "$@"
}

any_manager() {
  local n
  for n in $(jq -r '.nodes[] | select(.role=="manager") | .name' "$RUN_DIR/topology.json"); do
    vm_running "$n" && { echo "$n"; return 0; }
  done
  jq -r '.nodes[] | select(.role=="manager") | .name' "$RUN_DIR/topology.json" | head -1
}
