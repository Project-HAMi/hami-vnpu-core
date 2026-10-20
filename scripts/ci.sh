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

# Keep test-linked artifacts separate from deployable builds. No CANN SDK or
# driver is used: every unexpected call into these link stubs aborts the test.
export CARGO_TARGET_DIR="$work_dir/target"
export ASCEND_TOOLKIT_HOME="$work_dir/toolkit"
export ASCEND_DRIVER_HOME="$work_dir/driver"
unset HAMI_SOURCE_ROOT
runtime_dir="$ASCEND_TOOLKIT_HOME/lib64"
driver_dir="$ASCEND_DRIVER_HOME/lib64/driver"
mkdir -p "$runtime_dir" "$driver_dir"
cc -std=c11 -Wall -Wextra -Werror -shared -fPIC \
    scripts/ci/link-stubs.c -DSTUB_RUNTIME -o "$runtime_dir/libruntime.so"
cc -std=c11 -Wall -Wextra -Werror -shared -fPIC \
    scripts/ci/link-stubs.c -DSTUB_DCMI -o "$driver_dir/libdcmi.so"
export LD_LIBRARY_PATH="$runtime_dir:$driver_dir${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

rustc --version
cargo --version
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo test --manifest-path test/manager_cpu/Cargo.toml --locked
cargo test --manifest-path test/memory_cpu/Cargo.toml --locked -- --test-threads=1
python3 test/worker_cpu/run.py
bash test/process_local.sh
cargo build --release --locked -p hook
python3 test/spatial_cpu/run.py "$CARGO_TARGET_DIR/release/libvnpu.so"
