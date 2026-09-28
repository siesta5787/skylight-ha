#!/bin/sh
# Build and publish a GitHub Release for the in-app updater to find, entirely
# from this machine -- no CI build involved. Deliberately local rather than
# CI-triggered: a QEMU-emulated aarch64 build takes 2-3.5h regardless of
# caching, and running it on every tag push would burn that on every release
# for no benefit over just doing it here, where it's already the normal way
# this binary gets built (see CLAUDE.md's "Building the Pi binary" section).
#
# What this does, in order:
#   1. Reads the release version from the workspace Cargo.toml (single
#      source of truth -- no separate version argument to accidentally
#      mismatch).
#   2. Builds aarch64-unknown-linux-musl via the same QEMU-emulated Alpine
#      Docker recipe documented in CLAUDE.md.
#   3. Runs the freshly-built binary's own `--version` inside a second,
#      minimal container carrying only the three runtime libs the real
#      device provides (libinput/eudev-libs/libxkbcommon) -- the same
#      preflight the on-device installer itself performs before ever
#      swapping a downloaded binary in. Catches a wrong arch, a missing
#      DT_NEEDED, or a forgotten version bump here, not on a wall-mounted Pi
#      with no keyboard.
#   4. Renders manifest.json (version + sha256 + size + requires_reflash +
#      notes) -- the second asset the in-app updater's manifest fetch needs
#      alongside the binary.
#   5. Creates an annotated tag, pushes it, and publishes the GitHub Release
#      with both assets attached.
#
# Usage:
#   scripts/release.sh [-m "release notes"] [--requires-reflash] [--skip-build] [--allow-dirty]
#
#   -m TEXT           Release notes (also becomes the annotated tag's
#                      message). Defaults to "Release vX.Y.Z".
#   --requires-reflash Mark this release as needing more than a binary swap
#                      (a new required config.toml key, an overlay/kernel
#                      change). The in-app updater will report it but refuse
#                      to install it -- a manual reflash is required instead.
#   --skip-build       Reuse the existing target/release/skylight-ha instead
#                      of rebuilding. Only safe if you just built it
#                      yourself and know it matches the working tree.
#   --allow-dirty      Skip the clean-working-tree check. Not recommended --
#                      the published binary should match a committed, taggable
#                      state.
#
# Requires: docker (with qemu-user-static/binfmt set up, same as any local
# build), jq, gh (authenticated).

set -eu

cd "$(dirname "$0")/.."

NOTES=""
REQUIRES_REFLASH=false
SKIP_BUILD=false
ALLOW_DIRTY=false

while [ $# -gt 0 ]; do
    case "$1" in
        -m) NOTES="$2"; shift 2 ;;
        --requires-reflash) REQUIRES_REFLASH=true; shift ;;
        --skip-build) SKIP_BUILD=true; shift ;;
        --allow-dirty) ALLOW_DIRTY=true; shift ;;
        *) echo "unknown argument: $1" >&2; exit 1 ;;
    esac
done

if [ "$ALLOW_DIRTY" = false ] && [ -n "$(git status --porcelain)" ]; then
    echo "error: working tree is dirty (use --allow-dirty to override)." >&2
    git status --short >&2
    exit 1
fi

VERSION="$(cargo metadata --no-deps --format-version1 -q \
    | jq -r '.packages[] | select(.name == "skylight-ha") | .version')"
TAG="v$VERSION"
ASSET="skylight-ha-aarch64-linux-musl"
[ -n "$NOTES" ] || NOTES="Release $TAG"

if git rev-parse "$TAG" >/dev/null 2>&1; then
    echo "error: tag $TAG already exists locally. Bump the workspace version in Cargo.toml first." >&2
    exit 1
fi
if git ls-remote --exit-code --tags origin "$TAG" >/dev/null 2>&1; then
    echo "error: tag $TAG already exists on origin. Bump the workspace version in Cargo.toml first." >&2
    exit 1
fi

echo "== releasing $TAG =="

if [ "$SKIP_BUILD" = false ]; then
    echo "-- building aarch64-unknown-linux-musl (QEMU-emulated Alpine, ~1-4h) --"
    mkdir -p .cache/cargo-registry target
    docker run --rm --platform linux/arm64 \
        -v "$PWD":/workspace -w /workspace \
        -v "$PWD/.cache/cargo-registry":/root/.cargo/registry \
        -e SKYLIGHT_GIT_SHA="$(git rev-parse --short=12 HEAD)" \
        alpine:3.20 sh -c '
            set -eu
            apk add --no-cache curl gcc libinput-dev eudev-dev libxkbcommon-dev pkgconf musl-dev linux-headers
            curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | \
                sh -s -- -y --profile minimal --default-toolchain stable
            . "$HOME/.cargo/env"
            cargo build --release -p skylight-ha \
                --no-default-features -F ui/backend-linuxkms
        '
else
    echo "-- skipping build, reusing target/release/skylight-ha --"
fi

[ -f target/release/skylight-ha ] || { echo "error: target/release/skylight-ha not found." >&2; exit 1; }

echo "-- preflight: verifying the binary reports $VERSION on emulated aarch64 --"
mkdir -p dist
cp target/release/skylight-ha "dist/$ASSET"
chmod 755 "dist/$ASSET"
REPORTED="$(docker run --rm --platform linux/arm64 \
    -v "$PWD/dist":/dist:ro \
    alpine:3.20 sh -c '
        set -eu
        apk add --no-cache libinput eudev-libs libxkbcommon libgcc >/dev/null
        /dist/'"$ASSET"' --version
    ' | head -n 1 | cut -d' ' -f2)"
if [ "$REPORTED" != "$VERSION" ]; then
    echo "error: binary reports version '$REPORTED', expected '$VERSION'. Did the build pick up a stale target/ from a different branch?" >&2
    exit 1
fi
echo "binary confirms version $VERSION"

echo "-- rendering manifest.json --"
SHA256="$(sha256sum "dist/$ASSET" | cut -d' ' -f1)"
SIZE="$(stat -c%s "dist/$ASSET")"
jq -n \
    --arg version "$VERSION" \
    --arg asset "$ASSET" \
    --arg sha256 "$SHA256" \
    --argjson size "$SIZE" \
    --argjson requires_reflash "$REQUIRES_REFLASH" \
    --arg notes "$NOTES" \
    '{version:$version, asset:$asset, sha256:$sha256, size:$size, requires_reflash:$requires_reflash, notes:$notes}' \
    > dist/manifest.json
cat dist/manifest.json

echo "-- tagging and pushing $TAG --"
TAG_MESSAGE="$NOTES"
if [ "$REQUIRES_REFLASH" = true ]; then
    TAG_MESSAGE="$TAG_MESSAGE
requires-reflash: true"
fi
git tag -a "$TAG" -m "$TAG_MESSAGE"
git push origin "$TAG"

echo "-- creating the GitHub Release --"
case "$VERSION" in
    *-*) PRERELEASE="--prerelease" ;;
    *)   PRERELEASE="" ;;
esac
gh release create "$TAG" \
    "dist/$ASSET" \
    "dist/manifest.json" \
    --title "$TAG" \
    --notes "$NOTES" \
    --verify-tag \
    $PRERELEASE

echo "== done: $TAG published =="
echo "verify with: curl -sL https://github.com/$(gh repo view --json nameWithOwner -q .nameWithOwner)/releases/latest/download/manifest.json"
