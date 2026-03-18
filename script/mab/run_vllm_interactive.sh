#!/bin/bash
set -euo pipefail

# Launch vLLM (HAMI/VNPU) in a container with NPU limits and run an
# interactive benchmark client that supports start/pause/resume.
#
# Required arguments:
#   --id <int>                  Unique instance id (also drives ports)
#   --model <path>              Host path to model directory
#   --rtv <list>                ASCEND_RT_VISIBLE_DEVICES list (e.g. 0,1)
#   --tp <int>                  Tensor parallel size for vLLM
#   --mem-quota <int>           NPU_MEM_QUOTA value (MB)
#   --priority <int>            NPU_PRIORITY/share percent
#
# Optional:
#   --host-port <int>           Host port for vLLM (default: 9000+id)
#   --tokens <int>              Max tokens for each run (default: 2000)
#   --label <string>            Label shown in benchmark output
#   --image <string>            Container image id/tag (default set below)
#   --ready-timeout <int>       Seconds to wait for vLLM readiness (default: 45)
#
# Controls inside the benchmark:
#   s/b : start or restart generation
#   p   : pause current generation and show averages
#   q   : quit (also tears down the container)

usage() {
    cat <<EOF
Usage: $0 --id <ID> --model <MODEL_PATH> --rtv <LIST> --tp <N> --mem-quota <MB> --priority <PERCENT> [options]

Required:
  --id <ID>            Unique instance id (also sets default ports)
  --model <PATH>       Host path to model directory
  --rtv <LIST>         ASCEND_RT_VISIBLE_DEVICES list (e.g. 0,1)
  --tp <N>             Tensor parallel size
  --mem-quota <MB>     NPU_MEM_QUOTA value (MB)
  --priority <PERCENT> NPU_PRIORITY / share percent

Optional:
  --host-port <PORT>   Host port for vLLM (default: 9000+ID)
  --tokens <N>         Max tokens per run (default: 2000)
  --label <TEXT>       Label shown in benchmark output
  --image <TAG>        Container image (default: registry-cbu.huawei.com/ascend/vllm-ascend:v0.10.1rc1)
  --ready-timeout <S>  Seconds to wait for vLLM to be ready (default: 45)
  -h|--help            Show this help
EOF
}

# Defaults
IMAGE_ID="registry-cbu.huawei.com/ascend/vllm-ascend:v0.10.1rc1"
HOST_PROJECT_DIR="/mnt/nvme0/mab"
CONTAINER_PROJECT_DIR="/mab"
HOST_LLM_DIR="/mnt/nvme0/LLMs"
CONTAINER_LLM_DIR="/models"
HOST_SHARED_DIR="/tmp/hami-shared-region"
GLOBAL_SHM_PATH="/hami-shared-region/global_registry"
LIMITER_PATH="$CONTAINER_PROJECT_DIR/hami-vnpu-core/target/debug/limiter"
LIBRARY_PATH="$CONTAINER_PROJECT_DIR/hami-vnpu-core/target/debug/libvnpu.so"
CONTAINER_VLLM_PORT=9004
MAX_TOKENS=2000
LABEL="Interactive"
READY_TIMEOUT=300
NPU_TOKEN_SCALE=200.0

# Required values (unset markers)
ID=""
MODEL_PATH=""
RT_VISIBLE_DEVICES=""
TP_SIZE=""
NPU_MEM_QUOTA=""
NPU_PRIORITY=""
HOST_VLLM_PORT=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --id)
            ID="$2"; shift 2;;
        --model)
            MODEL_PATH="$2"; shift 2;;
        --rtv)
            RT_VISIBLE_DEVICES="$2"; shift 2;;
        --tp)
            TP_SIZE="$2"; shift 2;;
        --mem-quota)
            NPU_MEM_QUOTA="$2"; shift 2;;
        --priority)
            NPU_PRIORITY="$2"; shift 2;;
        --host-port)
            HOST_VLLM_PORT="$2"; shift 2;;
        --tokens)
            MAX_TOKENS="$2"; shift 2;;
        --label)
            LABEL="$2"; shift 2;;
        --image)
            IMAGE_ID="$2"; shift 2;;
        --ready-timeout)
            READY_TIMEOUT="$2"; shift 2;;
        -h|--help)
            usage; exit 0;;
        *)
            echo "Unknown argument: $1" >&2
            usage
            exit 1;;
    esac
done

