#!/usr/bin/env bash
# Entry point for the qemu integration suite.
#
# Usage:
#   run.sh <scenario-name>     run one scenario (tests/qemu/scenarios/<name>.yaml)
#   run.sh --all               run every scenario in tests/qemu/scenarios, in order
#   run.sh --list              list available scenarios
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCENARIOS_DIR="$SCRIPT_DIR/scenarios"
WORK_DIR="${QEMU_TEST_WORK:-$SCRIPT_DIR/work}"

usage() {
  echo "usage: $0 <scenario-name>|--all|--list" >&2
  exit 1
}

list_scenarios() {
  find "$SCENARIOS_DIR" -maxdepth 1 -name '*.yaml' -printf '%f\n' | sed 's/\.yaml$//' | sort
}

[[ $# -ge 1 ]] || usage

if ! command -v qemu-system-x86_64 >/dev/null 2>&1; then
  echo "run.sh: qemu-system-x86_64 not found on PATH" >&2
  exit 1
fi
if [[ ! -f "$SCRIPT_DIR/cache/vmlinuz" ]]; then
  echo "run.sh: no cached base image found, run tests/qemu/build-image.sh first" >&2
  exit 1
fi

run_one() {
  local name="$1"
  local scenario_file="$SCENARIOS_DIR/$name.yaml"
  [[ -f "$scenario_file" ]] || { echo "run.sh: no such scenario: $name" >&2; return 1; }
  local run_dir="$WORK_DIR/$name-$$"
  mkdir -p "$run_dir"
  echo "=== running scenario: $name ==="
  if "$SCRIPT_DIR/lib/scenario-run.sh" "$scenario_file" "$run_dir"; then
    echo "=== PASS: $name ==="
    # Each node's disk image is up to 1.5G and nothing here reuses it once
    # the scenario is done, so a --all run that kept them all would fill
    # the disk long before the last scenario runs. Nothing worth keeping
    # from a passing run, so drop the whole directory.
    rm -rf "$run_dir"
    return 0
  else
    echo "=== FAIL: $name (logs in $run_dir) ==="
    # Keep the small state/log files for diagnosis but drop the disk
    # images, which are the only thing here large enough to matter for
    # disk space and are useless without a live VM to attach them to.
    rm -f "$run_dir"/*.img
    return 1
  fi
}

case "$1" in
  --list)
    list_scenarios
    ;;
  --all)
    fail=0
    declare -a results=()
    while IFS= read -r name; do
      if run_one "$name"; then
        results+=("PASS  $name")
      else
        results+=("FAIL  $name")
        fail=1
      fi
    done < <(list_scenarios)
    echo
    echo "----- summary -----"
    printf '%s\n' "${results[@]}"
    exit "$fail"
    ;;
  -h|--help)
    usage
    ;;
  *)
    run_one "$1"
    ;;
esac
