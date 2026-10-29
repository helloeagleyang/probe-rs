#!/usr/bin/env bash
# Build the small hpm53x1-flavor algorithms for 1 MiB SiP HPM parts.
#
# SPDX-License-Identifier: MIT OR Apache-2.0
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd -- "$root/../.." && pwd)
rust_dir="$root/rust-small"
build_root=${HPM_SMALL_ALGO_BUILD_DIR:-"$root/build"}

if ! command -v cargo >/dev/null 2>&1; then
    echo "cargo not found" >&2
    exit 1
fi
if ! rustup target list --installed 2>/dev/null | grep -qx riscv32imac-unknown-none-elf; then
    echo "missing Rust target riscv32imac-unknown-none-elf" >&2
    echo "run: rustup target add riscv32imac-unknown-none-elf" >&2
    exit 1
fi

# Use the workspace toolchain without inheriting the workspace cargo source
# replacement (which may point at an unreachable mirror).
toolchain=$(sed -n 's/^channel = "\(.*\)"/\1/p' "$repo_root/rust-toolchain.toml" 2>/dev/null || true)
export RUSTUP_TOOLCHAIN=${RUSTUP_TOOLCHAIN:-$toolchain}

workdir=$(mktemp -d)
trap 'rm -rf "$workdir"' EXIT

build_one() {
    local feature=$1
    local name=$2
    local target_dir="$build_root/rust-$name"
    local out="$build_root/$name"

    (
        cd "$workdir"
        CARGO_TARGET_DIR="$target_dir" \
        RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=--nmagic -C link-arg=-T$rust_dir/link.x -C link-arg=-Tmemory.x -C link-arg=-eInit" \
        cargo build --release \
            --manifest-path "$rust_dir/Cargo.toml" \
            --target riscv32imac-unknown-none-elf \
            --features "$feature"
    )
    mkdir -p "$out"
    cp "$target_dir/riscv32imac-unknown-none-elf/release/flash-algo" "$out/hpm.elf"
    echo "built $name -> $out/hpm.elf"
}

build_one xpi-f300 flash-algo-hpm-xpi0-f300-1mb
build_one xpi-f00e flash-algo-hpm-xpi0-f00e-1mb
