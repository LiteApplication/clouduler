#!/usr/bin/env bash
# Release helper: source hashes, the reuse plan and manifest.json.
#
#   release-manifest.sh source-hash backend|ui [REV]
#       Hash of a component's inputs (the working tree's tracked files, or the
#       tree of git REV). Version lines are ignored, so a version bump alone
#       never changes it.
#   release-manifest.sh plan [PREV_MANIFEST [PREV_ASSETS]]
#       Prints key=value lines (for $GITHUB_OUTPUT): build_backend, build_ui,
#       backend_hash, ui_hash, backend_version, ui_version. A component is
#       reused when PREV_MANIFEST has the same source_hash and every asset it
#       needs is listed in PREV_ASSETS (one name per line; omit to skip that
#       check). No PREV_MANIFEST means build everything.
#   release-manifest.sh zip-ui DIST_DIR OUT.zip
#       Deterministic zip of the built ui (sorted entries, fixed mtimes).
#   release-manifest.sh verify-reused PREV_MANIFEST DIR backend|ui
#       Checks that the previous release's assets of that component, already
#       downloaded into DIR, match the blake3 hashes in PREV_MANIFEST.
#   release-manifest.sh assemble TAG DIST_DIR PLAN_FILE
#       Writes DIST_DIR/manifest.json from the artifacts in DIST_DIR and the
#       key=value lines of PLAN_FILE (the output of plan).
#
# blake3 comes from b3sum, or from python3 with the blake3 module
# (B3_PYTHON overrides the interpreter): pip install blake3.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

ARCHES=(x86_64 aarch64)
UI_ASSET="clouduler-ui.zip"

die() { echo "release-manifest: $*" >&2; exit 1; }

blake3_file() {
    if command -v b3sum >/dev/null 2>&1; then
        b3sum --no-names "$1"
    else
        "${B3_PYTHON:-python3}" -I -c '
import sys
from blake3 import blake3
h = blake3()
with open(sys.argv[1], "rb") as f:
    for chunk in iter(lambda: f.read(1 << 20), b""):
        h.update(chunk)
print(h.hexdigest())' "$1" || die "need b3sum or python3 with the blake3 module (pip install blake3)"
    fi
}

# Drops the version line(s) that change in a release, so the hash only sees
# real changes. $1 is the path, the content is on stdin.
normalize() {
    case "$1" in
        Cargo.toml | crates/*/Cargo.toml)
            awk '/^\[/{p=($0=="[package]" || $0=="[workspace.package]")} !(p && /^version(\.workspace)? *=/)' ;;
        Cargo.lock)
            # a workspace member has no "source" line; its version is the release version
            awk '
                function flush(   i) { for (i = 1; i <= n; i++) if (!(!src && blk[i] ~ /^version = /)) print blk[i]; n = 0; src = 0 }
                /^\[\[package\]\]/ { flush() }
                { blk[++n] = $0; if ($0 ~ /^source = /) src = 1 }
                END { flush() }' ;;
        ui/package.json)
            awk '!d && /^  "version":/ { d = 1; next } { print }' ;;
        ui/package-lock.json)
            awk 'NR <= 12 && /"version":/ { next } { print }' ;;
        ui/src/api/__fixtures__/api-shapes.json)
            grep -vE '"version": "[0-9]+\.[0-9]+\.[0-9]+[^"]*"' || true ;;
        *) cat ;;
    esac
}

source_hash() {
    local comp="$1" rev="${2:-}" paths list f
    case "$comp" in
        backend) paths=(crates Cargo.lock Cargo.toml) ;;
        ui) paths=(ui) ;;
        *) die "component must be backend or ui" ;;
    esac
    if [ -n "$rev" ]; then
        list="$(git ls-tree -r --name-only "$rev" -- "${paths[@]}")"
    else
        list="$(git ls-files -- "${paths[@]}")"
    fi
    list="$(LC_ALL=C sort <<< "$list")"
    while IFS= read -r f; do
        [ -n "$f" ] || continue
        if [ -z "$rev" ] && [ ! -e "$f" ]; then continue; fi
        {
            if [ -n "$rev" ]; then git show "$rev:$f"; else cat "$f"; fi
        } | normalize "$f" | sha256sum | cut -d' ' -f1 | sed "s|\$|  $f|"
    done <<< "$list" | sha256sum | cut -d' ' -f1
}

toml_version() { # the [package] / [workspace.package] version of a Cargo.toml
    awk '/^\[/{p=($0=="[package]" || $0=="[workspace.package]")} p && /^version *= *"/{gsub(/^version *= *"|".*$/,""); print; exit}' "$1"
}

backend_version() {
    local v
    v="$(toml_version crates/clouduler/Cargo.toml)"
    [ -n "$v" ] || v="$(toml_version Cargo.toml)" # version.workspace = true
    echo "$v"
}

ui_version() { sed -n -E 's/^[[:space:]]*"version": "([^"]*)".*/\1/p' ui/package.json | head -n1; }

has_asset() { # $1 = assets file or empty (no check), $2 = name
    [ -z "$1" ] || grep -qxF "$2" "$1"
}

cmd_plan() {
    local prev="${1:-}" assets="${2:-}"
    local bh uh bv uv
    bh="$(source_hash backend)"
    uh="$(source_hash ui)"
    bv="$(backend_version)"
    uv="$(ui_version)"
    local build_backend=true build_ui=true
    if [ -n "$prev" ] && [ -s "$prev" ]; then
        if [ "$(jq -r '.backend.source_hash // ""' "$prev")" = "$bh" ]; then
            local ok=1 a asset
            for a in "${ARCHES[@]}"; do
                asset="$(jq -r --arg a "$a" '.backend.arch[$a].asset // ""' "$prev")"
                if [ -z "$asset" ] || ! has_asset "$assets" "$asset"; then ok=0; fi
            done
            if [ "$ok" = 1 ]; then build_backend=false; bv="$(jq -r '.backend.version' "$prev")"; fi
        fi
        if [ "$(jq -r '.ui.source_hash // ""' "$prev")" = "$uh" ]; then
            local uasset
            uasset="$(jq -r '.ui.asset // ""' "$prev")"
            if [ -n "$uasset" ] && has_asset "$assets" "$uasset"; then build_ui=false; uv="$(jq -r '.ui.version' "$prev")"; fi
        fi
    fi
    cat <<EOF
build_backend=$build_backend
build_ui=$build_ui
backend_hash=$bh
ui_hash=$uh
backend_version=$bv
ui_version=$uv
EOF
}

cmd_zip_ui() {
    local dist="$1" out="$2"
    [ -f "$dist/index.html" ] || die "$dist has no index.html"
    rm -f "$out"
    python3 -I - "$dist" "$out" <<'PY'
import os, sys, zipfile
dist, out = sys.argv[1], sys.argv[2]
names = []
for root, dirs, files in os.walk(dist):
    dirs.sort()
    for f in files:
        p = os.path.join(root, f)
        names.append(os.path.relpath(p, dist).replace(os.sep, "/"))
with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as z:
    for n in sorted(names):
        info = zipfile.ZipInfo(n, date_time=(1980, 1, 1, 0, 0, 0))
        info.compress_type = zipfile.ZIP_DEFLATED
        info.create_system = 3
        info.external_attr = 0o100644 << 16
        with open(os.path.join(dist, n), "rb") as fh:
            z.writestr(info, fh.read(), compresslevel=9)
PY
}

cmd_verify_reused() {
    local prev="$1" dir="$2" comp="$3" rows row asset want got bad=0
    case "$comp" in
        backend) rows="$(jq -r '.backend.arch[] | [.asset, .blake3] | @tsv' "$prev")" ;;
        ui) rows="$(jq -r '.ui | [.asset, .blake3] | @tsv' "$prev")" ;;
        *) die "component must be backend or ui" ;;
    esac
    [ -n "$rows" ] || die "$prev lists no $comp assets"
    while IFS=$'\t' read -r asset want; do
        [ -f "$dir/$asset" ] || die "missing $dir/$asset"
        got="$(blake3_file "$dir/$asset")"
        if [ "$got" != "$want" ]; then
            echo "release-manifest: $asset hash $got differs from the manifest's $want" >&2
            bad=1
        fi
    done <<< "$rows"
    [ "$bad" = 0 ] || exit 1
}

