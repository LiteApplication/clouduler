#!/usr/bin/env bash
# Step verbs a scenario's `steps:` list can use. Each function takes the
# step's JSON argument (a jq value, possibly empty/null for bare steps) on
# stdin via $1.
#
# Reads are answered by any node, writes only by the leader. `any_manager`
# resolves the leader for writes; a step that waits for an election has to
# re-resolve on each poll rather than pin one node up front, since at the moment
# it starts there may be no leader at all.
#
# Live container state is the exception to "any node will do": only the node
# running a container can see it, so `instances_everywhere` asks all of them and
# merges, rather than trusting the leader's `unknown` for everyone else's work.
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

# Every instance every running node can see, as one json array. An instance
# shows up once per node that knows of it, with only its own node reporting a
# real state, so callers filter on state rather than deduplicating.
instances_everywhere() {
  local n
  for n in $(jq -r '.nodes[].name' "$RUN_DIR/topology.json"); do
    vm_running "$n" || continue
    api_get "$n" /api/instances 2>/dev/null
  done | jq -s 'add // []'
}

# A container is up to three sweeps away from running, and the sweeps belong to
# two independent 10s loops: the leader records the placement, the next overlay
# pass gives the instance its address, and only the pass after that can create
# the container. Add the image pull on top, so anything under ~40s here fails on
# tick phase rather than on the behaviour under test.
step_wait_running() {
  local stack svc timeout
  stack="$(echo "$1" | jq -r '.stack')"
  svc="$(echo "$1" | jq -r '.service')"
  timeout="$(echo "$1" | jq -r '.timeout // 90')"
  wait_for "$stack.$svc running" "$timeout" \
    "instances_everywhere | jq -e --arg s '$stack' --arg v '$svc' \
      '[.[] | select(.id | startswith(\$s + \".\" + \$v))] | any(.state == \"running\")' >/dev/null 2>&1"
  log "$stack.$svc is running"
}

