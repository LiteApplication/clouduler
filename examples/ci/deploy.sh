#!/usr/bin/env bash
# Deploy a stack to a clouduler cluster and wait for it to actually be running.
#
# Usage:
#   deploy.sh <stack-name> <stack-file.yaml>
#   deploy.sh --update <stack-name> <service>     re-resolve the image and restart
#
# Environment:
#   CLOUDULER_URL       https://cloud.int.example.com   (required)
#   CLOUDULER_TOKEN     an admin api key                 (required)
#   CLOUDULER_INSECURE  1 to skip certificate checking. Defaults to 1, because a
#                       cluster that has not been issued an ACME certificate yet
#                       serves a self signed one. Set 0 once it has a real one.
#   DEPLOY_TIMEOUT      seconds to wait for instances to run. Default 120.
#
# Exits nonzero if the deploy is rejected or the instances never come up, so a
# pipeline fails on a broken rollout rather than reporting success.
set -euo pipefail

: "${CLOUDULER_URL:?set CLOUDULER_URL to the cluster web address}"
: "${CLOUDULER_TOKEN:?set CLOUDULER_TOKEN to an admin api key}"
INSECURE="${CLOUDULER_INSECURE:-1}"
TIMEOUT="${DEPLOY_TIMEOUT:-120}"

curl_opts=(--silent --show-error --fail-with-body --max-time 30
           -H "Authorization: Bearer $CLOUDULER_TOKEN")
if [ "$INSECURE" = "1" ]; then
  curl_opts+=(--insecure)
fi

api() {
  local method="$1" path="$2"
  shift 2
  curl "${curl_opts[@]}" -X "$method" "$CLOUDULER_URL$path" "$@"
}

# "<id> <state>" for every instance whose id starts with the given prefix.
# Instance ids are "<stack>.<service>.<index>", so one prefix covers both a
# whole stack and a single service.
instances_under() {
  api GET /api/instances \
    | jq -r --arg p "$1" '.[] | select(.id | startswith($p)) | "\(.id) \(.state)"'
}

wait_running() {
  local prefix="$1" what="$2"
  local deadline=$(( SECONDS + TIMEOUT ))
  echo "waiting for $what (up to ${TIMEOUT}s)"

  while [ "$SECONDS" -lt "$deadline" ]; do
    local lines total running
    lines="$(instances_under "$prefix" || true)"
    total="$(printf '%s' "$lines" | grep -c . || true)"
    running="$(printf '%s' "$lines" | grep -c ' running$' || true)"

    if [ "$total" -gt 0 ] && [ "$running" = "$total" ]; then
      echo "$what: $running/$total running"
      return 0
    fi
    echo "  $running/$total running"
    sleep 3
  done

  echo "timed out waiting for $what. Current state:" >&2
  instances_under "$prefix" | sed 's/^/  /' >&2 || true
  echo "for detail: $CLOUDULER_URL/api/instances/<id>/logs" >&2
  return 1
}

# --update: the stack file has not changed, only the image behind a moving tag
# like :latest. The cluster re-resolves the digest and restarts the container.
if [ "${1:-}" = "--update" ]; then
  stack="${2:?usage: deploy.sh --update <stack> <service>}"
  service="${3:?usage: deploy.sh --update <stack> <service>}"
  ids="$(instances_under "$stack.$service." | cut -d' ' -f1)"
  if [ -z "$ids" ]; then
    echo "no instances of $stack.$service" >&2
    exit 1
  fi
  for id in $ids; do
    echo "updating $id"
    api POST "/api/instances/$id/update" >/dev/null
  done
  wait_running "$stack.$service." "$stack.$service"
  exit 0
fi

stack="${1:?usage: deploy.sh <stack-name> <stack-file.yaml>}"
file="${2:?usage: deploy.sh <stack-name> <stack-file.yaml>}"
if [ ! -f "$file" ]; then
  echo "no such file: $file" >&2
  exit 1
fi

# Validated first, so a typo is rejected before anything is written and the
# stack that is already running stays up.
echo "validating $file"
jq -n --rawfile spec "$file" '{spec: $spec}' \
  | api POST "/api/stacks/$stack/validate" -H 'content-type: application/json' --data @- >/dev/null

echo "deploying $stack from $file"
jq -n --rawfile spec "$file" '{spec: $spec}' \
  | api PUT "/api/stacks/$stack" -H 'content-type: application/json' --data @- >/dev/null

# Waiting on the stack prefix rather than picking service names out of the yaml:
# the cluster already knows what it created.
wait_running "$stack." "$stack"

echo "$stack deployed"
