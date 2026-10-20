# Configuration reference

[简体中文](configuration.zh-CN.md) · [Policy selection](scheduling-policies.md)

This reference describes the library in this branch. Load the matching `libvnpu.so` into every participating application process and configure its environment before startup. Changing environment variables does not upgrade an older library binary.

## Resource and coordination settings

| Variable | Default | Accepted value and meaning |
| --- | --- | --- |
| `NPU_GLOBAL_SHM_PATH` | Required for managed workloads | Nonempty file path in a read/write host-shared directory. Time-sliced workloads that contend for the same device must see the same underlying registry file. The library creates the file; do not create or size it manually. Required by manager configuration even with `spatial`. |
| `NPU_LOCAL_SHM_PATH` | `/tmp/vnpu_local_shmem` | Nonempty file path for one accounting/scheduling group of application processes. All participating processes must see the same underlying file. Give independent groups different files. |
| `NPU_MEMORY_LIMIT_MB` | `0` | Non-negative integer, in MiB despite the historical `_MB` name: `1` means `1,048,576` bytes. `0` disables the HBM quota. Values too large to convert to a `u64` byte count are rejected. |
| `NPU_COMPUTE_LIMIT` | `1` | Integer `1`–`100`, inclusive. Relative share with `dynamic`/`overdraft`, target run/rest percentage with `fixed`, cooperative per-process Cube/Vector percentage with `spatial`. Set it explicitly for your workload. |
| `NPU_SCHED_POLICY` | `spatial` | Exactly `dynamic`, `fixed`, `overdraft`, or `spatial`. Surrounding whitespace is trimmed; the core's names are case-sensitive. See [policy tradeoffs](scheduling-policies.md). |

There is one memory quota and one compute setting per local SHM group, not a list of per-device settings. HBM accounting sums allocations across its processes and device counters. A `spatial` compute budget applies independently to each process/device, so adding worker processes does not divide that budget between them.

For example, after preparing a writable shared directory and installing the matching library:

```bash
export NPU_GLOBAL_SHM_PATH=/hami-shared-region/device-7-registry
export NPU_LOCAL_SHM_PATH=/tmp/vnpu-local
export NPU_MEMORY_LIMIT_MB=20480
export NPU_COMPUTE_LIMIT=30
export NPU_SCHED_POLICY=spatial
LD_PRELOAD=/opt/hami/libvnpu.so python3 your_model.py
```

The `7` in this example labels the selected physical card for coordination; the filename does not select a device. Select the appropriate CANN runtime device or context in the application. Use the same global registry only for workloads that should coordinate on that device, and a different local SHM file for each independent accounting group.

## Shared memory and process lifecycle

The supervisor starts lazily on the first intercepted NPU use. Processes sharing local SHM elect an in-process manager; there is no separate manager executable. Manager election is delayed until the hosting process is at least three seconds old, and a worker waits up to ten seconds for initialized local SHM.

All processes sharing a local file must have matching policy, quotas, global registry path, and PID namespace, with readable `/proc/<pid>/stat` entries for each other. The library uses these PIDs for election and dead-process accounting. Configuration consistency is a deployment requirement; the SHM file does not validate a configuration fingerprint. If a group spans isolation boundaries such as containers, arrange both shared file access and the shared PID namespace explicitly.

When upgrading the library or changing policy/configuration, stop every process using the local file and start with a fresh local SHM file. Do not truncate, delete, or recreate SHM while participants still map it. Appended fields preserve monitor offsets, not compatibility between old and new process mappings. The global registry opener waits for at least the current layout size; it does not identify compatible versions or validate a schema. Global layout compatibility is the deployer's responsibility. Replace that registry only after all of its users stop.

Explicit invalid resource settings are reported as `invalid resource configuration` and terminate the intercepted process with status `78`. With `spatial`, inability to initialize required CANN resource limits also terminates with status `78`. For time-based policies, worker initialization failures such as unavailable local SHM can fall back to a no-op worker: a running application alone does not prove that limits are active. Verify the manager, SHM values, and the workload's behavior.

## Advanced scheduling and logging

