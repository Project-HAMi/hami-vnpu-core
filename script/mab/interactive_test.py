import torch
import torch_npu
import time
import sys
import argparse
import threading
import select
from torchvision import models

# --- GLOBAL CONTROL FLAGS ---
RUNNING = False
EXIT_FLAG = False

def input_listener():
    """Listens for user input in a background thread."""
    global RUNNING, EXIT_FLAG
    print("\n" + "="*60)
    print(" >>> INTERACTIVE CONTROLS <<<")
    print(" [s] START  : Begin inference loop")
    print(" [p] PAUSE  : Pause inference (release resources)")
    print(" [q] QUIT   : Exit the program")
    print(" [r] RESET  : Reset counters")
    print("="*60 + "\n", flush=True)
    
    while not EXIT_FLAG:
        # Check if there is input on stdin (non-blocking)
        if select.select([sys.stdin], [], [], 0.5)[0]:
            cmd = sys.stdin.readline().strip().lower()
            if cmd == 's':
                if not RUNNING:
                    print("\n>>> STARTING BENCHMARK...", flush=True)
                    RUNNING = True
            elif cmd == 'p':
                if RUNNING:
                    print("\n>>> PAUSING...", flush=True)
                    RUNNING = False
            elif cmd == 'q':
                print("\n>>> QUITTING...", flush=True)
                EXIT_FLAG = True
                RUNNING = False
            elif cmd == 'r':
                print("\n>>> RESETTING STATS...", flush=True)
            else:
                pass # Ignore empty lines

MODEL_MAP = {
    "resnet50": models.resnet50,
    "resnet18": models.resnet18,
    "vgg16": models.vgg16,
    "mobilenet_v2": models.mobilenet_v2,
    "efficientnet_b0": models.efficientnet_b0,
    "alexnet": models.alexnet,
}

def get_args():
    parser = argparse.ArgumentParser(description="Interactive NPU Load Test")
    parser.add_argument("--model", type=str, default="resnet50", choices=MODEL_MAP.keys())
    parser.add_argument("--batch-size", type=int, default=32)
    parser.add_argument("--prio", type=int, default=50, help="Share percentage (Priority)")
    return parser.parse_args()

def run_benchmark():
    global RUNNING, EXIT_FLAG
    args = get_args()
    
    print(f"\n{'='*60}")
    print(f"  ID: {args.model.upper()} | SHARE: {args.prio}% | BATCH: {args.batch_size}")
    print(f"{'='*60}", flush=True)

    # 1. Setup Model
    try:
        model_fn = MODEL_MAP[args.model]
        # weights=None for random init (fast, no download)
        model = model_fn(weights=None) 
    except KeyError:
        print(f"Error: Model {args.model} not found.")
        return

    device = torch.device("npu:0")
    model = model.to(device)
    model.eval()

    # 2. Prepare Data
    input_tensor = torch.randn(args.batch_size, 3, 224, 224, device=device)

    # 3. Warmup
    print(">>> Warming up NPU (200 iterations)...", flush=True)
    with torch.no_grad():
        for _ in range(200):
            _ = model(input_tensor)
    torch.npu.synchronize()
    print(">>> Warmup Complete. Waiting for command...", flush=True)

    # Start Input Listener
    t = threading.Thread(target=input_listener)
    t.start()

    # 4. Main Loop
    iteration = 0
    log_interval = 50
    start_time = None
    
    try:
        with torch.no_grad():
            while not EXIT_FLAG:
                if RUNNING:
                    if start_time is None:
                        # Reset timing on start/unpause
                        torch.npu.synchronize() 
                        start_time = time.time()
                        last_log_time = start_time
                        iteration = 0

                    # --- INFERENCE ---
                    _ = model(input_tensor)
                    iteration += 1
                    
                    # --- LOGGING ---
                    if iteration % log_interval == 0:
                        # CRITICAL: Sync before taking the timestamp
                        torch.npu.synchronize()
                        current_time = time.time()
                        
                        elapsed = current_time - last_log_time
                        speed = log_interval / elapsed
                        img_sec = speed * args.batch_size
                        
                        print(f"[{args.model}] Share: {args.prio}% | Iter: {iteration:6d} | Speed: {speed:6.2f} it/s | Img/s: {img_sec:7.1f}", flush=True)
                        last_log_time = current_time
                else:
                    # Idle loop to prevent CPU spin when paused
                    if start_time is not None:
                        start_time = None # Mark as needing reset
                    time.sleep(0.1)
                        
    except KeyboardInterrupt:
        EXIT_FLAG = True

    t.join()
    print("Exited.")

if __name__ == "__main__":
    run_benchmark()