step_kill_service_node() {
  local svc save mgr node
  svc="$(echo "$1" | jq -r '.service')"
  save="$(echo "$1" | jq -r '.save_as // "victim"')"
  mgr="$(any_manager)"
  node="$(instances_everywhere | jq -r --arg v "$svc" \
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

# vpn_down/vpn_up: ["n1"] or "n1". Brings the vpn-segment link (not the
# cluster network) up or down on the named node(s), to prove an egress
# network's kill-switch has no fallback when its interface disappears.
step_vpn_down() {
  local node
  for node in $(echo "$1" | jq -r 'if type=="array" then .[] else . end'); do vpn_down "$node"; done
}

step_vpn_up() {
  local node
  for node in $(echo "$1" | jq -r 'if type=="array" then .[] else . end'); do vpn_up "$node"; done
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
  local timeout; timeout="$(echo "$1" | jq -r 'if type=="object" then (.timeout // 45) else 45 end')"
  local mgr; mgr="$(any_manager)"
  wait_for "$node marked unreachable" "$timeout" \
    "api_get '$mgr' /api/nodes/$node 2>/dev/null | jq -e '.reachable == false' >/dev/null 2>&1"
  log "$node correctly marked unreachable"
}

step_assert_reachable() {
  local node="$(echo "$1" | jq -r 'if type=="string" then . else .node end')"
  local timeout; timeout="$(echo "$1" | jq -r 'if type=="object" then (.timeout // 45) else 45 end')"
  local mgr; mgr="$(any_manager)"
  wait_for "$node marked reachable" "$timeout" \
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
    "instances_everywhere | jq -e --arg v '$svc' --arg n '$not_on' \
      '[.[] | select(.id | contains(\$v))] | any(.state == \"running\" and .node != \$n)' >/dev/null 2>&1"
  log "$svc rescheduled away from $not_on"
}

step_assert_no_duplicate() {
  local svc mgr
  svc="$(echo "$1" | jq -r '.service')"
  mgr="$(any_manager)"
  local count
  count="$(_running_count "$svc")"
  [[ "$count" -le 1 ]] || die "$svc has $count running copies, expected at most 1"
  log "$svc has no duplicate after rejoin ($count running)"
}

# assert_fenced: ["n3", "n4"] or { nodes: [...], timeout: 60 }
#
# The default timeout is generous on purpose. A node gives up its containers
# fence_timeout after losing quorum, the scheduler notices on its next sweep
# (up to 10s later), and stopping a container waits out its grace period
# (10s). Thirty seconds is inside that budget and fails for the wrong reason.
step_assert_fenced() {
  local nodes timeout
  if [[ "$(echo "$1" | jq -r 'type')" == "array" ]]; then
    nodes="$(echo "$1" | jq -r '.[]')"
    timeout=60
  else
    nodes="$(echo "$1" | jq -r '.nodes[]')"
    timeout="$(echo "$1" | jq -r '.timeout // 60')"
  fi

  local node
  for node in $nodes; do
    wait_for "$node fenced (no running containers)" "$timeout" \
      "instances_everywhere | jq -e --arg n '$node' \
        '[.[] | select(.node == \$n and .state == \"running\")] | length == 0' >/dev/null 2>&1"
  done
  log "fencing confirmed for: $(echo "$nodes" | tr '\n' ' ')"
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
  id="$(instances_everywhere | jq -r --arg p "$prefix" \
    '[.[] | select(.id|startswith($p))][0].id // empty')"
  [[ -n "$id" ]] || die "no instance matching $prefix"
  api_call POST "$mgr" "/api/instances/$id/move" "{\"node\":\"$to\"}" >/dev/null
  wait_for "instance $id running on $to" "$timeout" \
    "api_get '$to' /api/instances/$id 2>/dev/null | jq -e --arg n '$to' \
      '.node == \$n and .state == \"running\"' >/dev/null 2>&1"
  # the migration state machine (docs/drives.md) only flips the assignment
  # after a hash-verified sync, so a running instance on the target is
  # already good evidence the volume arrived intact; this additionally
  # confirms the drive api agrees the target now holds the volume.
  # `replicas` is an array of {node, generation, in_sync}, not a map, so this
  # matches on .node rather than asking `has` for a key it cannot look up.
  wait_for "volume tracked on $to" 20 \
    "api_get '$mgr' /api/volumes 2>/dev/null | jq -e --arg n '$to' \
      'any(.[]?; .primary == \$n or any(.replicas[]?; .node == \$n))' >/dev/null 2>&1"
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

# Counting has to select twice: `select(.id | contains($v) and .state == ...)`
# pipes the object away and then asks a string for its .state, which is a jq
# error, not a false. Under set -e that took the whole run down.
_running_count() {
  instances_everywhere | jq --arg v "$1" \
    '[.[] | select(.id | contains($v)) | select(.state == "running")] | length'
}

step_assert_no_new_starts() {
  local svc timeout mgr before after
  svc="$(echo "$1" | jq -r '.service')"
  timeout="$(echo "$1" | jq -r '.timeout // 10')"
  mgr="$(any_manager)"
  before="$(_running_count "$svc")"
  sleep "$timeout"
  after="$(_running_count "$svc")"
  [[ "$before" == "$after" ]] || die "$svc instance count changed during fencing ($before -> $after)"
  log "no new $svc instances started while fenced"
}

# ---- publishing (internal dns, published ports, reverse proxy) ------------

# Every node runs the proxy and every node listens on every published port, so
# these assertions try each running node in turn rather than looking up where
# the container landed. A route that only answers on the node hosting the
# container is a bug this is meant to catch.
_running_nodes() {
  local n
  for n in $(jq -r '.nodes[].name' "$RUN_DIR/topology.json"); do
    vm_running "$n" && echo "$n"
  done
}

# assert_http_route: { host: app.cl.test, path: /, contains: "..." , timeout: 60 }
step_assert_http_route() {
  local host path contains timeout
  host="$(echo "$1" | jq -r '.host')"
  path="$(echo "$1" | jq -r '.path // "/"')"
  contains="$(echo "$1" | jq -r '.contains // empty')"
  timeout="$(echo "$1" | jq -r '.timeout // 60')"

  wait_for "$host$path answered by the proxy" "$timeout" \
    "_http_route_ok '$host' '$path' '$contains'"
  log "$host$path is served through the reverse proxy"
}

_http_route_ok() {
  local host="$1" path="$2" contains="$3" node body
  for node in $(_running_nodes); do
    body="$(curl -s -m 5 -H "Host: $host" "http://$(host_addr "$node" http_port)$path" 2>/dev/null)" || continue
    if [[ -z "$contains" || "$body" == *"$contains"* ]]; then
      return 0
    fi
  done
  return 1
}

# assert_published_port: { contains: "...", path: /, timeout: 60 }
#
# The published port is forwarded straight through, so anything that speaks
# to it works; http is used because the harness already has curl and the test
# service already answers it.
step_assert_published_port() {
  local contains path timeout
  contains="$(echo "$1" | jq -r '.contains // empty')"
  path="$(echo "$1" | jq -r '.path // "/"')"
  timeout="$(echo "$1" | jq -r '.timeout // 60')"

  wait_for "the published port answers on every node" "$timeout" \
    "_published_port_ok '$path' '$contains'"
  log "the published port is forwarded to the container"
}

_published_port_ok() {
  local path="$1" contains="$2" node body
  for node in $(_running_nodes); do
    body="$(curl -s -m 5 "http://$(host_addr "$node" expose_port)$path" 2>/dev/null)" || continue
    if [[ -z "$contains" || "$body" == *"$contains"* ]]; then
      return 0
    fi
  done
  return 1
}

# assert_healthy: { stack: publish, service: probe, timeout: 90 }
#
# Used to check things only a container can see. The container's healthcheck
# is the assertion; this waits for docker to report it passing.
step_assert_healthy() {
  local stack svc timeout
  stack="$(echo "$1" | jq -r '.stack')"
  svc="$(echo "$1" | jq -r '.service')"
  timeout="$(echo "$1" | jq -r '.timeout // 90')"
  wait_for "$stack.$svc healthy" "$timeout" \
    "instances_everywhere | jq -e --arg s '$stack' --arg v '$svc' \
      '[.[] | select(.id | startswith(\$s + \".\" + \$v))] | any(.health == \"healthy\")' >/dev/null 2>&1"
  log "$stack.$svc reports healthy"
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
