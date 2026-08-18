#!/usr/bin/env bash
# Drives a single scenario: builds the run's cluster config, boots the VMs
# named in it, runs the step list, tears everything down (always, even on
# failure), and prints a pass/fail summary with logs on failure.
#
# Usage: scenario-run.sh <scenario-file> <run-dir>
set -euo pipefail

SCENARIO_FILE="$1"
RUN_DIR="$2"
export RUN_DIR

QEMU_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=lib/harness.sh
source "$QEMU_DIR/lib/harness.sh"
# shellcheck source=lib/steps.sh
source "$QEMU_DIR/lib/steps.sh"

mkdir -p "$RUN_DIR"

# Must be a musl binary, since the guest rootfs is Alpine. build-binary.sh
# works out how to produce one; see its header for the ways it can.
CLOUDULER_BIN="$("$QEMU_DIR/build-binary.sh")" || die "could not build the clouduler binary"
export CLOUDULER_BIN

MCAST_ID=$(( (RANDOM % 200) + 1 ))

RESULT="FAIL"
teardown() {
  local status=$?
  log "tearing down VMs for $(basename "$SCENARIO_FILE")"
  vm_teardown_all
  if [[ "$RESULT" != "PASS" ]]; then
    echo "----- serial logs (tail) -----"
    for f in "$RUN_DIR"/*.serial.log; do
      [[ -f "$f" ]] || continue
      echo "== $f =="
      tail -n 40 "$f" || true
    done
  fi
  exit "$status"
}
trap teardown EXIT

log "preparing scenario $(basename "$SCENARIO_FILE")"
python3 "$QEMU_DIR/lib/scenario_prep.py" "$SCENARIO_FILE" \
  "$RUN_DIR/cluster.yaml" "$RUN_DIR/control.yaml" "$MCAST_ID" > "$RUN_DIR/topology.json"

"$CLOUDULER_BIN" -c "$RUN_DIR/cluster.yaml" check \
  || die "generated cluster.yaml failed clouduler check, scenario file may be malformed"

mkdir -p "$RUN_DIR/cli-scratch"

log "booting $(jq '.nodes | length' "$RUN_DIR/topology.json") nodes"
idx=0
while IFS=$'\t' read -r name ip web_port rpc_port host_ip; do
  vm_start "$name" "$ip" "$web_port" "$rpc_port" \
    "$(jq -r '.mcast_group' "$RUN_DIR/topology.json")" "$idx" "$host_ip"
  idx=$((idx + 1))
done < <(jq -r '.nodes[] | [.name, .ip, .web_port, .rpc_port, .control_ip] | @tsv' "$RUN_DIR/topology.json")

log "waiting for daemons to come up"
sleep 5

log "running $(jq '.steps | length' "$RUN_DIR/topology.json") steps"
while IFS= read -r step; do
  # substitute ${name} placeholders with values saved by earlier steps
  # (e.g. kill_service_node saving which node it killed).
  for vf in "$RUN_DIR"/var_*; do
    [[ -f "$vf" ]] || continue
    vname="${vf##*/var_}"
    vval="$(cat "$vf")"
    step="${step//\$\{$vname\}/$vval}"
  done
  log "step: $(echo "$step" | jq -c .)"
  dispatch_step "$step"
done < <(jq -c '.steps[]' "$RUN_DIR/topology.json")

RESULT="PASS"
log "PASS: $(jq -r '.name' "$RUN_DIR/topology.json")"