# Validate required args
if [[ -z "$ID" || -z "$MODEL_PATH" || -z "$RT_VISIBLE_DEVICES" || -z "$TP_SIZE" || -z "$NPU_MEM_QUOTA" || -z "$NPU_PRIORITY" ]]; then
    echo "Missing required arguments." >&2
    usage
    exit 1
fi

if [[ -z "$HOST_VLLM_PORT" ]]; then
    HOST_VLLM_PORT=$((9600 + ID))
fi

CONTAINER_NAME="npu-vllm-${ID}"
SERVED_MODEL="vllm-${ID}"

# Paths and ports
MODEL_BASENAME="$(basename "$MODEL_PATH")"

echo ">>> Config"
echo " ID                : $ID"
echo " Model             : $MODEL_PATH"
echo " RTV Devices       : $RT_VISIBLE_DEVICES"
echo " Tensor Parallel   : $TP_SIZE"
echo " NPU Priority      : $NPU_PRIORITY"
echo " NPU Mem Quota     : $NPU_MEM_QUOTA"
echo " Host vLLM Port    : $HOST_VLLM_PORT"
echo " Served Model Name : $SERVED_MODEL"
echo " Container Image   : $IMAGE_ID"
echo " Ready Timeout(s)  : $READY_TIMEOUT"
echo " Shared Dir        : $HOST_SHARED_DIR"
echo " Limiter           : $LIMITER_PATH"
echo " LD_PRELOAD Lib    : $LIBRARY_PATH"
echo

