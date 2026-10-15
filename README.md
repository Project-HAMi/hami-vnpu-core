# HAMi-vnpu-core —— Hook library for Ascend NPU
## Introduction
HAMi-vnpu-core is the in-container resource controller for Ascend NPU, written in Rust language.


## Features

HAMi-vnpu-core has the following features:
1. Virtualize device meory
2. Limit npu utilization by time shard

## Components
- **libvnpu.so (Interceptor + Manager)**: A dynamic library (`.so`) that intercepts NPU RTS API calls from AI frameworks to enforce constraints. On the first intercepted NPU call it starts a lightweight **supervisor thread**. The supervisors of all processes in a Pod elect a single **Manager** that enforces the **Total Memory Quota** and **Compute Utilization** for that Pod. If the process hosting the manager exits or crashes, another process in the Pod automatically takes over. No separate manager daemon needs to be started. A short-lived **vnpu-dcmi** supervisor collects hardware memory snapshots in a fresh query process, with a deadline and descendant reaping.


## Prerequisites
- **NPU**: Ascend 910B.
- **Shared Region**: A host directory for coordination between pods (e.g., `/tmp/hami-shared-region`).
- **Toolchain**: Docker & Rust installed.

Please follow the instructions on the official Rust website to install rust:
**[https://rust-lang.org/tools/install/](https://rust-lang.org/tools/install/)**

## Build
Build on Linux with Rust. CANN is required when running NPU applications; building does not load or link the vendor runtime.
```bash
cd hami-vnpu-core
cargo build --workspace
# build in release mode: cargo build --workspace --release
```

Artifacts Location:
- target/debug/**libvnpu.so**: Intercepts NPU calls and hosts the manager.
- target/debug/**vnpu-dcmi**: Isolated DCMI query executable. Install or mount it alongside `libvnpu.so`, with execute permission. The release build puts both files in `target/release`.

The interceptor resolves CANN runtime symbols on first use. This keeps the query executable free of CANN initialization even when `/etc/ld.so.preload` injects the interceptor. The helper does not participate in worker registration or manager election.

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
- Map both `libvnpu.so` and the executable `vnpu-dcmi` into the same container directory.
- `--privileged` is required for Ascend NPUs to be shared between containers when start docker containers.

#### Step 2. Set Environment Variables:
- **NPU_GLOBAL_SHM_PATH**: Define a unique filename within the **shared region**.
> Note: You do NOT need to create this file manually; the manager (embedded in `libvnpu.so`) handles file creation and initialization. However, the path must be identical across all Pods to allow coordination.

- **NPU_MEM_QUOTA**: Memory limit for the specific Pod (in MB).

- **NPU_PRIORITY**: Set the scheduling priority (e.g., 20).

```bash
export NPU_GLOBAL_SHM_PATH="/hami-shared-region/global_registry"
export NPU_MEM_QUOTA=10240 # 10GB HBM
export NPU_PRIORITY=20 # use half of computing power than another one with priority 40
```

#### Step 3. Launching the AI App:
Just launch the AI application with the `LD_PRELOAD` environment variable pointing to the `libvnpu.so` library. The manager starts automatically inside the library (there is no separate daemon to launch), and the app's NPU calls are routed through it.
```
LD_PRELOAD=./target/debug/libvnpu.so python3 your_model.py
```

### DCMI query configuration

The manager locates `vnpu-dcmi` next to the loaded interceptor. Set
`NPU_DCMI_HELPER_PATH` to an absolute executable path when installing it elsewhere.
`ASCEND_DRIVER_HOME` selects the driver installation (default:
`/usr/local/Ascend/driver`). The helper loads `lib64/driver/libdcmi.so` beneath it.
The application's usual CANN library search path must make `libruntime.so`
available when actual NPU calls begin.

Queries run every five seconds. The helper supervises a separate query worker with a 1.5-second deadline, and the manager bounds its own wait at two seconds. The helper acts as a subreaper and remains outside vendor code, so it can terminate and reap driver descendants without relying on container PID 1. A failed or missing
helper produces a warning and leaves hook-based memory accounting in place.
Cancellation requests orderly helper cleanup. A helper is reaped before another query is started; if a driver task remains in uninterruptible I/O, its supervisor stays alive to reap it later and no further queries are spawned. `NPU_DCMI_HELPER_PROCESS`
is an internal launch marker; do not set it on an inference application.

### Scheduler progress and upgrades

Normal scheduling ends a round after five milliseconds without new token
consumption, including when the batch previously made progress. Running budgets
are capped at five seconds, and completion-report waits at 500 milliseconds.
When the cap applies, the common scheduling unit is reduced across priorities;
fixed-share rest periods use that same unit. Idle, incomplete, and timed-out
rounds do not feed the execution-time average. Wall-clock fallbacks report completion without a duration. Hardware-event samples are accepted only when manager observations and reports remain prompt (within five milliseconds); otherwise the previous estimate is retained, since a delayed event endpoint can include idle time.

These are bounds on admission and report waiting, not device-kernel preemption.
Already submitted asynchronous work can complete after admission stops; worker
stream/event synchronization remains in place. Measurements from late reports
are not used as completed samples for a newer round.

Waiting managers maintain their own liveness heartbeat without extending another
manager's global turn. Registry transactions fence manager replacement and slot
reuse. Global rotation scans the bounded slot set, avoiding duplicate queue entries.
Fixed-share rest releases the turn so other containers can run.

**Upgrade all containers sharing a global registry together.** Stop their old
processes, replace the library and install the helper, then recreate their local
and global shared-memory files before starting the new version. Do not remove a
shared-memory file while a process still maps it. An incompatible legacy registry terminates the application with exit code 78, rather than allowing the worker-startup fallback to bypass enforcement or mixing the old queue and new scheduling protocol.
The shared structure sizes and device-plugin process-memory offsets are unchanged;
previous padding now carries manager generations.

### CPU regression tests

On Linux, without CANN or an NPU:

```sh
cargo build --workspace --locked
bash test/dcmi_helper.sh
cargo test --manifest-path test/manager_cpu/Cargo.toml --locked
python3 test/worker_cpu/run.py
cargo test --manifest-path test/memory_cpu/Cargo.toml --locked
bash test/process_local.sh
```

These tests cover scheduler progress, lease fencing, helper deadlines and protocol
validation, plus the existing worker and quota cases. Device ABI, real kernel
completion behavior and throughput require separate Ascend/CANN validation.

For a small real-NPU runtime/DCMI smoke test in a disposable Ascend environment:

```sh
python3 test/ascend_dcmi_smoke.py --artifacts target/release --report /tmp/vnpu-smoke.json
```

This reports whether any admission hook was observed. Eager operators may bypass
that hook surface, so this smoke test alone does not validate compute scheduling.
For that, restart actual graph-serving workloads with the candidate, confirm token
consumption, and exercise requests after idle periods through their serving APIs.
