#!/usr/bin/env python3
"""Verify native memory-query semantics through the actual Linux preload library.

CANN and per-card accounting state are controlled fixtures; no NPU is required.
"""
import os
from pathlib import Path
import platform
import subprocess
import sys
import tempfile


def main():
    if platform.system() != "Linux" or len(sys.argv) != 2:
        raise SystemExit("Usage: run.py /absolute/path/to/libvnpu.so (Linux)")
    library = Path(sys.argv[1]).resolve(strict=True)
    source = Path(__file__).resolve().parent
    mib = 1024 * 1024
    with tempfile.TemporaryDirectory(prefix="vnpu-memory-info-") as directory:
        root = Path(directory)
        subprocess.run(["cc", "-std=c11", "-Wall", "-Wextra", "-Werror", "-shared", "-fPIC",
                        str(source / "runtime.c"), "-o", str(root / "libruntime.so")], check=True, timeout=60)
        (root / "libascendcl.so").symlink_to("libruntime.so")
        subprocess.run(["cc", "-std=c11", "-Wall", "-Wextra", "-Werror",
                        str(source / "probe.c"), "-L" + str(root), "-lruntime",
                        "-o", str(root / "probe")], check=True, timeout=60)
        clean = {k: v for k, v in os.environ.items()
                 if not k.startswith(("NPU_", "FAKE_"))
                 and k not in {"LD_PRELOAD", "LD_AUDIT", "LD_LIBRARY_PATH"}}
        failures = []
        count = 0

        def case(name, kind, free, total, result=0, settings=None, configured=True,
                 device_queries=1, null_output=0):
            nonlocal count
            work = root / name
            work.mkdir()
            env = dict(clean, LD_PRELOAD=str(library), LD_LIBRARY_PATH=str(root))
            if configured:
                env.update(NPU_GLOBAL_SHM_PATH=str(work / "global"),
                           NPU_LOCAL_SHM_PATH=str(work / "local"),
                           NPU_MEMORY_LIMIT_MB="4096", NPU_SCHED_POLICY="dynamic")
            env.update(settings or {})
            run = subprocess.run([str(root / "probe"), str(kind), str(result), str(free),
                                  str(total), str(device_queries), str(null_output)],
                                 env=env, text=True, capture_output=True, timeout=15)
            count += 1
            if run.returncode:
                failures.append((name, run.returncode, run.stderr))
            else:
                print("PASS " + name, flush=True)

        case("aggregate-quota", 1, 2944 * mib, 4096 * mib)
        case("nonaggregate-pool-over-quota", 2, 2944 * mib, 4096 * mib)
        case("ddr-pool", 0, 1024 * mib, 2048 * mib)
        case("huge-page-pool", 4, 256 * mib, 512 * mib)
        case("exhausted-normal-pool", 5, 0, 256 * mib)
        case("empty-p2p-pool", 8, 0, 0)
        case("additional-native-type", 12, 32 * mib, 64 * mib)
        case("invalid-type-error", 99, 333, 444, result=107001, device_queries=0)
        case("unsupported-feature-error", 10, 333, 444, result=207000, device_queries=0)
        case("null-free-error", 1, 111, 222, result=101, null_output=1, device_queries=0)
        case("null-total-error", 1, 111, 222, result=101, null_output=2, device_queries=0)
        case("physical-card-remapping", 1, 1920 * mib, 4096 * mib, settings={"FAKE_DEVICE": "1"})
        case("exhausted-quota", 1, 0, 4096 * mib, settings={"FAKE_USED_MB": "4096"})
        case("small-quota-overhead", 1, 0, 64 * mib,
             settings={"NPU_MEMORY_LIMIT_MB": "64", "FAKE_USED_MB": "0"})
        case("unlimited", 1, 6144 * mib, 8192 * mib, settings={"NPU_MEMORY_LIMIT_MB": "0"})
        case("unlimited-cold-query", 1, 6144 * mib, 8192 * mib,
             settings={"NPU_MEMORY_LIMIT_MB": "0", "FAKE_COLD_QUERY": "1"})
        case("unconfigured", 1, 6144 * mib, 8192 * mib, configured=False, device_queries=0)
        case("no-current-device", 1, 6144 * mib, 8192 * mib, settings={"FAKE_NO_DEVICE": "1"})
        case("spatial-cold-query", 1, 2944 * mib, 4096 * mib, settings={"NPU_SCHED_POLICY": "spatial"})
        if failures:
            raise AssertionError(f"{len(failures)}/{count} memory-query cases failed: {failures}")
        print(f"PASS: {count} native memory-query cases; fake runtime, no NPU validation")


if __name__ == "__main__":
    main()