Leave the timing controls at their defaults unless measuring a particular scheduling workload. They affect time-based policies; they do not change spatial core budgets.

| Variable | Default | Effect and parsing |
| --- | --- | --- |
| `NPU_TOKEN_SCALE` | `100.0` | Floating-point multiplier for admission batch size and the `fixed` rest calculation. Parsed values are floored at `0.1`; an unparsable string falls back to `1.0`, which differs from the unset default. Use finite positive values. |
| `NPU_AVG_ALPHA` | `0.7` | Floating-point weight for updating measured average kernel time. Higher values respond faster to new measurements. Unparsable strings fall back to `0.7`; parsed values are clamped to `0.05`–`0.95`. Use finite values. |
| `RUST_LOG` | `info` | `env_logger` filter, for example `RUST_LOG=limiter=debug,hook=debug`. Spatial initialization/fatal diagnostics also write directly to stderr. |
| `RUST_LOG_STYLE` | `auto` | `env_logger` color style: `auto`, `always`, or `never`. This does not affect direct stderr diagnostics. |

`overdraft` has internal defaults of 10 shared admission tickets, a 5,000 µs blocking threshold per admission call, and 1,000 µs polling. These are not environment settings. A depleted open pool can refill in the same round; 10 is pool capacity, not a cumulative per-round limit or a runtime budget. Enable it with `NPU_SCHED_POLICY=overdraft`.

The virtual HBM free-memory report reserves 128 MiB of overhead. This internal constant is not a configurable variable; the configured quota remains the allocation ceiling.

## Library loading and build paths

| Variable | Scope | Meaning |
| --- | --- | --- |
| `LD_PRELOAD` | Application's Linux dynamic loader | Absolute path to `libvnpu.so`, for example `/opt/hami/libvnpu.so`. Set it when launching every participating application process. |
| `LD_LIBRARY_PATH` | Application/build loader | Normal Linux dependency search path. Make the matching CANN runtime and Ascend driver libraries available; this is a loader setting rather than a quota control. |
| `ASCEND_TOOLKIT_HOME` | Build only | Toolkit root, default `/usr/local/Ascend/ascend-toolkit/latest`; the build adds `<root>/lib64` as a native library search path. |
| `ASCEND_DRIVER_HOME` | Build only | Driver root, default `/usr/local/Ascend/driver`; the build adds `<root>/lib64/driver` and links `dcmi`. |

The library also links the CANN `runtime` library. Build against the intended Linux architecture and install compatible runtime/driver dependencies in the deployment. Setting the two build roots does not install those dependencies or configure the application's runtime library lookup.

## Migration from older variables

The default scheduling policy is now `spatial`; `NPU_COMPUTE_LIMIT` still defaults
to `1`. Deployments that previously relied on an unset policy for Dynamic
scheduling must set `NPU_SCHED_POLICY=dynamic` explicitly to preserve that behavior.
Keep `NPU_GLOBAL_SHM_PATH` configured for every policy.

Remove the old variables completely, even if set to an empty string or alongside the replacement. The current parser rejects their presence.

| Removed variable | Replacement |
| --- | --- |
| `NPU_MEM_QUOTA` | `NPU_MEMORY_LIMIT_MB`, in MiB |
| `NPU_PRIORITY` | `NPU_COMPUTE_LIMIT`, integer `1`–`100` |
| `NPU_FIXED_SHARE_RATIO` | `NPU_SCHED_POLICY=fixed` for the former fixed-share behavior; `dynamic` for relative sharing |
| `NPU_OVERDRAFT_ENABLED` | `NPU_SCHED_POLICY=overdraft` |

`NPU_LOCAL_SHM_NAME` is not read by this library. Use `NPU_LOCAL_SHM_PATH` with a
file path to select the local accounting group; setting only a name does not
override `/tmp/vnpu_local_shmem`.

Sources: [resource parser](../crates/limiter/src/config.rs), [manager tuning](../crates/limiter/src/manager.rs), [supervisor](../crates/limiter/src/supervisor.rs), [worker and HBM accounting](../crates/limiter/src/worker.rs), [logger](../crates/limiter/src/lib.rs), [build paths](../crates/limiter/build.rs).
