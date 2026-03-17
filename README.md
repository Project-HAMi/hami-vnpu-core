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