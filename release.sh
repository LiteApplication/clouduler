#!/usr/bin/env bash
set -euo pipefail

DRY_RUN=0
ARGS=()
for a in "$@"; do
    case "$a" in
        --dry-run) DRY_RUN=1 ;;
        *) ARGS+=("$a") ;;
    esac
done

if [ ${#ARGS[@]} -ne 1 ]; then
    echo "Usage: $0 [--dry-run] <version>  (e.g. $0 0.2.0)" >&2
    exit 1
fi

VERSION="${ARGS[0]}"

# Crates that identify the release and are ALWAYS bumped, even when unchanged:
#  - clouduler: the binary users install
#  - cl-api: reports env!("CARGO_PKG_VERSION") in cluster/node info and the
#    self-update running_version comparison, and the alert User-Agent
#  - cl-daemon: reports its CARGO_PKG_VERSION to the cluster (node version checks)
#  - cl-alert: sends "clouduler/<CARGO_PKG_VERSION>" as User-Agent
ALWAYS_BUMPED_CRATES=(clouduler cl-api cl-daemon cl-alert)

if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
    echo "error: version must look like x.y.z (e.g. 0.2.0)" >&2
    exit 1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO_ROOT"

if [ -n "$(git status --porcelain)" ]; then
    if [ "$DRY_RUN" = 1 ]; then
        echo "warning: working tree is not clean (a real release would refuse to run)" >&2
    else
        echo "error: working tree is not clean, commit or stash your changes first" >&2
        exit 1
    fi
fi

if git rev-parse "v${VERSION}" >/dev/null 2>&1; then
    echo "error: tag v${VERSION} already exists" >&2
    exit 1
fi

LAST_TAG="$(git describe --tags --abbrev=0 --match 'v[0-9]*' 2>/dev/null || true)"
if [ -z "$LAST_TAG" ]; then
    echo "error: no previous v[0-9]* tag found" >&2
    exit 1
fi
CHANGED_FILES="$(git diff --name-only "$LAST_TAG" HEAD)"

in_array() { local x="$1"; shift; for y in "$@"; do [ "$x" = "$y" ] && return 0; done; return 1; }
crate_version() { awk '/^\[/{p=($0=="[package]")} p && /^version *= *"/{gsub(/^version *= *"|".*$/,""); print; exit}' "crates/$1/Cargo.toml"; }

UI_CHANGED=0
if grep -q '^ui/' <<<"$CHANGED_FILES"; then UI_CHANGED=1; fi

BUMP=()
KEEP=()
for dir in crates/*/; do
    c="$(basename "$dir")"
    if in_array "$c" "${ALWAYS_BUMPED_CRATES[@]}" \
        || grep -q "^crates/$c/" <<<"$CHANGED_FILES" \
        || { [ "$UI_CHANGED" = 1 ] && [ "$c" = cl-api ]; }; then
        BUMP+=("$c")
    else
        KEEP+=("$c")
    fi
done

UI_OLD="$(sed -n -E 's/^[[:space:]]*"version": "([^"]*)".*/\1/p' ui/package.json | head -n1)"

echo "Last release tag: $LAST_TAG"
echo "Release version:  ${VERSION} (workspace)"
echo "Bumped:"
for c in "${BUMP[@]}"; do echo "  $c: $(crate_version "$c") -> ${VERSION}"; done
if [ "$UI_CHANGED" = 1 ]; then echo "  ui: ${UI_OLD} -> ${VERSION}"; fi
echo "Kept:"
for c in "${KEEP[@]}"; do echo "  $c: $(crate_version "$c")"; done
if [ "$UI_CHANGED" = 0 ]; then echo "  ui: ${UI_OLD}"; fi

if [ "$DRY_RUN" = 1 ]; then
    echo "Dry run: nothing modified."
    exit 0
fi

echo "Bumping version to ${VERSION}..."

sed -i -E "/^\[workspace.package\]/,/^\[/ s/^version = \".*\"/version = \"${VERSION}\"/" Cargo.toml
for c in "${BUMP[@]}"; do
    sed -i -E "/^\[package\]/,/^\[/ s/^version = \".*\"/version = \"${VERSION}\"/" "crates/$c/Cargo.toml"
done
if [ "$UI_CHANGED" = 1 ]; then
    sed -i -E "0,/\"version\": \"[^\"]*\"/s//\"version\": \"${VERSION}\"/" ui/package.json
fi

if [ -f Cargo.lock ]; then
    cargo update --workspace --offline >/dev/null 2>&1 || cargo update --workspace
fi

if command -v npm >/dev/null 2>&1 && [ -f ui/package-lock.json ]; then
    (cd ui && npm install --package-lock-only --silent)
fi

echo "Regenerating api-shapes fixture..."
UPDATE_API_FIXTURES=1 cargo test -p cl-api api_shapes
# The regenerating run returns before comparing, so run it again without the
# flag: the written fixture must pass before it is committed, tagged and pushed.
echo "Checking the api-shapes fixture..."
cargo test -p cl-api api_shapes

git add Cargo.toml crates/*/Cargo.toml ui/package.json Cargo.lock ui/package-lock.json ui/src/api/__fixtures__/api-shapes.json

git commit -m "release: v${VERSION}"
git tag -a "v${VERSION}" -m "v${VERSION}"

git push origin HEAD
git push origin "v${VERSION}"

echo "Released v${VERSION}"
