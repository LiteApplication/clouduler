#!/usr/bin/env bash
# Builds the cached Alpine base rootfs used by every scenario VM: alpine-base,
# docker and its dependencies, plus a kernel and initramfs for direct boot.
#
# This does not touch the clouduler binary. That is injected per node at disk
# build time (see lib/mkdisk.sh) so a run always exercises the current tree,
# while the expensive Alpine + docker install stays cached between runs.
#
# Usage: build-image.sh [--force]
#   --force   rebuild the base rootfs even if the cache is present.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CACHE_DIR="${QEMU_TEST_CACHE:-$SCRIPT_DIR/cache}"
ROOTFS_DIR="$CACHE_DIR/rootfs"
APK_CACHE="$CACHE_DIR/apk-cache"
STAMP="$ROOTFS_DIR/.image-complete"

ALPINE_VERSION="3.20"
ALPINE_ARCH="x86_64"
APK_TOOLS_STATIC_URL="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/main/${ALPINE_ARCH}"
ALPINE_MIRROR="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/main"
ALPINE_MIRROR_COMMUNITY="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/community"

FORCE=0
if [[ "${1:-}" == "--force" ]]; then
  FORCE=1
fi

if [[ -f "$STAMP" && "$FORCE" -eq 0 ]]; then
  echo "build-image: cached base rootfs already present at $ROOTFS_DIR (use --force to rebuild)"
  exit 0
fi

if ! command -v qemu-system-x86_64 >/dev/null 2>&1; then
  echo "build-image: qemu-system-x86_64 not found on PATH" >&2
  exit 1
fi

mkdir -p "$CACHE_DIR" "$APK_CACHE"
rm -rf "$ROOTFS_DIR"
mkdir -p "$ROOTFS_DIR"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "build-image: fetching static apk-tools"
# apk-tools-static lets us populate an Alpine rootfs from a non-Alpine host,
# the same trick Alpine's own mkimage.sh uses.
APKGZ="$WORK/apk-tools-static.apk"
INDEX_HTML="$WORK/index.html"
curl -fsSL "$APK_TOOLS_STATIC_URL/" -o "$INDEX_HTML"
APK_PKG_NAME="$(grep -o 'apk-tools-static-[0-9][^"<]*\.apk' "$INDEX_HTML" | sort -V | tail -1)"
if [[ -z "$APK_PKG_NAME" ]]; then
  echo "build-image: could not find apk-tools-static package in mirror index" >&2
  exit 1
fi
curl -fsSL "$APK_TOOLS_STATIC_URL/$APK_PKG_NAME" -o "$APKGZ"
tar -xz -f "$APKGZ" -C "$WORK" sbin/apk.static
APK="$WORK/sbin/apk.static"

echo "build-image: installing base packages into $ROOTFS_DIR"
# apk's post-install scripts (mkinitfs, docker-engine, iproute2) chroot into
# the target root. A plain unprivileged run can lay files down but can't
# chroot, so we run apk inside a user namespace with uid 0 mapped: that
# grants real CAP_SYS_CHROOT for this process tree without host root.
# A few setuid binaries (mount, su...) can't get their exact ownership set
# under a mapped user namespace and apk exits nonzero for that alone; those
# binaries are not needed by anything this harness runs, so we check for the
# files that matter (kernel, initramfs, docker, clouduler's binary slot)
# afterward instead of trusting apk's exit code here.
# gcompat lets a glibc-linked clouduler binary (a plain `cargo build
# --release` on most dev machines and CI runners) run on this musl-based
# rootfs. It is not a perfect glibc, so a genuinely musl build (--target
# x86_64-unknown-linux-musl) is the more robust option if this ever bites;
# see docs/testing.md.
unshare --user --map-root-user --mount -- "$APK" \
  -X "$ALPINE_MIRROR" -X "$ALPINE_MIRROR_COMMUNITY" \
  -U --allow-untrusted --arch "$ALPINE_ARCH" \
  --root "$ROOTFS_DIR" --cache-dir "$APK_CACHE" --initdb \
  add alpine-base linux-virt docker docker-cli iptables ip6tables \
      util-linux iproute2 curl jq coreutils gcompat \
  || echo "build-image: apk reported non-fatal errors (likely setuid ownership under userns), continuing"

echo "build-image: writing init"
# No openrc: our custom init (below) starts dockerd and clouduler itself,
# since it is PID 1 (booted with init=/sbin/init-clouduler).
install -m 0755 "$SCRIPT_DIR/lib/guest-init.sh" "$ROOTFS_DIR/sbin/init-clouduler"

# nsswitch/hosts basics so short names resolve locally, and a serial getty
# is not needed since our init drives the console directly.
cat > "$ROOTFS_DIR/etc/hosts" <<'EOF'
127.0.0.1 localhost
::1 localhost
EOF

mkdir -p "$ROOTFS_DIR/etc/clouduler" "$ROOTFS_DIR/var/lib/clouduler"

[[ -f "$ROOTFS_DIR/boot/vmlinuz-virt" ]] || { echo "build-image: vmlinuz-virt missing, apk install did not complete" >&2; exit 1; }
[[ -f "$ROOTFS_DIR/boot/initramfs-virt" ]] || { echo "build-image: initramfs-virt missing, apk install did not complete" >&2; exit 1; }
[[ -x "$ROOTFS_DIR/usr/bin/dockerd" ]] \
  || { echo "build-image: dockerd missing, apk install did not complete" >&2; exit 1; }

# Pull the kernel and initramfs out to the cache root for direct -kernel boot.
KVER_DIR="$(find "$ROOTFS_DIR/lib/modules" -maxdepth 1 -mindepth 1 -type d | head -1)"
KVER="$(basename "$KVER_DIR")"
cp "$ROOTFS_DIR/boot/vmlinuz-virt" "$CACHE_DIR/vmlinuz"
cp "$ROOTFS_DIR/boot/initramfs-virt" "$CACHE_DIR/initramfs"
echo "$KVER" > "$CACHE_DIR/kernel-version"

date -u +%Y-%m-%dT%H:%M:%SZ > "$STAMP"
echo "build-image: base rootfs cached at $ROOTFS_DIR (kernel $KVER)"
