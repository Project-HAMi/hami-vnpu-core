#!/usr/bin/env python3
"""Run the actual worker and Hook CPU cases in bounded, isolated Linux processes.

CANN calls are test doubles; the production worker, Hook bodies, shared layout,
and Linux futex implementation are compiled from this checkout. No NPU is used.
"""
import json
import os
from pathlib import Path
import platform
import signal
import subprocess


def run(argv, timeout):
    process = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                               start_new_session=True, text=True)
    try:
        output, _ = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        output, _ = process.communicate()
        raise RuntimeError("Timed out: " + " ".join(argv) + "\n" + output)
    if process.returncode:
        raise RuntimeError("Failed: " + " ".join(argv) + "\n" + output)
    return output


def main():
    if platform.system() != "Linux":
        raise RuntimeError("Linux required; the tests exercise real futex waits")
    manifest = Path(__file__).resolve().parent / "Cargo.toml"
    output = run(["cargo", "test", "--manifest-path", str(manifest), "--locked",
                  "--no-run", "--message-format=json"], 180)
    binaries = []
    for line in output.splitlines():
        if line.startswith("{"):
            row = json.loads(line)
            if (row.get("reason") == "compiler-artifact" and row.get("executable")
                    and row.get("profile", {}).get("test")):
                binaries.append(row["executable"])
    if len(binaries) != 1:
        raise RuntimeError("Expected exactly one worker test binary")
    binary = binaries[0]
    cases = [line.removesuffix(": test") for line in run([binary, "--list"], 10).splitlines()
             if line.startswith("worker::audit::") and line.endswith(": test")]
    if not cases:
        raise RuntimeError("No worker cases discovered")
    for case in cases:
        run([binary, "--exact", case, "--test-threads=1", "--nocapture"], 15)
        print("PASS " + case, flush=True)
    print("PASS: %d Linux CPU cases; CANN mocked, no NPU performance claim" % len(cases))


if __name__ == "__main__":
    main()
