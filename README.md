# HAMi-vnpu-core —— Hook library for Ascend NPU
## Introduction
HAMi-vnpu-core is the in-container resource controller for Ascend NPU, written in Rust language.


## Features

HAMi-vnpu-core has the following features:
1. Virtualize device meory
2. Limit npu utilization by time shard

## Components
- **libvnpu.so (Interceptor + Manager)**: A dynamic library (`.so`) that intercepts NPU RTS API calls from AI frameworks to enforce constraints. On load it also spawns a lightweight **supervisor thread**. The supervisors of all processes in a Pod elect a single **Manager** that enforces the **Total Memory Quota** and **Compute Utilization** for that Pod. If the process hosting the manager exits or crashes, another process in the Pod automatically takes over. No separate daemon needs to be started.


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

- **NPU_SCHED_POLICY**: `dynamic` (default) or `fixed`. With `dynamic`, the compute limit is a relative scheduling share and idle capacity can be borrowed; it is not a strict percentage ceiling. With `fixed`, the compute limit is the target percentage used to control the run/rest ratio. Actual limiting accuracy depends on the scheduling mechanism.

The current library no longer accepts `NPU_MEM_QUOTA`, `NPU_PRIORITY`, or `NPU_FIXED_SHARE_RATIO`; setting them reports a migration error. Use the new variables above instead. The `fixed` policy corresponds to the previous fixed-share setting of `true`, while `dynamic` corresponds to `false`. Upgrade an older deployed library before using the new variables; older binaries do not recognize them.

```bash
export NPU_GLOBAL_SHM_PATH="/hami-shared-region/global_registry"
export NPU_MEMORY_LIMIT_MB=10240 # 10 GiB HBM
export NPU_COMPUTE_LIMIT=20 # target 20% run/rest ratio with fixed policy
export NPU_SCHED_POLICY=fixed
```

#### Step 3. Launching the AI App:
Just launch the AI application with the `LD_PRELOAD` environment variable pointing to the `libvnpu.so` library. The manager starts automatically inside the library (there is no separate daemon to launch), and the app's NPU calls are routed through it.
```
LD_PRELOAD=./target/debug/libvnpu.so python3 your_model.py
```