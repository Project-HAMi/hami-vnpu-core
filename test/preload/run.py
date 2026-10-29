#!/usr/bin/env python3
"""Preload the built library without CANN, then load a backend after a miss."""

import os
from pathlib import Path
import platform
import re
import subprocess
import sys
import tempfile


def main():
    if platform.system() != "Linux" or len(sys.argv) != 2:
        raise SystemExit("Usage on Linux: run.py /absolute/path/to/libvnpu.so")
    library = Path(sys.argv[1]).resolve(strict=True)
    source = Path(__file__).resolve().parent
    dynamic = subprocess.check_output(["readelf", "-dW", str(library)], text=True)
    needed = set(re.findall(r"\(NEEDED\).*?\[(.*?)\]", dynamic))
    assert not needed.intersection({"libruntime.so", "libascendcl.so", "libdcmi.so"}), dynamic
    symbols = subprocess.check_output(["readelf", "--dyn-syms", "--wide", str(library)], text=True)
    assert not re.search(r"\bGLOBAL\s+DEFAULT\s+(?!UND\b)\S+\s+sigaction\b", symbols), symbols

    with tempfile.TemporaryDirectory(prefix="hami-preload-") as directory:
        work = Path(directory)
        probe = work / "probe"
        subprocess.run(["cc", "-std=c11", "-Wall", "-Wextra", "-Werror",
                        str(source / "probe.c"), "-ldl", "-o", str(probe)], check=True)
        headers = subprocess.check_output(["readelf", "-lW", str(probe)], text=True,
                                          env=dict(os.environ, LC_ALL="C"))
        interpreter = re.search(r"Requesting program interpreter: ([^\]]+)\]", headers)
        assert interpreter, headers
        # Developer hosts can have an SDK or old link stubs registered in ld.so.cache.
        # Absent-backend cases must not accidentally open those instead of the fixture.
        isolated_probe = [interpreter.group(1), "--inhibit-cache", str(probe)]
        forbidden = work / "forbidden"
        forbidden.mkdir()
        for name in ("libruntime.so", "libascendcl.so", "libdcmi.so"):
            subprocess.run(["cc", "-shared", "-fPIC", "-DFORBID_LOAD",
                            str(source / "runtime.c"), "-o", str(forbidden / name)], check=True)
        backend = work / "libruntime.so"
        subprocess.run(["cc", "-shared", "-fPIC", "-Wl,-soname,libruntime.so",
                        str(source / "runtime.c"), "-o", str(backend)], check=True)
        clean = {key: value for key, value in os.environ.items()
                 if not key.startswith("NPU_") and key not in {"LD_PRELOAD", "LD_AUDIT", "LD_LIBRARY_PATH"}}
        clean["LD_PRELOAD"] = str(library)

        def case(name, command, settings=None, code=0):
            env = dict(clean, **(settings or {}))
            run = subprocess.run(command, env=env, text=True, capture_output=True, timeout=15)
            assert run.returncode == code, (name, run.returncode, run.stdout, run.stderr)
            assert not list(work.glob("*.shm*")), (name, list(work.iterdir()))
            print("PASS " + name, flush=True)

        for label, settings in [
            ("no-sdk", {}),
            ("sdk-must-stay-unloaded", {"LD_LIBRARY_PATH": str(forbidden)}),
            ("configured-but-unused", {
                "LD_LIBRARY_PATH": str(forbidden),
                "NPU_GLOBAL_SHM_PATH": str(work / "global.shm"),
                "NPU_LOCAL_SHM_PATH": str(work / "local.shm"),
                "NPU_MEMORY_LIMIT_MB": "1024",
            }),
        ]:
            for name, command in [
                ("true", ["/bin/true"]),
                ("ls", ["/bin/ls", str(work)]),
                ("bash", ["/bin/bash", "--noprofile", "--norc", "-c", ":"]),
                ("signals-and-threads", [str(probe), "idle"]),
            ]:
                case(label + "-" + name, command, settings)
        case("missing-optional-runtime", [*isolated_probe, "missing"])
        for visibility in ("global", "local"):
            case("retry-after-" + visibility + "-dlopen",
                 [*isolated_probe, "late", visibility, str(backend)])
        case("configured-missing-runtime-fails-closed", [*isolated_probe, "missing"], {
            "NPU_GLOBAL_SHM_PATH": str(work / "global.shm"),
            "NPU_LOCAL_SHM_PATH": str(work / "local.shm"),
            "NPU_SCHED_POLICY": "dynamic",
        }, code=78)


if __name__ == "__main__":
    main()
