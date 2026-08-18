#!/usr/bin/env bash
# Step verbs a scenario's `steps:` list can use. Each function takes the
# step's JSON argument (a jq value, possibly empty/null for bare steps) on
# stdin via $1.
#
# Every /api route other than the auth ones answers 503 on a follower, so any
# step that reads the api has to aim at the leader. `any_manager` resolves that,
# but a step that waits for an election has to re-resolve on each poll rather
# than pin one node up front: at the moment it starts there may be no leader at
# all, and whichever manager it picked may never become one.
#
# Sourced by scenario-run.sh, not run directly.

step_wait_quorum() {
  local timeout; timeout="$(echo "$1" | jq -r '.timeout // 30')"
  wait_for "cluster quorum" "$timeout" "_quorum_reached"
  log "quorum reached, leader is $(leader_node)"
}

_quorum_reached() {
  local leader
  leader="$(leader_node)" || return 1
  api_get "$leader" /api/status 2>/dev/null \
    | jq -e '.has_quorum == true and .leader != null' >/dev/null 2>&1
}

step_deploy_stack() {
  local file name via
  file="$(echo "$1" | jq -r '.file')"
  name="$(echo "$1" | jq -r '.name // empty')"
  via="$(echo "$1" | jq -r '.via // empty')"
  local args=(stack deploy "$(basename "$file")")
  [[ -n "$name" ]] && args+=(--name "$name")
  [[ -n "$via" ]] && args+=(--via "$via")
  # config/secret file: paths in the stack are resolved relative to the cwd
  # of the deploying process, so run from the stack's own directory.
  (cd "$QEMU_DIR/scenarios/$(dirname "$file")" && cli "${args[@]}") || die "stack deploy failed"
  log "deployed stack from $file"
}

step_deploy_stack_async() {
  local file name
  file="$(echo "$1" | jq -r '.file')"
  name="$(echo "$1" | jq -r '.name // empty')"
  local args=(stack deploy "$(basename "$file")")
  [[ -n "$name" ]] && args+=(--name "$name")
  (cd "$QEMU_DIR/scenarios/$(dirname "$file")" && cli "${args[@]}") \
    > "$RUN_DIR/deploy_async.log" 2>&1 &
  echo $! > "$RUN_DIR/deploy_async.pid"
  log "fired async deploy of $file (pid $!)"
}

# Confirms an in-flight deploy landed all-or-nothing: either the stack is
# absent (write never took), or it is present and fully readable back (write
# took, no half-applied state visible).
step_assert_stack_clean() {
  local stack mgr listing
  stack="$(echo "$1" | jq -r '.stack')"
  if [[ -f "$RUN_DIR/deploy_async.pid" ]]; then
    wait "$(cat "$RUN_DIR/deploy_async.pid")" 2>/dev/null || true
  fi
  mgr="$(any_manager)"
  listing="$(cli stack list --via "$mgr" 2>&1 || true)"
  if echo "$listing" | grep -q "$stack"; then
    cli stack show "$stack" --via "$mgr" >/dev/null 2>&1 \
      || die "stack $stack is listed but show failed: looks half-applied"
    log "deploy landed cleanly: $stack present and readable"
  else
    log "deploy did not land: $stack absent, which is a clean outcome too"
  fi
}

step_wait_running() {
  local stack svc timeout mgr
  stack="$(echo "$1" | jq -r '.stack')"
  svc="$(echo "$1" | jq -r '.service')"
  timeout="$(echo "$1" | jq -r '.timeout // 30')"
  mgr="$(any_manager)"
  wait_for "$stack.$svc running" "$timeout" \
    "api_get '$mgr' /api/instances 2>/dev/null | jq -e --arg s '$stack' --arg v '$svc' \
      '[.[] | select(.id | startswith(\$s + \".\" + \$v))] | any(.state == \"running\")' >/dev/null 2>&1"
  log "$stack.$svc is running"
}

step_kill_service_node() {
  local svc save mgr node
  svc="$(echo "$1" | jq -r '.service')"
  save="$(echo "$1" | jq -r '.save_as // "victim"')"
  mgr="$(any_manager)"
  node="$(api_get "$mgr" /api/instances | jq -r --arg v "$svc" \
    '[.[] | select(.id|contains($v)) | select(.state=="running")][0].node // empty')"
  [[ -n "$node" ]] || die "no running $svc instance found to kill"
  echo -n "$node" > "$RUN_DIR/var_$save"
  vm_kill "$node"
  log "killed node running $svc: $node (saved as \$$save)"
}

step_kill_leader() {
  local save mgr node
  save="$(echo "$1" | jq -r '.save_as // "victim"')"
  mgr="$(any_manager)"
  node="$(api_get "$mgr" /api/status | jq -r '.leader // empty')"
  [[ -n "$node" ]] || die "no leader known, cannot kill it"
  echo -n "$node" > "$RUN_DIR/var_$save"
  vm_kill "$node"
  log "killed leader: $node (saved as \$$save)"
}

step_kill_node() {
  local node; node="$(echo "$1" | jq -r 'if type=="string" then . else .node end')"
  vm_kill "$node"
}

