# HAMi-vnpu-core —— Hook library for Ascend NPU
## Introduction
HAMi-vnpu-core is the in-container resource controller for Ascend NPU, written in Rust language.


## Features

HAMi-vnpu-core has the following features:
1. Virtualize device meory
2. Limit npu utilization by time shard

## Components
- **Limiter (Manager)**: Each Pod runs a dedicated `limiter` instance. Its primary responsibility is to enforce the **Total Memory Quota** and **Compute Utilization** for all processes within that specific Pod.
- **libvnpu.so (Interceptor)**: A dynamic library (`.so`) that intercepts NPU RTS API calls from AI frameworks to enforce constraints.


## Prerequisites
- **NPU**: Ascend 910B.
- **Shared Region**: A host directory for coordination between pods (e.g., `/tmp/hami-shared-region`).
- **Toolchain**: Docker & Rust installed.

Please follow the instructions on the official Rust website to install rust:
**[https://rust-lang.org/tools/install/](https://rust-lang.org/tools/install/)**

## Build
Use `cargo` to build inside a environment with CANN installed.
```bash
cd hami-vnpu-core
cargo build
# build in release mode: cargo build --release
```

Artifacts Location:
- target/debug/**limiter**: The Per-Pod daemon process binary.
- target/debug/**libvnpu.so**: The Interceptor library.

## Deployment
### Host Environment preparation
Before launching any containers, the **Global Shared Memory (SHM) Region** must be initialized on the host to allow inter-Pod coordination.
Create the Shared Directory:
```
sudo mkdir -p /tmp/hami-shared-region
sudo chmod 777 /tmp/hami-shared-region
```
### Container Deployment
#### Step 1. Start Container
- When starting the container, you must map the following:
SHM Volume: Map the host's shared region (e.g.`/tmp/hami-shared-region`) to a container path (e.g., `/hami-shared-region`).
- Map `limiter` and `libvnpu.so` into container.
- `--privileged` is required for Ascend NPUs to be shared between containers when start docker containers.

#### Step 2. Set Environment Variables:
- **NPU_GLOBAL_SHM_PATH**: Define a unique filename within the **shared region**.
> Note: You do NOT need to create this file manually; the `limiter` handles file creation and initialization. However, the path must be identical across all Pods to allow coordination.

- **NPU_MEM_QUOTA**: Memory limit for the specific Pod (in MB).

- **NPU_PRIORITY**: Set the scheduling priority (e.g., 20).

```bash
export NPU_GLOBAL_SHM_PATH="/hami-shared-region/global_registry"
export NPU_MEM_QUOTA=10240 # 10GB HBM
export NPU_PRIORITY=20 # use half of computing power than another one with priority 40
```

#### Step 3. Launching the Limiter:
Inside each container, the `limiter` process must start first as a background process.
```
./target/debug/limiter > limiter.log 2>&1 &
```

#### Step 4. Launching the AI App:
The AI application must be launched with the `LD_PRELOAD` environment variable pointing to the `libvnpu.so` library. This forces the app to route NPU calls through the local Limiter.
```
LD_PRELOAD=./target/debug/libvnpu.so python3 your_model.py
```

## Testing Dynamic Priority with gRPC

The NPU limiter daemon exposes a Unix Domain Socket (UDS) that allows you to dynamically update the priority of a running container. Because we use `tonic-reflection`, you don't even need the `.proto` files to interact with it.

If you started a container using the provided scripts (e.g. `run_vllm_interactive.sh`, `run_hami_mab.sh`, or `run_hami_mab_multi.sh`), the socket will be exposed to the host in the shared region directory.

### Example Usage

Assuming you launched a container with ID `2` (which creates the socket at `/tmp/hami-shared-region/npu_limiter_2.sock`), you can use `grpcurl` from the host to update the priority to `80.0`:

```bash
grpcurl \
  -plaintext \
  -unix \
  -d '{"priority": 80.0}' \
  /tmp/hami-shared-region/npu_limiter_2.sock \
  npu_limiter.LimiterControl/SetPriority
```

*Note: The limiter daemon now automatically sets the socket permissions to `0o777`, so you do not need `sudo` to run this command from the host.*

## Reporting Compute Utilization via gRPC

In addition to `SetPriority`, the daemon exposes a `GetUtilization` RPC that reports the compute utilization of this container, per NPU device.

### How it works

Each manager thread participates in a global baton-passing scheme: only the container that currently owns the baton is allowed to issue NPU work. A background reporter thread (one per device) wakes on each baton handoff (the same `signal_counter` futex the managers use) and on fixed window deadlines. It records how long our manager held the baton within each window, then computes `busy_us / window_us` at each deadline and keeps a rolling history for averaging. Window boundaries do not shift when baton events are processed.

The reporter only reads shared memory — it does not modify or interfere with the scheduler.

Each device entry also includes **memory** numbers:

- When **`NPU_MEM_QUOTA`** was set at limiter start (`limit_enforced: true`), totals come from the same quota tracker as the hook: `total_mb` is the configured quota (floor MB), `used_mb` is `memory_used` from shared memory (floor MB), and `free_mb` is `total_mb - used_mb`. This matches what the limiter enforces; it is **not** `rtMemGetInfoEx` in that mode.
- When **no quota** was set (`limit_enforced: false`), the daemon uses **`rtMemGetInfoEx`** on that device (after `rtSetDevice` with the visible-device index) so you still get total / used / derived free in MB.

### Environment variables

Read once at daemon startup:

| Variable | Default | Effect |
| --- | --- | --- |
| `NPU_REPORT_INTERVAL_MS` | `1000` | Window length in ms. Set to `0` to disable the reporter thread; `GetUtilization` will then return zero percentages. |
| `NPU_REPORT_HISTORY_SCALE` | `10` | Number of recent windows averaged for `utilization_recent_windows_avg_percent`. Minimum `1`. |

### Example usage

```bash
grpcurl \
  -plaintext \
  -unix \
  -d '{}' \
  /tmp/hami-shared-region/npu_limiter_2.sock \
  npu_limiter.LimiterControl/GetUtilization
```

Example response:

```json
{
  "intervalMs": "1000",
  "historyScale": "10",
  "devices": [
    {
      "deviceId": 0,
      "tracked": true,
      "utilizationLastIntervalPercent": 37.25,
      "utilizationRecentWindowsAvgPercent": 32.88,
      "memory": {
        "limitEnforced": true,
        "totalMb": "10240",
        "usedMb": "1248",
        "freeMb": "8992"
      }
    }
  ]
}
```

### Field meanings

| Field | Meaning |
| --- | --- |
| `interval_ms` | Effective window length (from `NPU_REPORT_INTERVAL_MS`). |
| `history_scale` | Effective rolling-average size (from `NPU_REPORT_HISTORY_SCALE`). |
| `devices[].device_id` | Physical device id (matches entries in `ASCEND_RT_VISIBLE_DEVICES`). |
| `devices[].tracked` | `true` once the reporter has located this device's manager slot. |
| `devices[].utilization_last_interval_percent` | Utilization of the most recently completed window, `0..=100`. |
| `devices[].utilization_recent_windows_avg_percent` | Mean utilization over up to `history_scale` completed windows, `0..=100`. |
| `devices[].memory.limit_enforced` | `true` if `NPU_MEM_QUOTA` was set at daemon start for this pod. |
| `devices[].memory.total_mb` | Quota total in MB (floor) when enforced; otherwise runtime total from `rtMemGetInfoEx`. |
| `devices[].memory.used_mb` | Tracked used memory in MB (floor) when enforced; otherwise `total_mb - free_mb` from runtime. |
| `devices[].memory.free_mb` | `total_mb - used_mb` when enforced; otherwise runtime free in MB (floor). |

Notes:

- Scope is **per container**. Multiple containers publish independently on their own UDS.
- History is in-memory and resets on daemon restart.
- "Busy" here means wall time that this container's manager held the global baton between consecutive handoffs; it does not count time spent waiting in the queue.
- If no work happens for a whole window, the value is `0%` and reports continue to arrive at the fixed cadence.

---