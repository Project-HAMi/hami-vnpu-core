#!/usr/bin/env python3
"""Real-NPU smoke test for lazy runtime loading and DCMI process isolation.

Run in a disposable Ascend/CANN environment with the candidate libvnpu.so and
vnpu-dcmi installed together. This does not alter an existing service's registry.
Eager operators can bypass the intercepted compute APIs: a pass here does not
prove admission scheduling or vLLM startup. Validate those with a graph-serving
workload and inspect actual token consumption. Admission observations are reported.
"""
import argparse
import json
import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import time


def actor(index):
    import torch
    import torch_npu  # noqa: F401

    torch.npu.set_device(0)
    torch.manual_seed(1)
    model = torch.nn.Linear(128, 128, bias=False).to(device="npu", dtype=torch.float16)
    inputs = torch.ones((128, 128), device="npu", dtype=torch.float16)
    for step in range(12):
        started = time.monotonic()
        with torch.inference_mode():
            result = model(inputs)
            torch.npu.synchronize()
        assert torch.isfinite(result).all().item()
        print("VNPU_RESULT " + json.dumps({"pid": os.getpid(), "actor": index,
              "step": step, "seconds": time.monotonic() - started}), flush=True)
        time.sleep(3.5 if index == 0 and step == 0 else 0.5)


def local_sample(path):
    matches = list(path.parent.glob(path.name + ".device-*"))
    if len(matches) > 1:
        raise RuntimeError("single-device actor created multiple physical domains")
    if not matches:
        return None
    try:
        data = matches[0].read_bytes()
    except FileNotFoundError:
        return None
    if len(data) < 6512:
        return None
    return {"state": struct.unpack_from("<I", data, 24)[0],
            "batch": struct.unpack_from("<Q", data, 32)[0],
            "tokens": struct.unpack_from("<Q", data, 40)[0],
            "manager_pid": struct.unpack_from("<i", data, 6200)[0],
            "generation": struct.unpack_from("<I", data, 6204)[0],
            "active_workers": struct.unpack_from("<I", data, 48)[0],
            "last_reported_batch": max(struct.unpack_from("<Q", data, 56 + i * 32)[0] for i in range(32)),
            "heartbeat_us": struct.unpack_from("<Q", data, 6208)[0]}


def exercise(artifacts, policy, timeout):
    with tempfile.TemporaryDirectory(prefix="vnpu-ascend-regression-") as directory:
        region = Path(directory)
        children, files, samples = [], [], []
        try:
            for index in range(2):
                env = os.environ.copy()
                env.pop("NPU_DCMI_HELPER_PROCESS", None)
                for legacy in ["NPU_MEM_QUOTA", "NPU_PRIORITY", "NPU_FIXED_SHARE_RATIO", "NPU_OVERDRAFT_ENABLED"]:
                    env.pop(legacy, None)
                env.update({"LD_PRELOAD": str(artifacts / "libvnpu.so"),
                            "NPU_DCMI_HELPER_PATH": str(artifacts / "vnpu-dcmi"),
                            "NPU_GLOBAL_SHM_PATH": str(region / "global"),
                            "NPU_LOCAL_SHM_PATH": str(region / f"local-{index}"),
                            "NPU_COMPUTE_LIMIT": "80" if index == 0 else "20",
                            "NPU_MEMORY_LIMIT_MB": "0", "NPU_SCHED_POLICY": policy,
                            "RUST_LOG": "limiter::manager=debug", "PYTHONUNBUFFERED": "1"})
                output = (region / f"actor-{index}.log").open("w")
                files.append(output)
                children.append(subprocess.Popen([sys.executable, __file__, "--actor", str(index)],
                                                  env=env, stdout=output, stderr=subprocess.STDOUT))
            deadline = time.monotonic() + timeout
            while any(child.poll() is None for child in children):
                if time.monotonic() >= deadline:
                    raise TimeoutError(f"{policy}: workloads did not finish within {timeout}s")
                samples.append({"time_us": time.time_ns() // 1000,
                                "actors": [local_sample(region / f"local-{i}") for i in range(2)]})
                time.sleep(0.05)
            results = []
            for index, child in enumerate(children):
                files[index].flush()
                log = (region / f"actor-{index}.log").read_text()
                if child.returncode:
                    raise RuntimeError(f"{policy}: actor {index} exited {child.returncode}\n{log}")
                operations = [json.loads(line.removeprefix("VNPU_RESULT ")) for line in log.splitlines()
                              if line.startswith("VNPU_RESULT ")]
                assert len(operations) == 12, log
                assert max(row["seconds"] for row in operations[1:]) < 10, operations
                owners = {row["actors"][index]["manager_pid"] for row in samples
                          if row["actors"][index] and row["actors"][index]["manager_pid"]}
                assert owners == {child.pid}, ("unexpected manager takeover", owners, child.pid)
                refreshes = log.count("DCMI snapshot received:")
                assert refreshes >= 1, ("no successful isolated DCMI refresh", log)
                assert "DCMI snapshot failed:" not in log, log
                results.append({"actor": index, "pid": child.pid, "operations": operations,
                                "dcmi_refreshes": refreshes, "manager_pids": sorted(owners),
                                "admission_observed": any(row["actors"][index] and
                                    (row["actors"][index]["active_workers"] or row["actors"][index]["last_reported_batch"])
                                    for row in samples)})
            return {"policy": policy, "results": results, "samples": samples}
        except BaseException:
            for index in range(len(children)):
                files[index].flush()
                print((region / f"actor-{index}.log").read_text(), file=sys.stderr)
            raise
        finally:
            for child in children:
                if child.poll() is None:
                    child.kill()
                child.wait()
            for output in files:
                output.close()
            # When this test is PID 1 in a disposable namespace, collect helpers
            # orphaned by actor exit after their own descendant cleanup finishes.
            if os.getpid() == 1:
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    try:
                        pid, _ = os.waitpid(-1, os.WNOHANG)
                    except ChildProcessError:
                        break
                    if pid == 0:
                        time.sleep(0.02)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--actor", type=int, choices=[0, 1])
    parser.add_argument("--artifacts", type=Path)
    parser.add_argument("--report", type=Path)
    parser.add_argument("--policy", choices=["dynamic", "fixed", "both"], default="both")
    parser.add_argument("--timeout", type=float, default=90)
    args = parser.parse_args()
    if args.actor is not None:
        actor(args.actor)
        return
    if args.artifacts is None or args.report is None:
        parser.error("--artifacts and --report are required")
    artifacts = args.artifacts.resolve()
    for name in ["libvnpu.so", "vnpu-dcmi"]:
        assert (artifacts / name).is_file(), name
    results = []
    for policy in (["dynamic", "fixed"] if args.policy == "both" else [args.policy]):
        results.append(exercise(artifacts, policy, args.timeout))
        args.report.write_text(json.dumps(results, indent=2) + "\n")
        print(f"PASS {policy}: NPU/DCMI smoke; admission observed="
              f"{[actor['admission_observed'] for actor in results[-1]['results']]}", flush=True)


if __name__ == "__main__":
    main()