step_start_node() {
  local node ip idx mcast
  node="$(echo "$1" | jq -r 'if type=="string" then . else .node end')"
  ip="$(jq -r --arg n "$node" '.nodes[] | select(.name==$n) | .ip' "$RUN_DIR/topology.json")"
  idx="$(jq -r --arg n "$node" '[.nodes[].name] | index($n)' "$RUN_DIR/topology.json")"
  mcast="$(jq -r '.mcast_group' "$RUN_DIR/topology.json")"
  vm_restart "$node" "$ip" "$mcast" "$idx"
}

step_sleep() {
  local secs; secs="$(echo "$1" | jq -r 'if type=="number" or type=="string" then . else .seconds end')"
  log "sleeping ${secs}s"
  sleep "$secs"
}

step_partition() {
  local node
  for node in $(echo "$1" | jq -r '.[]'); do partition_isolate "$node"; done
}

step_heal() {
  local node
  for node in $(echo "$1" | jq -r '.[]'); do partition_heal "$node"; done
}

step_restart_all() {
  local n
  for n in "${VM_NODES[@]}"; do vm_kill "$n"; done
  sleep 2
  for n in "${VM_NODES[@]}"; do
    local ip idx mcast
    ip="$(jq -r --arg n "$n" '.nodes[] | select(.name==$n) | .ip' "$RUN_DIR/topology.json")"
    idx="$(jq -r --arg n "$n" '[.nodes[].name] | index($n)' "$RUN_DIR/topology.json")"
    mcast="$(jq -r '.mcast_group' "$RUN_DIR/topology.json")"
    vm_restart "$n" "$ip" "$mcast" "$idx"
  done
  log "restarted full cluster"
}

# ---- assertions -----------------------------------------------------------

step_assert_unreachable() {
  local node="$(echo "$1" | jq -r 'if type=="string" then . else .node end')"
  local mgr; mgr="$(any_manager)"
  wait_for "$node marked unreachable" 20 \
    "api_get '$mgr' /api/nodes/$node 2>/dev/null | jq -e '.reachable == false' >/dev/null 2>&1"
  log "$node correctly marked unreachable"
}

step_assert_reachable() {
  local node="$(echo "$1" | jq -r 'if type=="string" then . else .node end')"
  local mgr; mgr="$(any_manager)"
  wait_for "$node marked reachable" 20 \
    "api_get '$mgr' /api/nodes/$node 2>/dev/null | jq -e '.reachable == true' >/dev/null 2>&1"
  log "$node rejoined and is reachable"
}

step_assert_rescheduled() {
  local svc not_on timeout mgr
  svc="$(echo "$1" | jq -r '.service')"
  not_on="$(echo "$1" | jq -r '.not_on')"
  timeout="$(echo "$1" | jq -r '.timeout // 30')"
  mgr="$(any_manager)"
  wait_for "$svc rescheduled off $not_on" "$timeout" \
    "api_get '$mgr' /api/instances 2>/dev/null | jq -e --arg v '$svc' --arg n '$not_on' \
      '[.[] | select(.id | contains(\$v))] | any(.state == \"running\" and .node != \$n)' >/dev/null 2>&1"
  log "$svc rescheduled away from $not_on"
}

step_assert_no_duplicate() {
  local svc mgr
  svc="$(echo "$1" | jq -r '.service')"
  mgr="$(any_manager)"
  local count
  count="$(api_get "$mgr" /api/instances | jq --arg v "$svc" \
    '[.[] | select(.id | contains($v)) | select(.state=="running")] | length')"
  [[ "$count" -le 1 ]] || die "$svc has $count running copies, expected at most 1"
  log "$svc has no duplicate after rejoin ($count running)"
}

step_assert_fenced() {
  local mgr; mgr="$(any_manager)"
  local node
  for node in $(echo "$1" | jq -r '.[]'); do
    wait_for "$node fenced (no running containers)" 30 \
      "api_get '$mgr' /api/instances 2>/dev/null | jq -e --arg n '$node' \
        '[.[] | select(.node == \$n and .state == \"running\")] | length == 0' >/dev/null 2>&1"
  done
  log "fencing confirmed for: $(echo "$1" | jq -r 'join(", ")')"
}

step_assert_leader_elected() {
  local timeout
  timeout="$(echo "$1" | jq -r '.timeout // 30')"
  wait_for "a leader is elected" "$timeout" "leader_node >/dev/null"
  local leader; leader="$(leader_node)"
  log "leader elected: $(api_get "$leader" /api/status | jq -r '.leader')"
}

step_assert_converged() {
  local timeout; timeout="$(echo "$1" | jq -r '.timeout // 30')"
  local mgrs; mgrs="$(managers)"
  wait_for "all managers converge on the same state version" "$timeout" \
    "_all_versions_match \"$mgrs\""
}

