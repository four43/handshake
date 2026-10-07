#!/usr/bin/env bash
# Run cargo in the same toolchain image as the Dockerfile; cargo is not needed on the host.
# Usage: scripts/test.sh [cargo test args...]   e.g. scripts/test.sh -- --ignored
# CARGO_CMD overrides the subcommand, e.g. CARGO_CMD="build --release --locked" scripts/test.sh
# The registry and target dir live in named volumes (target outside /src), so
# rebuilds are fast and no root-owned build output lands in the repo.
set -euo pipefail
cd "$(dirname "$0")/.."
tty=()
[ -t 1 ] && tty=(-t)
exec docker run --rm "${tty[@]}" \
    -v "$PWD":/src -w /src \
    -v handshake-cargo:/usr/local/cargo/registry \
    -v handshake-target:/target -e CARGO_TARGET_DIR=/target -e UPDATE_SCHEMAS \
    rust:1.91-slim-bookworm \
    cargo ${CARGO_CMD:-test} "$@"
