import torch
import time
import torch_npu
import sys

def run_benchmark():
    # Parameters
    N = 4096 
    LIMIT = 100000        # Total operations
    LOG_INTERVAL = 1000  # Sync and report every 1000 ops
    # device_id = 1 
    # if device_id >= torch.npu.device_count():
    #     print(f"Error: NPU {device_id} not found. Total NPUs: {torch.npu.device_count()}")
    #     return
    
    # torch.npu.set_device(device_id)
    print(f"Initializing tensors ({N}x{N}, float16)...", flush=True)
    
    torch.manual_seed(0)
    x = torch.randn(N, N, dtype=torch.float16).npu()
    y = torch.randn(N, N, dtype=torch.float16).npu()
    
    print("Warming up...", flush=True)
    for _ in range(50):
        torch.matmul(x, y)
    torch.npu.synchronize()

    print(f"Starting benchmark: {LIMIT} total ops, syncing every {LOG_INTERVAL} ops.", flush=True)
    print(f"{'-'*70}", flush=True)
    
    total_start_time = time.time()
    last_sync_time = total_start_time

    try:
        for iteration in range(1, LIMIT + 1):
            torch.matmul(x, y)

            # Middle Sync Point
            if iteration % LOG_INTERVAL == 0:
                torch.npu.synchronize()
                current_time = time.time()
                
                # 1. Moving Speed (Current Interval)
                interval_elapsed = current_time - last_sync_time
                moving_speed = LOG_INTERVAL / interval_elapsed
                
                # 2. Total Average Speed (From start to now)
                total_elapsed_so_far = current_time - total_start_time
                total_avg_speed = iteration / total_elapsed_so_far
                
                print(f"Iter: {iteration:5d} | "
                      f"Moving Speed: {moving_speed:7.2f} it/s | "
                      f"Total Avg: {total_avg_speed:7.2f} it/s | "
                      f"Time: {total_elapsed_so_far:6.2f}s", flush=True)
                
                last_sync_time = current_time

        # Final cleanup sync just in case LIMIT isn't a multiple of LOG_INTERVAL
        torch.npu.synchronize()
        final_end_time = time.time()
        total_duration = final_end_time - total_start_time

        print(f"{'-'*70}")
        print(f"FINAL RESULTS")
        print(f"Total Iterations: {LIMIT}")
        print(f"Total Runtime:    {total_duration:.4f} seconds")
        print(f"Final Avg Speed:  {LIMIT / total_duration:.2f} iterations/sec")
        print(f"{'-'*70}", flush=True)

    except KeyboardInterrupt:
        print("\nTest stopped by user.", flush=True)

if __name__ == "__main__":
    run_benchmark()