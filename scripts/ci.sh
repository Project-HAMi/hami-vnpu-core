#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != Linux ]]; then
    echo 'The CPU suite requires Linux (futex, procfs, LD_PRELOAD and fork).' >&2
    exit 1
fi

repo_dir="$(cd "$(dirname "$0")/.." && pwd)"
work_dir="$(mktemp -d "${TMPDIR:-/tmp}/hami-core-ci.XXXXXX")"
trap 'rm -rf "$work_dir"' EXIT
cd "$repo_dir"

# No SDK/driver link stubs are needed: libvnpu resolves backends on first use.
# Keep CPU-test artifacts separate from verified release builds.
export CARGO_TARGET_DIR="$work_dir/target"
unset HAMI_SOURCE_ROOT

rustc --version
cargo --version
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo test --manifest-path test/manager_cpu/Cargo.toml --locked
cargo test --manifest-path test/memory_cpu/Cargo.toml --locked -- --test-threads=1
python3 test/worker_cpu/run.py
bash test/process_local.sh
cargo build --release --locked --workspace
python3 test/preload/run.py "$CARGO_TARGET_DIR/release/libvnpu.so"
python3 test/spatial_cpu/run.py "$CARGO_TARGET_DIR/release/libvnpu.so"
HAMI_ARTIFACT_DIR="$CARGO_TARGET_DIR/release" bash test/host_args.sh
HAMI_ARTIFACT_DIR="$CARGO_TARGET_DIR/release" bash test/dcmi_helper.sh