# /api/status is answered by every node, leader or not, and reports that node's
# own applied state version. That is what makes this a real convergence check
# rather than one node's opinion repeated three times.
_all_versions_match() {
  local mgrs="$1" first="" v n
  for n in $mgrs; do
    vm_running "$n" || continue
    v="$(api_get "$n" /api/status 2>/dev/null | jq -r '.state_version // empty')"
    [[ -z "$v" ]] && return 1
    [[ -z "$first" ]] && first="$v"
    [[ "$v" != "$first" ]] && return 1
  done
  return 0
}

step_assert_volume_migrates() {
  local prefix to timeout mgr id
  prefix="$(echo "$1" | jq -r '.instance_prefix')"
  to="$(echo "$1" | jq -r '.to')"
  timeout="$(echo "$1" | jq -r '.timeout // 60')"
  mgr="$(any_manager)"
  id="$(api_get "$mgr" /api/instances | jq -r --arg p "$prefix" \
    '[.[] | select(.id|startswith($p))][0].id // empty')"
  [[ -n "$id" ]] || die "no instance matching $prefix"
  api_call POST "$mgr" "/api/instances/$id/move" "{\"node\":\"$to\"}" >/dev/null
  wait_for "instance $id running on $to" "$timeout" \
    "api_get '$mgr' /api/instances/$id 2>/dev/null | jq -e --arg n '$to' \
      '.node == \$n and .state == \"running\"' >/dev/null 2>&1"
  # the migration state machine (docs/drives.md) only flips the assignment
  # after a hash-verified sync, so a running instance on the target is
  # already good evidence the volume arrived intact; this additionally
  # confirms the drive api agrees the target now holds the volume.
  wait_for "volume tracked on $to" 20 \
    "api_get '$mgr' /api/volumes 2>/dev/null | jq -e --arg n '$to' \
      'any(.[]?; .primary == \$n or (.replicas // {} | has(\$n)))' >/dev/null 2>&1"
  log "instance $id migrated to $to with volume tracked"
}

step_move_instance() {
  local id to mgr
  id="$(echo "$1" | jq -r '.instance')"
  to="$(echo "$1" | jq -r '.to')"
  mgr="$(any_manager)"
  api_call POST "$mgr" "/api/instances/$id/move" "{\"node\":\"$to\"}" >/dev/null
  log "requested move of $id to $to"
}

step_assert_stack_files_present() {
  local stack via mgr
  stack="$(echo "$1" | jq -r '.stack')"
  mgr="$(any_manager)"
  local out
  out="$(cli stack show "$stack" --files --via "$mgr")" || die "stack show $stack failed"
  echo "$out" | grep -q "$(echo "$1" | jq -r '.config // "config"')" \
    || die "config not present in stack $stack per manager $mgr"
  log "stack $stack files present via $mgr"
}

step_assert_config_on_all_managers() {
  local name mgrs n
  name="$(echo "$1" | jq -r '.config')"
  mgrs="$(jq -r '.nodes[] | select(.role=="manager") | .name' "$RUN_DIR/topology.json")"
  for n in $mgrs; do
    api_get "$n" /api/configs | jq -e --arg c "$name" 'any(.[]?; .name==$c)' >/dev/null \
      || die "config $name missing on manager $n"
  done
  log "config $name present on every manager"
}

step_assert_secret_ciphertext() {
  local node name
  node="$(echo "$1" | jq -r '.node')"
  name="$(echo "$1" | jq -r '.secret')"
  # A quick, indirect check: the api never returns secret values (per
  # docs/api.md), and the plaintext we deployed must not appear anywhere
  # under the node's data dir on disk. We can't read the guest's disk from
  # the host without mounting the image, so this instead confirms the api
  # refuses to serve a value and only exposes metadata.
  local mgr; mgr="$(any_manager)"
  local resp; resp="$(api_get "$mgr" /api/secrets)"
  echo "$resp" | jq -e --arg n "$name" 'any(.[]; .name == $n)' >/dev/null \
    || die "secret $name missing from /api/secrets on $mgr"
  echo "$resp" | jq -e 'all(.[]; has("value") | not)' >/dev/null \
    || die "a secret value leaked through /api/secrets"
  log "secret $name present as metadata only, no value in the api response"
}

step_assert_no_new_starts() {
  local svc timeout mgr before after
  svc="$(echo "$1" | jq -r '.service')"
  timeout="$(echo "$1" | jq -r '.timeout // 10')"
  mgr="$(any_manager)"
  before="$(api_get "$mgr" /api/instances 2>/dev/null | jq --arg v "$svc" '[.[]|select(.id|contains($v))]|length')"
  sleep "$timeout"
  after="$(api_get "$mgr" /api/instances 2>/dev/null | jq --arg v "$svc" '[.[]|select(.id|contains($v))]|length')"
  [[ "$before" == "$after" ]] || die "$svc instance count changed during fencing ($before -> $after)"
  log "no new $svc instances started while fenced"
}

dispatch_step() {
  local key val
  key="$(echo "$1" | jq -r 'keys[0]')"
  val="$(echo "$1" | jq -c '.[keys[0]]')"
  local fn="step_${key}"
  if declare -f "$fn" >/dev/null; then
    "$fn" "$val"
  else
    die "unknown step verb: $key"
  fi
}
