#!/usr/bin/env bash
# Builds one node's boot disk: a copy of the cached base rootfs with the
# freshly built clouduler binary, the run's cluster.yaml, the guest init and
# the node's hostname dropped in, packed into an ext4 image with mke2fs -d (no
# loop mount, no root needed).
#
# The init script is installed here rather than left to the cached rootfs for
# the same reason as the binary: an edit to it should take effect on the next
# run, not on the next time someone remembers to rebuild the image.
#
# Usage: mkdisk.sh <rootfs-cache-dir> <node-name> <cluster-yaml> <clouduler-bin> <out-image> <cluster-key>
set -euo pipefail

if [[ $# -ne 6 ]]; then
  echo "usage: mkdisk.sh <rootfs-cache-dir> <node-name> <cluster-yaml> <clouduler-bin> <out-image> <cluster-key>" >&2
  exit 1
fi

GUEST_INIT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/guest-init.sh"

ROOTFS_CACHE="$1"
NODE="$2"
CLUSTER_YAML="$3"
CLOUDULER_BIN="$4"
OUT_IMAGE="$5"
CLUSTER_KEY="$6"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

STAGE="$WORK/rootfs"

# A few files in the cached rootfs (setuid helpers like bbsuid) are
# read-restricted even to their own owner, which is normal for them but
# means a plain `cp` run outside a namespace can't read them back out. The
# same user-namespace trick used to build the rootfs gives us that access
# again here.
unshare --user --map-root-user --mount -- bash -c '
  set -euo pipefail
  cp -a "$1" "$2"
  install -D -m 0755 "$3" "$2/usr/local/bin/clouduler"
  install -D -m 0644 "$4" "$2/etc/clouduler/cluster.yaml"
  # Every node gets the same key, generated once per run. The daemon refuses
  # to read a key file anyone else could read, so the mode matters.
  install -D -m 0600 "$7" "$2/etc/clouduler/secrets.key"
  install -D -m 0755 "$8" "$2/sbin/init-clouduler"
  # Optional per-scenario script the guest runs in the background (see
  # guest-init.sh); empty when the scenario has none.
  if [ -n "$9" ]; then install -D -m 0755 "$9" "$2/etc/clouduler/probe.sh"; fi
  echo "$5" > "$2/etc/hostname"
  # rootfs is ~370M, plus the clouduler binary (a debug build can be over
  # 100M) and headroom for docker image layers pulled during a scenario.
  mke2fs -q -t ext4 -d "$2" -F "$6" 1536M
' _ "$ROOTFS_CACHE" "$STAGE" "$CLOUDULER_BIN" "$CLUSTER_YAML" "$NODE" "$OUT_IMAGE" "$CLUSTER_KEY" \
    "$GUEST_INIT" "${GUEST_PROBE:-}"
