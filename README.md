# HAMi-vnpu-core —— Hook library for Ascend NPU
## Introduction
HAMi-vnpu-core is the in-container resource controller for Ascend NPU, written in Rust language.


## Features

HAMi-vnpu-core has the following features:
1. Virtualize device meory
2. Limit NPU compute through time scheduling or cooperative Cube/Vector budgets

## Components
- **libvnpu.so (Interceptor + Manager)**: A dynamic library (`.so`) that intercepts NPU RTS API calls from AI frameworks. A lightweight **supervisor thread** starts lazily on the first use of an intercepted RTS path. Processes sharing a Pod's local SHM and PID namespace elect a **Manager** for shared HBM accounting and, with time-based policies, compute scheduling. Another participating process can take over after the manager exits or its heartbeat expires. No separate daemon needs to be started.

Compute-manager time scheduling applies to `dynamic`, `fixed`, and `overdraft`. With `spatial`, workers apply per-process core budgets while retaining shared HBM accounting and memory housekeeping; the manager does not schedule their compute turns.


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
- target/debug/**libvnpu.so**: The one and only artifact. It intercepts NPU calls AND hosts the manager (via an internal supervisor thread), so this is all you build and deploy.

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
- Map `libvnpu.so` into the container.
- `--privileged` is required for Ascend NPUs to be shared between containers when start docker containers.

#### Step 2. Set Environment Variables:
- **NPU_GLOBAL_SHM_PATH** (required): Define a unique filename within the **shared region**.
> Note: You do NOT need to create this file manually; the manager (embedded in `libvnpu.so`) handles file creation and initialization. However, the path must be identical across all Pods to allow coordination.

- **NPU_MEMORY_LIMIT_MB**: Total memory limit for the Pod, in MB (1 MB = 1024 * 1024 bytes). The default is `0`, which disables the memory limit.

- **NPU_COMPUTE_LIMIT**: Integer from `1` to `100`, default `1`. Its interpretation depends on `NPU_SCHED_POLICY`.

- **NPU_SCHED_POLICY**: `dynamic` (default), `fixed`, `overdraft`, or `spatial`. With `dynamic`, the compute limit is a relative scheduling share and idle capacity can be borrowed; it is not a strict percentage ceiling. With `fixed`, the compute limit is the target percentage used to control the run/rest ratio. Actual limiting accuracy depends on the scheduling mechanism. `overdraft` retains Dynamic scheduling and adds the bounded IDLE borrowing described below. `spatial` configures cooperative Cube/Vector core budgets and bypasses time scheduling.

The current library no longer accepts `NPU_MEM_QUOTA`, `NPU_PRIORITY`, or `NPU_FIXED_SHARE_RATIO`; setting them reports a migration error. Use the new variables above instead. The `fixed` policy corresponds to the previous fixed-share setting of `true`, while `dynamic` corresponds to `false`. Upgrade an older deployed library before using the new variables; older binaries do not recognize them.

```bash
export NPU_GLOBAL_SHM_PATH="/hami-shared-region/global_registry"
export NPU_MEMORY_LIMIT_MB=10240 # 10 GiB HBM
export NPU_COMPUTE_LIMIT=20 # target 20% run/rest ratio with fixed policy
export NPU_SCHED_POLICY=fixed
```

#### Optional Overdraft Policy

`NPU_SCHED_POLICY=overdraft` builds on the normal Dynamic rotation and adds a bounded shared borrowing pool when a Pod is in `IDLE`. The policy itself enables borrowing. The removed `NPU_OVERDRAFT_ENABLED` variable is rejected with a migration error directing users to `NPU_SCHED_POLICY=overdraft`. This implements the bounded-pool direction discussed in [issue #29](https://github.com/Project-HAMi/hami-vnpu-core/issues/29). Historical ResNet results in that issue are not measurements of this implementation.

The strategy uses internal defaults: a shared pool of `10` admission tickets, a `5000`-microsecond cumulative blocking threshold for one kernel-admission call, and a `1000`-microsecond futex polling interval. These parameters are not exposed as environment variables. Enable the strategy only through `NPU_SCHED_POLICY=overdraft`. Borrowing is permitted only while in `IDLE`; the timer does not restart on entry to `IDLE`.

Blocking time accumulates from entry to each kernel-admission wait, including waits on the RWLock and in `RUNNING` or `MEASURING`; changing state does not reset it. A timed-out worker can fill an empty, open pool, including after it is exhausted again in the same scheduling round. Every borrowed admission consumes one ticket, including the worker that triggers a fill. The pool holds at most `10` tickets; this is not a cumulative per-round admission limit. The `5000`-microsecond threshold applies to each waiting call, not a global refill interval: several workers that have already timed out may trigger successive fills as the pool empties. Closing the pool prevents refills until the manager opens a new round. Borrowed work is allowed only in `IDLE` and does not contribute to normal `active_workers` accounting or normal completion events. Multiple Pods may therefore execute borrowed work concurrently; the policy does not guarantee a strict compute percentage or complete fairness. Tickets count admissions, not kernel runtime or outstanding kernel execution.

The added shared-memory fields are appended after the existing layout so monitoring tools retain the old field offsets. This does not make old process mappings compatible with the extended layout. Before upgrading, stop all old processes in the same Pod and use a fresh `NPU_LOCAL_SHM_PATH` file (for example `/tmp/vnpu_local_shmem_overdraft_v2`). All participating processes in that Pod must use the new mapping; do not mix old and new mappings.

The pool uses a non-wrapping 32-bit round tag. After approximately 4.29 billion rounds on one local shared-memory file, borrowing stays disabled while normal scheduling continues. Stop the participating processes and use a fresh local shared-memory file to restore borrowing.

Example configuration (after preparing the shared directory and the fresh local shared-memory path):

```bash
export NPU_GLOBAL_SHM_PATH="/hami-shared-region/global_registry"
export NPU_MEMORY_LIMIT_MB=10240
export NPU_COMPUTE_LIMIT=20
export NPU_SCHED_POLICY=overdraft
```

#### Optional Spatial Policy

Use `NPU_SCHED_POLICY=spatial` with an explicit `NPU_COMPUTE_LIMIT` to select cooperative CANN core budgets. No additional environment variables are needed. The library sets the budget through intercepted `aclrtSetDevice` or `aclrtSetCurrentContext` calls, before resource-aware operators perform tiling. This policy bypasses normal admission tickets, IDLE borrowing, and global time rotation. The local manager still maintains the Pod's shared HBM accounting and process cleanup.

Cube and Vector budgets are calculated independently as `ceil(physical_core_count * NPU_COMPUTE_LIMIT / 100)`. Every valid percentage in `1..=100` is rounded upward; no rounding-error threshold is applied. On a 20-Cube/40-Vector device, `31` gives 7 Cube and 13 Vector cores, `33` gives 7 Cube and 14 Vector cores, and the default compute setting of `1` gives 1 Cube and 1 Vector core. Configure the percentage explicitly for the intended workload.

These are per-process, per-device budgets for cooperating operators, not exclusive physical partitions. Three processes configured with `33` each have a summed budget of 21 Cube / 42 Vector cores on 20 / 40 hardware; the runtime shares the hardware. Multiple processes in one Pod each receive the configured core budget, while HBM accounting remains shared for the Pod. This policy does not enforce Pod-wide aggregate compute usage, utilization percentages, throughput ratios, or priority ordering.

An operator must honor CANN device/stream resource limits for this mechanism to affect its work. Some custom operators obtain the full hardware count directly. In earlier testing, a Triton RoPE implementation used the hardware Vector count without querying the configured budget. This implementation does not rewrite kernels or reduce arbitrary launch dimensions, which could break their work partitioning. Resource-budget readback and inference throughput alone do not prove that every operator is constrained. Direct lower-level runtime resource resets bypassing the intercepted ACL APIs are not covered. Graph execution and cross-device stream use have not been qualified. NPU use in a fork child after the parent enters device initialization is refused; use spawned worker processes and select their device before operator initialization.

Requested spatial limits fail with exit status `78` if the required CANN APIs are unavailable, the physical resource count is zero, a budget cannot be applied, or its readback differs. After a valid device has been initialized, later application-issued setter failures retain the original CANN error return; invalid or uninitialized device requests can instead fail during budget initialization and terminate the process. Resource reset APIs reapply the configured budgets, and device resets retain the original core counts for the next initialization. When multiple devices have been initialized in one process, setting or resetting the limits of a non-default stream is refused because its device ownership cannot be verified through the supported ACL API. Use a CANN runtime supporting the intercepted device and stream resource APIs. All processes sharing a local SHM file must use the same policy, resource configuration, and PID namespace. Stop the Pod's old processes and use a fresh local SHM file when switching policies.

`aclrtCreateContext` alone is not intercepted. Applications using it directly must explicitly select the device or the newly created context through the intercepted APIs before initializing or tiling operators. Applying a budget at a later runtime submission cannot change work already tiled with the full hardware count.

Historical compute-focused BF16 GEMM validation on a 20-Cube/40-Vector Ascend 910B3 measured 60% at **1.94–1.99 times** the throughput of 30% when run separately, and approximately **2 times** when both run concurrently. Separate device traces confirmed 6/12 Cube blocks and less than 0.004% own-process kernel gaps. Removing the rounding-error gate leaves these 30% and 60% budgets unchanged. The results describe the tested MatMuls; generic operator coverage and native-relative overhead require their own measurements. See [spatial design and benchmark results](docs/spatial-policy.md).

```bash
export NPU_GLOBAL_SHM_PATH="/hami-shared-region/global_registry"
export NPU_LOCAL_SHM_PATH="/hami-shared-region/pod-spatial"
export NPU_MEMORY_LIMIT_MB=20480
export NPU_COMPUTE_LIMIT=33
export NPU_SCHED_POLICY=spatial
```

#### Step 3. Launching the AI App:
Just launch the AI application with the `LD_PRELOAD` environment variable pointing to the `libvnpu.so` library. The manager starts automatically inside the library (there is no separate daemon to launch), and the app's NPU calls are routed through it.
```
LD_PRELOAD=./target/debug/libvnpu.so python3 your_model.py
```
