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

The same scalar settings apply independently to every selected physical card. `NPU_MEMORY_LIMIT_MB=20480` allows **20 GiB on each card**, shared by this group's processes on that card. Usage on card A does not consume card B's allowance. There is no per-device configuration list. Each card also has its own time-scheduling state and tickets. A `spatial` compute budget applies independently to each process/device; adding worker processes does not divide that budget between them.

For example, after preparing a writable shared directory and installing the matching library:

```bash
export NPU_GLOBAL_SHM_PATH=/hami-shared-region/registry
export NPU_LOCAL_SHM_PATH=/tmp/vnpu-local
export NPU_MEMORY_LIMIT_MB=20480
export NPU_COMPUTE_LIMIT=30
export NPU_SCHED_POLICY=spatial
LD_PRELOAD=/opt/hami/libvnpu.so python3 your_model.py
```

Select CANN devices or contexts in the application. These paths are **base names**: the library appends `.device-<physical-id>` to both paths, including single-card workloads. With physical cards 6 and 7, this example creates `/tmp/vnpu-local.device-6` and `/tmp/vnpu-local.device-7`, each with a 20 GiB quota. Logical ordinals are mapped through CANN to stable physical IDs, so processes with different visible-device orderings still account for the same card together. Time-scheduled workloads that contend on the same cards must share one global base path; independent accounting groups need different local base paths.


## Shared memory and process lifecycle

A supervisor for each physical card starts lazily on its first intercepted NPU use. Processes sharing local SHM elect an in-process manager; there is no separate manager executable. Manager election is delayed until the hosting process is at least three seconds old, and a worker waits up to ten seconds for initialized local SHM.

All processes sharing a local file must have matching policy, quotas, global registry path, and PID namespace, with readable `/proc/<pid>/stat` entries for each other. The library uses these PIDs for election and dead-process accounting. Configuration consistency is a deployment requirement; the SHM file does not validate a configuration fingerprint. If a group spans isolation boundaries such as containers, arrange both shared file access and the shared PID namespace explicitly.

When upgrading the library or changing policy/configuration, stop every process using the local file and start with a fresh local SHM file. Do not truncate, delete, or recreate SHM while participants still map it. Appended fields preserve monitor offsets, not compatibility between old and new process mappings. The global registry opener waits for at least the current layout size; it does not identify compatible versions or validate a schema. Global layout compatibility is the deployer's responsibility. Replace that registry only after all of its users stop.

Invalid resource settings, failure to identify the current physical card, and inability to initialize a required per-card limiter terminate the intercepted process with status `78`. Spatial resource application/readback failures also terminate with `78`.

HBM admission includes pending allocations; failed allocations roll back their reservation. Freeing an intercepted allocation updates its original card even after a device switch. The virtual allocation hooks cover `aclrtMalloc`, `aclrtMallocAlign32`, `aclrtMallocCached`, and `aclrtMallocWithCfg` through their runtime entry points. Delegation between these entry points is charged once; CANN validates allocation configuration unchanged.

Accounting follows allocations created by the container's local accounting group. Exporting, importing or mapping an existing allocation does not add another charge; externally created memory imported into the container is not charged. Successful frees through `rtFree`, `rtsFree` or `rtFreePhysical` refund the original allocation record. A failed free retains its charge, and address reuse during a concurrent free cannot remove the newer allocation's record.

DVPP allocations are intercepted at `rtMemAlloc`, `rtDvppMalloc` and `rtDvppMallocWithFlag`. Physical allocations use `rtMallocPhysical` or `rtsMemMallocPhysical`; an explicit target is charged to that card. Retaining a tracked physical handle adds a reference without charging its bytes again. Its original charge remains until the last successful reference release and any in-flight lifetime operations finish. Managed/UVM and driver shared-pool/buffer accounting are outside this change's supported scope.

DCMI snapshots are diagnostic for per-card domains and do not overwrite this ledger. The quota covers intercepted allocations, not all physical HBM attributed to a process by the driver. Runtime/context overhead and allocation paths that bypass the hooks can make driver usage higher. Timing streams/events are separate for each context on each card. A card domain has 32 timing/report slots shared by its contexts and 64 process accounting slots; spatial workers do not use timing slots. A just-retired completed report can retain its slot until the current batch is consumed.

The SHM structures retain their field offsets, but the filenames now identify physical-card domains. Monitoring tools must open the suffixed files; each file's `hbm_used[0]` is that card's counter. Stop old participants and use fresh base names when upgrading; old unsuffixed files do not coordinate with this version.

Explicit stream resource overrides use the recorded owner. Kernel submissions still require the stream's context to be current, as required by CANN. Select that context before submitting work. Device reset invalidates tracked stream ownership; recreate streams after selecting the device/context again.

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
| `LD_PRELOAD` | Application's Linux dynamic loader | Optional alternative to a container-specific `/etc/ld.so.preload` file. Use the absolute path to `libvnpu.so`, for example `/opt/hami/libvnpu.so`. |
| `LD_LIBRARY_PATH` | Application/build loader | Normal Linux dependency search path. Make the matching CANN runtime and Ascend driver libraries available; this is a loader setting rather than a quota control. |
| `ASCEND_TOOLKIT_HOME` | Release verification | Toolkit root, default `/usr/local/Ascend/ascend-toolkit/latest`; verification inspects `<root>/lib64`. |
| `ASCEND_DRIVER_HOME` | Release verification | Driver root, default `/usr/local/Ascend/driver`; verification inspects `<root>/lib64/driver`. |

`libvnpu.so` has no link-time CANN/runtime/DCMI dependency. It looks up SDK symbols only when used, including SDKs loaded with `RTLD_LOCAL`; missing optional symbols are retried after a later `dlopen`. Without resource configuration, preloading stays passive and runtime hooks pass through. Explicit resource settings still require valid manager configuration.

Mount a container-specific `/etc/ld.so.preload` containing the absolute library path to inject it into child processes without changing framework commands. Keep this mount within workload containers. Prepare compatible CANN/driver libraries and their search paths before launching NPU applications. Setting the two verification roots does not install dependencies or configure runtime lookup.

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

Sources: [resource parser](../crates/limiter/src/config.rs), [manager tuning](../crates/limiter/src/manager.rs), [supervisor](../crates/limiter/src/supervisor.rs), [worker and HBM accounting](../crates/limiter/src/worker.rs), [logger](../crates/limiter/src/supervisor.rs), [lazy loading](../crates/limiter/src/dynamic.rs).
