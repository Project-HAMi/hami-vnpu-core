#!/usr/bin/env bash
# Exercise the actual interceptor against an ACL implementation with known ABI.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
fixture="$(mktemp -d "${TMPDIR:-/tmp}/vnpu-hostargs.XXXXXX")"
trap 'rm -rf "$fixture"' EXIT
mkdir -p "$fixture/lib64/driver"
cc -shared -fPIC "$root/test/host_args/runtime.c" -o "$fixture/lib64/libruntime.so"
cp "$fixture/lib64/libruntime.so" "$fixture/lib64/driver/libdcmi.so"
cc -shared -fPIC "$root/test/host_args/acl.c" -L"$fixture/lib64" -lruntime -ldl -o "$fixture/lib64/libascendcl.so"
cc "$root/test/host_args/probe.c" -ldl -pthread -o "$fixture/probe"
if test -z "${HAMI_ARTIFACT_DIR:-}"; then
    ASCEND_TOOLKIT_HOME="$fixture" ASCEND_DRIVER_HOME="$fixture" \
        cargo build --manifest-path "$root/Cargo.toml" --locked -p hook
fi
artifacts="${HAMI_ARTIFACT_DIR:-${CARGO_TARGET_DIR:-$root/target}/debug}"
for visibility in global local; do
    env -u NPU_MEM_QUOTA -u NPU_PRIORITY -u NPU_FIXED_SHARE_RATIO \
        -u NPU_OVERDRAFT_ENABLED -u NPU_DCMI_HELPER_PROCESS \
        LD_LIBRARY_PATH="$fixture/lib64:$fixture/lib64/driver${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
        LD_PRELOAD="$artifacts/libvnpu.so" NPU_LOCAL_SHM_PATH="$fixture/$visibility.shm" \
        NPU_GLOBAL_SHM_PATH="$fixture/$visibility.global" NPU_SCHED_POLICY=dynamic \
        NPU_MEMORY_LIMIT_MB=0 NPU_COMPUTE_LIMIT=100 \
        timeout 15 "$fixture/probe" "$visibility"
done
