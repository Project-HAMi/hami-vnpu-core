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
- **NPU_GLOBAL_SHM_PATH** (required): Define a unique filename within the **shared region**.
> Note: You do NOT need to create this file manually; the manager (embedded in `libvnpu.so`) handles file creation and initialization. However, the path must be identical across all Pods to allow coordination.

- **NPU_MEMORY_LIMIT_MB**: Total memory limit for the Pod, in MB (1 MB = 1024 * 1024 bytes). The default is `0`, which disables the memory limit.

- **NPU_COMPUTE_LIMIT**: Integer from `1` to `100`, default `1`. Its interpretation depends on `NPU_SCHED_POLICY`.

- **NPU_SCHED_POLICY**: `dynamic` (default), `fixed`, or `overdraft`. With `dynamic`, the compute limit is a relative scheduling share and idle capacity can be borrowed; it is not a strict percentage ceiling. With `fixed`, the compute limit is the target percentage used to control the run/rest ratio. Actual limiting accuracy depends on the scheduling mechanism. `overdraft` retains Dynamic scheduling and adds the bounded IDLE borrowing described below.

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

#### Step 3. Launching the AI App:
Just launch the AI application with the `LD_PRELOAD` environment variable pointing to the `libvnpu.so` library. The manager starts automatically inside the library (there is no separate daemon to launch), and the app's NPU calls are routed through it.
```
LD_PRELOAD=./target/debug/libvnpu.so python3 your_model.py
```

### DCMI query configuration

The manager locates `vnpu-dcmi` next to the loaded interceptor. Set
`NPU_DCMI_HELPER_PATH` to an absolute executable path when installing it elsewhere.
Relative helper and library paths are resolved against the working directory at
library load time, so later application `chdir` calls do not break queries.
`ASCEND_DRIVER_HOME` selects the driver installation (default:
`/usr/local/Ascend/driver`). The helper loads `lib64/driver/libdcmi.so` beneath it.
The application's usual CANN library search path must make `libruntime.so`
available when actual NPU calls begin.

Queries run every five seconds. The helper supervises a separate query worker with a 1.5-second deadline, and the manager bounds its own wait at two seconds. The helper acts as a subreaper and remains outside vendor code, so it can terminate and reap driver descendants without relying on container PID 1. A failed or missing
helper produces a warning and leaves hook-based memory accounting in place.
Cancellation requests orderly helper cleanup. A helper is reaped before another query is started; if a driver task remains in uninterruptible I/O, its supervisor stays alive to reap it later and no further queries are spawned. `NPU_DCMI_HELPER_PROCESS`
is an internal launch marker; do not set it on an inference application.

### Scheduler progress and upgrades

`NPU_LOCAL_SHM_PATH` and `NPU_GLOBAL_SHM_PATH` must refer to different files,
including when using symlinks or hard links. Invalid shared-memory paths terminate
startup with exit code 78 before either mapping is opened.

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
Device-plugin process-memory offsets remain unchanged; previous padding carries
manager generations. The local mapping includes the appended overdraft fields.
The fenced registry protocol is version 3; version-2 participants must also stop
and recreate their local/global files before upgrading.

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