BENCH_SCRIPT=""
cleanup() {
    echo -e "\n>>> Cleaning up container $CONTAINER_NAME..."
    docker kill "$CONTAINER_NAME" > /dev/null 2>&1 || true
    docker rm -f "$CONTAINER_NAME" > /dev/null 2>&1 || true
    rm -f "$BENCH_SCRIPT" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

if [[ ! -d "$HOST_SHARED_DIR" ]]; then
    mkdir -p "$HOST_SHARED_DIR"
fi

DOCKER_COMMAND=$(cat <<EOF
set -e
cd "$CONTAINER_PROJECT_DIR"
source /usr/local/Ascend/ascend-toolkit/set_env.sh

export ASCEND_RT_VISIBLE_DEVICES="$RT_VISIBLE_DEVICES"
export NPU_PRIORITY="$NPU_PRIORITY"
export NPU_MEM_QUOTA="$NPU_MEM_QUOTA"
export NPU_GLOBAL_SHM_PATH="$GLOBAL_SHM_PATH"
export NPU_LOCAL_SHM_NAME="vnpu_local_session_${ID}"
export NPU_TOKEN_SCALE="$NPU_TOKEN_SCALE"

# Limiter log: info=init only, debug=updates (batch ends, MyAvg, anchor, dead leader), trace=all
export RUST_LOG="${RUST_LOG:-limiter=info}"

VLLM_LOG="$CONTAINER_PROJECT_DIR/vllm_${ID}.log"
LIMITER_LOG="$CONTAINER_PROJECT_DIR/limiter_${ID}.log"

echo ">>> Starting limiter..."
"$LIMITER_PATH" > "\$LIMITER_LOG" 2>&1 &
LIMITER_PID=\$!
sleep 2
if ! kill -0 "\$LIMITER_PID" >/dev/null 2>&1; then
    echo "Limiter failed to start. Logs:"
    tail -n 200 "\$LIMITER_LOG" || true
    exit 1
fi

echo ">>> Launching vLLM for $MODEL_BASENAME (tp=$TP_SIZE)"
env ASCEND_RT_VISIBLE_DEVICES="$RT_VISIBLE_DEVICES" \
    NPU_PRIORITY="$NPU_PRIORITY" \
    NPU_MEM_QUOTA="$NPU_MEM_QUOTA" \
    NPU_GLOBAL_SHM_PATH="$GLOBAL_SHM_PATH" \
    ASCEND_SLOG_PRINT_TO_STDOUT=0 \
    ASCEND_GLOBAL_LOG_LEVEL=2 \
    NPU_LOCAL_SHM_NAME="vnpu_local_session_${ID}" \
    NPU_TOKEN_SCALE="$NPU_TOKEN_SCALE" \
    LD_PRELOAD="$LIBRARY_PATH" \
    vllm serve "$CONTAINER_LLM_DIR/$MODEL_BASENAME" \
        --host 0.0.0.0 \
        --port $CONTAINER_VLLM_PORT \
        --tensor-parallel-size $TP_SIZE \
        --seed 1024 \
        --served-model-name "$SERVED_MODEL" \
        --max-num-seqs 16 \
        --max-model-len 2048 \
        --max-num-batched-tokens 4096 \
        --trust-remote-code \
        --no-enable-prefix-caching \
        --gpu-memory-utilization 0.9 2>&1 | tee "\$VLLM_LOG"

kill "\$LIMITER_PID" 2>/dev/null || true
EOF
)

echo ">>> Launching container $CONTAINER_NAME..."
docker run -d \
    --ipc=host \
    --pid=host \
    --privileged=true \
    --name "$CONTAINER_NAME" \
    -p "$HOST_VLLM_PORT:$CONTAINER_VLLM_PORT" \
    -u root \
    --device=/dev/davinci0 --device=/dev/davinci1 --device=/dev/davinci2 --device=/dev/davinci3 \
    --device=/dev/davinci4 --device=/dev/davinci5 --device=/dev/davinci6 --device=/dev/davinci7 \
    --device=/dev/davinci_manager --device=/dev/devmm_svm --device=/dev/hisi_hdc \
    -v /usr/local/bin/npu-smi:/usr/local/bin/npu-smi \
    -v /etc/ascend_install.info:/etc/ascend_install.info \
    -v /usr/local/Ascend/driver/lib64/driver:/usr/local/Ascend/driver/lib64/driver \
    -v /usr/local/Ascend/driver/version.info:/usr/local/Ascend/driver/version.info \
    -v "$HOST_PROJECT_DIR:$CONTAINER_PROJECT_DIR" \
    -v "$HOST_LLM_DIR:$CONTAINER_LLM_DIR" \
    -v "$HOST_SHARED_DIR:/hami-shared-region" \
    -e HOME=/ \
    --entrypoint /bin/bash "$IMAGE_ID" -c "$DOCKER_COMMAND"

echo ">>> Waiting for vLLM to become ready (timeout: ${READY_TIMEOUT}s)..."
start_wait_ts=$(date +%s)
ready=0
while true; do
    # If container died, surface logs and exit
    if ! docker ps --format '{{.Names}}' | grep -q "^${CONTAINER_NAME}\$"; then
        echo ">>> Container exited early. Last logs:"
        docker logs "$CONTAINER_NAME" 2>&1 | tail -n 200 || true
        exit 1
    fi

    # Check HTTP readiness
    status_code=$(curl -s -o /dev/null -w "%{http_code}" "http://localhost:${HOST_VLLM_PORT}/v1/models" || true)
    if [[ "$status_code" == "200" ]]; then
        ready=1
        break
    fi

    now_ts=$(date +%s)
    elapsed=$((now_ts - start_wait_ts))
    if (( elapsed >= READY_TIMEOUT )); then
        echo ">>> Timed out waiting for vLLM readiness after ${elapsed}s"
        echo ">>> Recent container logs:"
        docker logs "$CONTAINER_NAME" 2>&1 | tail -n 200 || true
        exit 1
    fi

    sleep 2
done

echo ">>> vLLM is up (HTTP 200). Starting interactive benchmark..."

export BENCH_URL="http://localhost:${HOST_VLLM_PORT}/v1/completions"
export BENCH_MODEL="$SERVED_MODEL"
export BENCH_TOKENS="$MAX_TOKENS"
export BENCH_LABEL="$LABEL"

# Run Python from a temp file so stdin stays the terminal (heredoc consumes stdin for script source)
BENCH_SCRIPT=$(mktemp)
cat <<'PY' > "$BENCH_SCRIPT"
import json
import os
import queue
import sys
import threading
import time
from collections import deque
from typing import Deque, Optional, Tuple

import requests

LONG_PROMPT = (
    "Write a very long, detailed academic essay about the history of artificial "
    "intelligence, starting from the 1950s to the modern era, covering all major "
    "winter periods and breakthroughs."
)

API_URL = os.environ["BENCH_URL"]
MODEL_NAME = os.environ["BENCH_MODEL"]
MAX_TOKENS = int(os.environ.get("BENCH_TOKENS", "2000"))
LABEL = os.environ.get("BENCH_LABEL", "Interactive")

PRINT_EVERY = 100
WINDOW_LIMIT = 500

cmd_queue = queue.Queue()
EXIT_ALL = False


def cmd_listener() -> None:
    print("\n" + "=" * 60)
    print(" Interactive Benchmark Controls")
    print(" [s/b] START : begin or restart generation")
    print(" [p]   PAUSE : stop current generation and show averages")
    print(" [q]   QUIT  : exit client (container will stop)")
    print("=" * 60 + "\n", flush=True)
    for line in sys.stdin:
        cmd_queue.put(line.strip().lower())


def wait_for_start_or_quit() -> str:
    while True:
        cmd = cmd_queue.get()
        if cmd in ("s", "b", "start"):
            return "start"
        if cmd in ("q", "quit"):
            return "quit"


def poll_cmd() -> Optional[str]:
    try:
        return cmd_queue.get_nowait()
    except queue.Empty:
        return None


def summarize(token_count: int, start_time: float, window: Deque[float]) -> Tuple[float, float, float]:
    now = time.time()
    elapsed = max(now - start_time, 1e-6)
    avg_speed = token_count / elapsed
    last500_speed = 0.0
    if window:
        window_elapsed = max(now - window[0], 1e-6)
        last500_speed = len(window) / window_elapsed
    return elapsed, avg_speed, last500_speed


def run_once() -> str:
    headers = {"Content-Type": "application/json"}
    payload = {
        "model": MODEL_NAME,
        "prompt": LONG_PROMPT,
        "max_tokens": MAX_TOKENS,
        "temperature": 0.0,
        "stream": True,
        "ignore_eos": True,
    }

    print(f"\n{'='*60}")
    print(f" CONNECTING TO : {API_URL}")
    print(f" LABEL         : {LABEL}")
    print(f" TARGET TOKENS : {MAX_TOKENS}")
    print(f"{'='*60}\n")

    try:
        response = requests.post(API_URL, headers=headers, json=payload, stream=True, timeout=10)
        response.raise_for_status()
    except Exception as exc:
        print(f"Connection Failed: {exc}")
        return "error"

    start_time = time.time()
    token_count = 0
    window_tokens = 0
    window_start = start_time
    window = deque(maxlen=WINDOW_LIMIT)

    print(f"{'Total Toks':<12} | {'Inst TPS':<10} | {'Avg Lat(ms)':<12} | {'Elapsed':<9} | {'Last500 TPS':<12}")
    print("-" * 70)

    status: str = "done"
    try:
        for line in response.iter_lines():
            cmd = poll_cmd()
            if cmd in ("p", "pause"):
                status = "paused"
                break
            if cmd in ("q", "quit"):
                status = "quit"
                break

            if not line:
                continue
            decoded = line.decode("utf-8").strip()
            if decoded == "data: [DONE]":
                status = "done"
                break
            if not decoded.startswith("data: "):
                continue

            try:
                _ = json.loads(decoded[6:])
            except Exception:
                continue

            token_count += 1
            now = time.time()
            window.append(now)
            window_tokens += 1

            if token_count % PRINT_EVERY == 0:
                interval = max(now - window_start, 1e-6)
                inst_tps = window_tokens / interval
                avg_lat_ms = (interval / window_tokens) * 1000 if window_tokens else 0.0
                last500_tps = 0.0
                if len(window) > 1:
                    last500_tps = len(window) / max(now - window[0], 1e-6)

                print(
                    f"{token_count:<12} | {inst_tps:<10.2f} | {avg_lat_ms:<12.2f} | "
                    f"{now - start_time:<9.2f}s | {last500_tps:<12.2f}"
                )
                window_tokens = 0
                window_start = now

    except Exception as exc:
        print(f"\n[!] Error while streaming: {exc}")
        status = "error"
    finally:
        response.close()

    elapsed, avg_speed, last500 = summarize(token_count, start_time, window)
    print(f"\n{'='*60}")
    print(f" STATUS        : {status}")
    print(f" Total Tokens  : {token_count}")
    print(f" Elapsed       : {elapsed:.2f}s")
    print(f" Avg Speed     : {avg_speed:.2f} tokens/sec")
    print(f" Last 500 Avg  : {last500:.2f} tokens/sec")
    print(f"{'='*60}\n")

    return status


def main() -> None:
    listener = threading.Thread(target=cmd_listener, daemon=True)
    listener.start()

    global EXIT_ALL
    while not EXIT_ALL:
        next_action = wait_for_start_or_quit()
        if next_action == "quit":
            break
        status = run_once()
        if status == "quit":
            break
        print(">>> Press 's' to start again, 'p' to pause during a run, 'q' to quit.")


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        pass
PY

python3 "$BENCH_SCRIPT"
