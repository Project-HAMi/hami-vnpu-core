import argparse
import multiprocessing as mp
import os
import time

import torch
import torch_npu  # noqa: F401 - required to register NPU backend
from torchvision import models


MODEL_MAP = {
    "resnet50": models.resnet50,
    "resnet18": models.resnet18,
    "vgg16": models.vgg16,
    "mobilenet_v2": models.mobilenet_v2,
    "efficientnet_b0": models.efficientnet_b0,
    "alexnet": models.alexnet,
}


def parse_args():
    parser = argparse.ArgumentParser(description="Multi-process NPU load test")
    parser.add_argument("--model", type=str, default="resnet50", choices=MODEL_MAP.keys())
    parser.add_argument("--batch-size", type=int, default=32)
    parser.add_argument("--num-procs", type=int, default=2, help="Number of worker processes to launch")
    parser.add_argument("--duration", type=int, default=120, help="Seconds to run. 0 = run until interrupted.")
    parser.add_argument("--warmup", type=int, default=100, help="Warmup iterations per process")
    parser.add_argument("--log-interval", type=int, default=20, help="Iterations between log lines")
    parser.add_argument("--prio", type=int, default=50, help="Container-wide share/priority (read by manager)")
    parser.add_argument("--log-prefix", type=str, default="", help="Optional prefix for log lines")
    return parser.parse_args()


def worker(rank: int, args):
    """
    Worker process that warms up the model and then runs inference in a loop.
    """
    prefix = f"[{args.log_prefix}/P{rank}]" if args.log_prefix else f"[P{rank}]"

    device = torch.device("npu:0")
    torch.npu.set_device(device)

    model_fn = MODEL_MAP[args.model]
    model = model_fn(weights=None).to(device)
    model.eval()

    input_tensor = torch.randn(args.batch_size, 3, 224, 224, device=device)

    # Warmup
    with torch.no_grad():
        for _ in range(args.warmup):
            _ = model(input_tensor)
    torch.npu.synchronize()
    print(f"{prefix} warmup complete (container prio={os.getenv('NPU_PRIORITY', 'unset')})", flush=True)

    iterations = 0
    start_time = time.time()
    last_log_time = start_time

    try:
        while True:
            with torch.no_grad():
                _ = model(input_tensor)
            iterations += 1

            if iterations % args.log_interval == 0:
                torch.npu.synchronize()
                now = time.time()
                elapsed = now - last_log_time
                speed = args.log_interval / elapsed if elapsed > 0 else 0.0
                img_sec = speed * args.batch_size

                print(
                    f"{prefix} iter={iterations:6d} | speed={speed:6.2f} it/s | img/s={img_sec:7.1f}",
                    flush=True,
                )
                last_log_time = now

            if args.duration > 0 and (time.time() - start_time) >= args.duration:
                break

    except KeyboardInterrupt:
        print(f"{prefix} interrupted at iter={iterations}", flush=True)
    finally:
        torch.npu.synchronize()
        print(f"{prefix} finished after {iterations} iterations", flush=True)


def main():
    args = parse_args()
    mp.set_start_method("spawn", force=True)

    print(
        f"{'='*60}\n"
        f"Launching {args.num_procs} process(es)\n"
        f"Model: {args.model} | Batch: {args.batch_size} | Duration: {args.duration}s\n"
        f"Container priority (env NPU_PRIORITY): {args.prio}\n"
        f"{'='*60}",
        flush=True,
    )

    # Ensure all workers inherit the same container priority
    os.environ["NPU_PRIORITY"] = str(args.prio)

    processes = []
    for idx in range(args.num_procs):
        p = mp.Process(target=worker, args=(idx, args), daemon=False)
        p.start()
        processes.append(p)

    for p in processes:
        p.join()

    print("All processes completed.", flush=True)


if __name__ == "__main__":
    main()
