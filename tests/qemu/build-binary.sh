#!/usr/bin/env bash
# Builds the clouduler binary the scenario VMs run, and prints its path.
#
# It has to be linked against musl. The guest rootfs is Alpine, and a normal
# `cargo build` on a typical dev machine or CI runner produces a glibc binary
# that will not start there: gcompat covers a lot of glibc but not
# libgcc_s.so.1, not the _Unwind_* family, and not modern glibc's
# __isoc23_* symbols. The failure looks like a boot loop with "Error
# relocating" lines on the serial console.
#
# Three ways to get one, tried in order:
#   1. $CLOUDULER_BIN, if you already have a musl binary you want used.
#   2. A musl C compiler on PATH (musl-gcc, from musl-tools on Debian/Ubuntu
#      or musl on Arch). ring and aws-lc-rs have C in them, so the rust musl
#      target alone is not enough.
#   3. Docker, building inside rust:alpine. Slower, but the harness already
#      requires docker and this needs nothing installed on the host.
#
# Usage: build-binary.sh   (prints the binary path on stdout, logs to stderr)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TARGET="x86_64-unknown-linux-musl"
BUILDER_IMAGE="clouduler-musl-builder"
OUT="$REPO_ROOT/target/$TARGET/release/clouduler"

log() { printf 'build-binary: %s\n' "$*" >&2; }

if [[ -n "${CLOUDULER_BIN:-}" ]]; then
  [[ -x "$CLOUDULER_BIN" ]] || { log "CLOUDULER_BIN=$CLOUDULER_BIN is not executable"; exit 1; }
  log "using CLOUDULER_BIN=$CLOUDULER_BIN"
  echo "$CLOUDULER_BIN"
  exit 0
fi

musl_cc=""
for candidate in musl-gcc x86_64-linux-musl-gcc; do
  if command -v "$candidate" >/dev/null 2>&1; then
    musl_cc="$candidate"
    break
  fi
done

if [[ -n "$musl_cc" ]] && rustup target list --installed 2>/dev/null | grep -qx "$TARGET"; then
  log "building with $musl_cc"
  CC_x86_64_unknown_linux_musl="$musl_cc" \
  CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="$musl_cc" \
    cargo build --release --target "$TARGET" -p clouduler >&2
elif command -v docker >/dev/null 2>&1; then
  log "no musl compiler on PATH, building in docker"
  # A one-off image rather than `apk add` on every run: the packages have to
  # be installed as root, but the build has to run as the host user or every
  # file it writes into the bind-mounted target dir comes back root owned.
  # Doing the install at image build time separates the two.
  if ! docker image inspect "$BUILDER_IMAGE" >/dev/null 2>&1; then
    log "building the $BUILDER_IMAGE image (once)"
    docker build -t "$BUILDER_IMAGE" -q - >&2 <<'DOCKERFILE'
FROM rust:alpine
RUN apk add --no-cache musl-dev
DOCKERFILE
  fi
  # Cargo's home has to be somewhere this uid can write, and keeping it under
  # target/ means the download cache survives between runs.
  docker run --rm \
    -v "$REPO_ROOT:/src" \
    -w /src \
    -u "$(id -u):$(id -g)" \
    -e CARGO_HOME=/src/target/musl-cargo-home \
    -e CARGO_TARGET_DIR=/src/target \
    "$BUILDER_IMAGE" \
    cargo build --release --target "$TARGET" -p clouduler >&2
else
  log "no way to build a musl binary: install musl-tools (or musl), or docker, or set CLOUDULER_BIN"
  exit 1
fi

[[ -x "$OUT" ]] || { log "build finished but $OUT is missing"; exit 1; }

# A glibc binary here would boot-loop the VMs with relocation errors, so catch
# it now rather than 40 seconds into a scenario.
if file "$OUT" 2>/dev/null | grep -q "dynamically linked, interpreter /lib64"; then
  log "$OUT is dynamically linked against glibc, not musl"
  exit 1
fi

log "built $OUT"
echo "$OUT"