cmd_assemble() {
    local tag="$1" dist="$2" planf="$3"
    local k v bv="" uv="" bh="" uh=""
    while IFS='=' read -r k v; do
        case "$k" in
            backend_version) bv="$v" ;;
            ui_version) uv="$v" ;;
            backend_hash) bh="$v" ;;
            ui_hash) uh="$v" ;;
        esac
    done < "$planf"
    if [ -z "$bv$uv" ] || [ -z "$bh" ] || [ -z "$uh" ] || [ -z "$bv" ] || [ -z "$uv" ]; then
        die "plan file $planf lacks versions or hashes"
    fi
    local arch_json='{}' a asset
    for a in "${ARCHES[@]}"; do
        asset="clouduler-${a}-unknown-linux-gnu"
        [ -f "$dist/$asset" ] || die "missing $dist/$asset"
        arch_json="$(jq -c --arg a "$a" --arg n "$asset" --arg h "$(blake3_file "$dist/$asset")" \
            '. + {($a): {asset: $n, blake3: $h}}' <<< "$arch_json")"
    done
    [ -f "$dist/$UI_ASSET" ] || die "missing $dist/$UI_ASSET"
    jq -n \
        --arg tag "$tag" --arg bv "$bv" --arg bh "$bh" --argjson arch "$arch_json" \
        --arg uv "$uv" --arg uh "$uh" --arg ua "$UI_ASSET" --arg ub "$(blake3_file "$dist/$UI_ASSET")" \
        '{schema: 1, tag: $tag,
          backend: {version: $bv, source_hash: $bh, arch: $arch},
          ui: {version: $uv, source_hash: $uh, asset: $ua, blake3: $ub}}' > "$dist/manifest.json"
}

case "${1:-}" in
    source-hash) shift; [ $# -ge 1 ] || die "usage: source-hash backend|ui [REV]"; source_hash "$@" ;;
    plan) shift; cmd_plan "$@" ;;
    zip-ui) shift; [ $# -eq 2 ] || die "usage: zip-ui DIST_DIR OUT.zip"; cmd_zip_ui "$@" ;;
    verify-reused) shift; [ $# -eq 3 ] || die "usage: verify-reused PREV_MANIFEST DIR backend|ui"; cmd_verify_reused "$@" ;;
    assemble) shift; [ $# -eq 3 ] || die "usage: assemble TAG DIST_DIR PLAN_FILE"; cmd_assemble "$@" ;;
    *) die "usage: $0 source-hash|plan|zip-ui|verify-reused|assemble ..." ;;
esac
