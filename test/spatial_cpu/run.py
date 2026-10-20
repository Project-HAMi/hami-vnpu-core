#!/usr/bin/env python3
"""Exercise the actual built Rust libvnpu.so against a fake ACL, with no NPU.

Usage: python3 test/spatial_cpu/run.py target/release/libvnpu.so
The library's normal runtime/DCMI dependencies must be resolvable in the build
environment; no CANN resource call reaches those libraries in these tests.
"""
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import tempfile


def main():
    if platform.system() != "Linux":
        raise RuntimeError("Linux is required for the LD_PRELOAD integration tests")
    if len(sys.argv) != 2:
        raise SystemExit("Usage: run.py /absolute/path/to/libvnpu.so")
    library = Path(sys.argv[1]).resolve(strict=True)
    source = Path(__file__).resolve().parent
    results = []
    with tempfile.TemporaryDirectory(prefix="hami-spatial-cpu-") as directory:
        work = Path(directory)
        backends = {}
        for variant, flags in [("full", []), ("missing-get", ["-DOMIT_GET"]),
                               ("missing-set", ["-DOMIT_SET"]),
                               ("missing-stream-get", ["-DOMIT_STREAM_GET"])]:
            target = work / variant
            target.mkdir()
            subprocess.run(["cc", "-std=c11", "-Wall", "-Wextra", "-Werror", "-shared", "-fPIC",
                            *flags, str(source / "fake_acl.c"), "-o", str(target / "libfake_acl.so")], check=True)
            backends[variant] = target
        executable = work / "client"
        subprocess.run(["cc", "-std=c11", "-Wall", "-Wextra", "-Werror", str(source / "client.c"),
                        "-L" + str(backends["full"]), "-lfake_acl", "-pthread",
                        "-Wl,-rpath," + str(backends["full"]), "-o", str(executable)], check=True)

        def case(name, percent="33", mode="initial", expected=None, code=0,
                 policy="spatial", backend="full", settings=None, error=None):
            env = {key: value for key, value in os.environ.items()
                   if not key.startswith("FAKE_") and key not in {
                       "NPU_MEMORY_LIMIT_MB", "NPU_COMPUTE_LIMIT", "NPU_SCHED_POLICY", "NPU_LOCAL_SHM_PATH",
                       "NPU_MEM_QUOTA", "NPU_PRIORITY", "NPU_FIXED_SHARE_RATIO", "NPU_OVERDRAFT_ENABLED"}}
            env.update(LD_PRELOAD=str(library),
                       LD_LIBRARY_PATH=str(backends[backend]) + ":" + env.get("LD_LIBRARY_PATH", ""),
                       NPU_GLOBAL_SHM_PATH=str(work / (name + "-global")),
                       NPU_LOCAL_SHM_PATH=str(work / (name + "-local")))
            if percent is not None:
                env["NPU_COMPUTE_LIMIT"] = str(percent)
            if policy is not None:
                env["NPU_SCHED_POLICY"] = policy
            env.update(settings or {})
            run = subprocess.run([str(executable), mode], env=env, capture_output=True,
                                 text=True, timeout=20)
            assert run.returncode == code, (name, run.returncode, run.stdout, run.stderr)
            if expected is not None:
                assert run.stdout.splitlines() == expected, (name, run.stdout, run.stderr)
            if code == 78:
                assert "fatal" in run.stderr, (name, run.stderr)
            if error:
                assert error in run.stderr, (name, run.stderr)
            if policy == "spatial" and code == 0 and mode not in {"fork-after", "fork-locked"}:
                assert "rounding=ceil cooperative=1" in run.stderr, (name, run.stderr)
            results.append(dict(name=name, passed=True, returncode=run.returncode))
            print("PASS " + name, flush=True)

        for percent, cube, vector in [("1", 1, 1), ("2", 1, 1), ("5", 1, 2),
                                       ("6", 2, 3), ("10", 2, 4), ("12", 3, 5),
                                       ("22", 5, 9), ("30", 6, 12), ("31", 7, 13),
                                       ("32", 7, 13), ("33", 7, 14), ("35", 7, 14),
                                       ("60", 12, 24), ("66", 14, 27), ("95", 19, 38),
                                       ("96", 20, 39), ("99", 20, 40), ("100", 20, 40)]:
            budget = f"{cube} {vector}"
            case("ceil-" + percent, percent=percent, mode="basic",
                 expected=[budget] * 3 + [f"stream={vector}"] + [budget] * 2)
        for percent in ["", "0", "101", "-1", "10x"]:
            case("refuse-" + (percent or "empty"), percent=percent, code=78)
        case("default-one-percent-ceil", percent=None, expected=["1 1"])
        for name, settings in [
                ("setter-failure", {"FAKE_FAIL_SET": "1"}),
                ("getter-failure", {"FAKE_FAIL_GET": "1"}),
                ("setter-ignored", {"FAKE_IGNORE_SET": "1"}),
                ("readback-mismatch", {"FAKE_BAD_READBACK": "1"}),
                ("zero-cube-count", {"FAKE_CUBE": "0"}),
                ("zero-vector-count", {"FAKE_VECTOR": "0"})]:
            case(name, settings=settings, code=78)
        case("independent-vector-ceil", percent="31",
             settings={"FAKE_CUBE": "40", "FAKE_VECTOR": "20"}, expected=["13 7"])
        case("maximum-count-arithmetic", percent="100", settings={"FAKE_CUBE": "4294967295", "FAKE_VECTOR": "4294967295"},
             expected=["4294967295 4294967295"])
        for backend in ["missing-get", "missing-set"]:
            case(backend, backend=backend, code=78)
        for policy in [None, "dynamic", "fixed", "overdraft"]:
            case("passthrough-" + str(policy), policy=policy, backend="missing-get", mode="passthrough",
                 expected=["20 40", "20 40", "100 40", "stream=100", "20 40", "20 40", "gets=0"])
        case("context-initialization", mode="context", expected=["7 14"])
        case("resource-reset-reapplied", mode="device-resource-reset", expected=["7 14"] * 2)
        case("nonfinal-reset-does-not-rescale", mode="refcount-reset", expected=["7 14"] * 2)
        case("force-reset-does-not-rescale", mode="force-reset", expected=["7 14"])
        case("failed-reset-preserves-cap", mode="failed-reset", settings={"FAKE_FAIL_RESET": "1"}, expected=["7 14"])
        case("stream-override-clamped", mode="stream-set", expected=["stream=7/14"])
        case("stream-reset-reapplied", mode="stream-reset", expected=["stream=7/14"])
        for name, backend, settings in [
                ("stream-get-missing", "missing-stream-get", {}),
                ("stream-get-failed", "full", {"FAKE_FAIL_STREAM_GET": "1"}),
                ("stream-write-ignored", "full", {"FAKE_IGNORE_STREAM_SET": "1"})]:
            case(name, mode="stream-set", backend=backend, settings=settings, code=78)
        case("stream-write-failed-keeps-native-error", mode="stream-set",
             settings={"FAKE_FAIL_STREAM_SET": "1"}, code=2)
        case("stream-current-device-failure", mode="stream-set", settings={"FAKE_FAIL_CURRENT": "1"}, code=78)
        case("two-device-bases", mode="multi-device", settings={"FAKE_CUBE1": "40", "FAKE_VECTOR1": "80"},
             expected=["7 14", "14 27", "7 14", "14 27"])
        case("cross-device-explicit-stream-refused", mode="multi-stream", code=78,
             error="multiple devices are unsupported")
        for action in ["set", "reset"]:
            case("raw-switch-first-stream-" + action + "-refused", mode="raw-switch-stream-" + action,
                 settings={"FAKE_CUBE1": "40", "FAKE_VECTOR1": "80"}, code=78,
                 error="multiple devices are unsupported")
        case("concurrent-repeat-no-rescaling", mode="concurrent", expected=["7 14"])
        case("fork-before-initialization", mode="fork-before", expected=["7 14", "child=0", "7 14"])
        case("fork-after-initialization-refused", mode="fork-after", expected=["child=78"])
        case("fork-inherited-lock-not-touched", mode="fork-locked", settings={"FAKE_SLOW_GET": "1"}, expected=["child=78"])
        case("fork-from-internal-set-callback-refused", mode="fork-callback",
             settings={"FAKE_FORK_IN_SET": "1"}, expected=["7 14", "child=78"])
        case("fork-from-internal-get-callback-refused", mode="fork-callback",
             settings={"FAKE_FORK_IN_GET": "1"}, expected=["7 14", "child=78"])
        case("direct-runtime-first-selection", mode="direct-runtime", code=97,
             settings={"FAKE_STOP_RUNTIME": "1"})
        case("direct-runtime-device-switch", mode="direct-runtime-switch", code=97,
             settings={"FAKE_STOP_RUNTIME": "1", "FAKE_CUBE1": "40", "FAKE_VECTOR1": "80",
                       "FAKE_EXPECT_CUBE": "14", "FAKE_EXPECT_VECTOR": "27"})
    print(json.dumps(dict(status="passed", library=str(library), cases=len(results),
                         no_npu=True, results=results), indent=2))


if __name__ == "__main__":
    main()
