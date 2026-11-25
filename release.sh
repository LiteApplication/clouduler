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

# Crates ALWAYS bumped, even when unchanged: the binary's version is the
# reported release version (see crates/cl-config/src/version.rs).
ALWAYS_BUMPED_CRATES=(clouduler)

if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
    echo "error: version must look like x.y.z (e.g. 0.2.0)" >&2
    exit 1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO_ROOT"

BRANCH="$(git rev-parse --abbrev-ref HEAD)"
if [ "$BRANCH" != "master" ]; then
    if [ "$DRY_RUN" = 1 ]; then
        echo "warning: not on master (on '$BRANCH'); a real release would refuse to run" >&2
    else
        echo "error: releases must be cut from master (on '$BRANCH')" >&2
        exit 1
    fi
fi

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
# True when every added/removed line of crates/$1/Cargo.toml is a version line
# (a workspace-inheritance switch or a plain version bump), so the file is noise.
version_only_cargo() {
    local other
    other="$(git diff -U0 "$LAST_TAG" HEAD -- "crates/$1/Cargo.toml" | grep -E '^[+-]' | grep -vE '^(\+\+\+|---)' | grep -vE '^[+-]version(\.workspace)? *= .' || true)"
    [ -z "$other" ]
}
# A crate changed when any file under crates/$1/ changed, except a version-only Cargo.toml.
crate_changed() {
    local c="$1" files
    files="$(grep "^crates/$c/" <<< "$CHANGED_FILES" || true)"
    if grep -qx "crates/$c/Cargo.toml" <<< "$files" && version_only_cargo "$c"; then
        files="$(grep -vx "crates/$c/Cargo.toml" <<< "$files" || true)"
    fi
    [ -n "$files" ]
}

UI_CHANGED=0
if grep -q '^ui/' <<<"$CHANGED_FILES"; then UI_CHANGED=1; fi

BUMP=()
KEEP=()
for dir in crates/*/; do
    c="$(basename "$dir")"
    if in_array "$c" "${ALWAYS_BUMPED_CRATES[@]}" \
        || crate_changed "$c" \
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
echo "  ui: ${UI_OLD} -> ${VERSION}"
echo "Kept:"
for c in "${KEEP[@]}"; do echo "  $c: $(crate_version "$c")"; done

if [ "$DRY_RUN" = 1 ]; then
    echo "Dry run: nothing modified."
    exit 0
fi

read -r -p "Release v${VERSION} and push to origin? [y/N] " CONFIRM
case "$CONFIRM" in
    y|Y|yes|YES) ;;
    *) echo "Aborted." >&2; exit 1 ;;
esac

# The api schema (api-shapes fixture and generated ts bindings) may only change
# between minor or major releases. Regenerate both, and in a patch release abort
# if they differ from what is committed.
LAST_VER="${LAST_TAG#v}"
IFS=. read -r LAST_MAJOR LAST_MINOR _ <<< "${LAST_VER%%-*}"
IFS=. read -r NEW_MAJOR NEW_MINOR _ <<< "${VERSION%%-*}"
PATCH_RELEASE=0
if [ "$LAST_MAJOR" = "$NEW_MAJOR" ] && [ "$LAST_MINOR" = "$NEW_MINOR" ]; then PATCH_RELEASE=1; fi
SCHEMA_FILES=(ui/src/api/__fixtures__/api-shapes.json ui/src/api/generated)

echo "Regenerating api-shapes fixture..."
UPDATE_API_FIXTURES=1 cargo test -p cl-api api_shapes
# The regenerating run returns before comparing, so run it again without the
# flag: the written fixture must pass before it is committed, tagged and pushed.
echo "Checking the api-shapes fixture..."
cargo test -p cl-api api_shapes

echo "Regenerating typescript bindings..."
scripts/gen-ts-bindings.sh

if [ "$PATCH_RELEASE" = 1 ] && [ -n "$(git status --porcelain -- "${SCHEMA_FILES[@]}")" ]; then
    echo "error: the api schema changed, which is only allowed in a minor or major release (v${VERSION} is a patch release over $LAST_TAG):" >&2
    git status --short -- "${SCHEMA_FILES[@]}" >&2
    echo "Restoring the schema files; pick a new minor/major version or revert the schema change." >&2
    git checkout -- "${SCHEMA_FILES[@]}"
    exit 1
fi

echo "Bumping version to ${VERSION}..."

sed -i -E "/^\[workspace.package\]/,/^\[/ s/^version = \".*\"/version = \"${VERSION}\"/" Cargo.toml
for c in "${BUMP[@]}"; do
    sed -i -E "/^\[package\]/,/^\[/ s/^version = \".*\"/version = \"${VERSION}\"/" "crates/$c/Cargo.toml"
done
# ui/package.json always follows the release version.
sed -i -E "0,/\"version\": \"[^\"]*\"/s//\"version\": \"${VERSION}\"/" ui/package.json

if [ -f Cargo.lock ]; then
    cargo update --workspace --offline >/dev/null 2>&1 || cargo update --workspace
fi

if command -v npm >/dev/null 2>&1 && [ -f ui/package-lock.json ]; then
    (cd ui && npm install --package-lock-only --silent)
fi

git add Cargo.toml crates/*/Cargo.toml ui/package.json Cargo.lock ui/package-lock.json ui/src/api/__fixtures__/api-shapes.json ui/src/api/generated

git commit -m "release: v${VERSION}"
git tag -a "v${VERSION}" -m "v${VERSION}"

git push origin HEAD
git push origin "v${VERSION}"

echo "Released v${VERSION}"
