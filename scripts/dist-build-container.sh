#!/bin/sh
set -eu

target=${1:?Usage: dist-build-container.sh TARGET}
engine=${CONTAINER_ENGINE:-podman}
rust_image=${DIST_RUST_IMAGE:-docker.io/library/rust:1.98.0-trixie}
dist_version=${CARGO_DIST_VERSION:-0.32.0}

case "$target" in
    x86_64-unknown-linux-gnu) platform=${DIST_AMD64_RUNNER_PLATFORM:-linux/arm64} ;;
    aarch64-unknown-linux-gnu) platform=linux/arm64 ;;
    *) echo "unsupported dist target: $target" >&2; exit 2 ;;
esac

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_dir=$(CDPATH= cd -- "$script_dir/.." && pwd)
"$engine" run --rm --platform "$platform" \
    --mount "type=bind,src=$repo_dir,dst=/work" \
    --workdir /work \
    "$rust_image" sh -eu -c '
        rustup toolchain install 1.98.1 --profile minimal
        host=$(rustc -vV | sed -n "s/^host: //p")
        if [ "$host" != "$1" ]; then
            apt-get update
            apt-get install -y --no-install-recommends python3-pip
            python3 -m pip install --break-system-packages ziglang
            cargo install cargo-zigbuild --locked
            rustup target add "$1"
        fi
        if ! command -v dist >/dev/null 2>&1 || [ "$(dist --version)" != "cargo-dist '"$dist_version"'" ]; then
            cargo install cargo-dist --locked --version '"$dist_version"'
        fi
        dist build --artifacts=local --target "$1"
    ' sh "$target"
