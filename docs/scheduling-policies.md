# Choosing a scheduling policy

[中文](scheduling-policies.zh-CN.md) · [Configuration reference](configuration.md)

Set `NPU_SCHED_POLICY` to `dynamic`, `fixed`, `overdraft`, or `spatial`. All four policies retain shared HBM accounting; they differ in how they control compute. `NPU_COMPUTE_LIMIT` must be an integer from `1` to `100`, but its meaning depends on the policy.

The core library defaults to `spatial` and `NPU_COMPUTE_LIMIT=1`. Set the compute limit explicitly for your workload, and select a time-based policy explicitly when required. The core reads these settings directly from each participating application's environment. Deployments upgrading from the previous Dynamic default should set `NPU_SCHED_POLICY=dynamic` to preserve that behavior. `NPU_GLOBAL_SHM_PATH` remains required for manager configuration under every policy.

## Compare the four policies

| Policy | Meaning of `NPU_COMPUTE_LIMIT` | Benefit | Tradeoff | Suitable starting point |
| --- | --- | --- | --- | --- |
| `dynamic` | Relative share in time-based scheduling | Adapts to competing workloads and allows an active workload to use otherwise idle capacity | Admission waits introduce pauses; the value is not an absolute percentage ceiling | Shared workloads where utilization matters more than a fixed ceiling |
| `fixed` | Target percentage for the run/rest ratio | Retains intentional rest periods even when other workloads are absent | Leaves capacity unused during rests; admission and timing granularity affect accuracy and latency | Workloads needing a run/rest target and accepting scheduled pauses |
| `overdraft` | Dynamic share plus extra admissions while this group is `IDLE` | Lets waiting submissions make progress between normal turns | Borrowed work can overlap other groups; compute shares become less predictable | Bursty or submission-sensitive workloads that accept elastic interference |
| `spatial` | Percentage used to calculate per-process Cube and Vector core budgets | Cooperating operators can run concurrently without the library's time-admission waits | Operators must honor CANN resource budgets; limits are per process and are not exclusive core partitions | Resource-aware operators where concurrent execution is preferable to time slicing |

These controls do not guarantee a particular tokens/s rate, utilization reading, latency, or priority order. Kernel mix, memory bandwidth, host submission, operator implementation, and other device users affect those results. Compare the policies with the actual application and its expected concurrency.

## `dynamic`: relative time shares

Participating local SHM groups share a global scheduling rotation. Within its turn, each group receives a shared pool of kernel-admission tickets. The manager uses the configured share and observed kernel timing to size that pool. Processes in one local SHM group consume the same normal ticket pool.

For three continuously active groups configured as `10`, `30`, and `60`, those values request relative scheduling shares of `1:3:6`. They do not promise that each group stays below 10%, 30%, or 60% of the device at every moment. A lone active group may use more than its configured fraction. The numbers need not sum to `100`; they must each remain in `1..100`.

Changing all weights by the same factor preserves the intended proportions but also changes admission batch sizes, so it need not produce identical scheduling behavior. Choose one consistent scale for comparable workloads.

Time-based policies gate intercepted submissions; they do not interrupt an already running kernel. Long kernels and heterogeneous submission paths can therefore affect sharing accuracy and the time another group waits.

## `fixed`: run/rest targets

`fixed` uses the same time-admission mechanism and adds a rest interval derived from the requested percentage. At `30`, the intended run/rest proportion is approximately `30:70`; at `100`, there is no added fixed-policy rest interval. The manager still measures batches and participates in the global rotation.

This policy keeps its intentional rest behavior when no competing group is active. It is useful when borrowing all idle capacity would defeat the purpose of the limit. The setting is a scheduling target, rather than a hardware utilization meter or a precise throughput cap. Kernel durations, timing estimates, and competing turns affect observed results.

## `overdraft`: elastic progress between turns

`overdraft` keeps the normal Dynamic rotation and adds shared admission tickets while its own group is in `IDLE`. Here, `IDLE` is a scheduler state: it does not mean the whole NPU is unused. Borrowing can therefore allow this group to execute concurrently with another group's normal turn.

The internal defaults are a pool capacity of `10` tickets, a `5000` µs blocking threshold per admission call, and a `1000` µs timed-wait polling interval. Blocking time includes the call's waits before it reaches `IDLE`. A successful borrowed admission returns immediately; a later admission call starts a new waiting interval. These values are internal policy parameters, not environment variables.

An empty, open pool can be refilled after the waiting call reaches its threshold, including repeatedly in one scheduling round. The capacity of `10` limits tickets available in the pool at one time. It does not cap total borrowed admissions per round, kernel runtime, or outstanding work. This policy is appropriate when extra progress is worth less predictable isolation; choose `fixed` when intentional non-borrowing periods are required.

## `spatial`: cooperative core budgets

For each process and device, the library calculates Cube and Vector budgets independently:

```text
budget = ceil(physical_core_count * NPU_COMPUTE_LIMIT / 100)
```

Every valid percentage is rounded upward; no rounding-error threshold is applied. For a device with 20 Cube and 40 Vector cores:

| Setting | Cube budget | Vector budget |
| --- | ---: | ---: |
| `1` | 1 | 1 |
| `10` | 2 | 4 |
| `30` | 6 | 12 |
| `31` | 7 | 13 |
| `33` | 7 | 14 |
| `60` | 12 | 24 |
| `100` | 20 | 40 |

The library applies and reads back these budgets through CANN device resource APIs after successful `aclrtSetDevice` or `aclrtSetCurrentContext` calls. Select the device or context before initializing operators and computing their tiling. `aclrtCreateContext` alone does not apply the budget. A later allocation/submission check cannot change a kernel that was already tiled.

Spatial workers bypass normal admission tickets, time measurement, overdraft, and global compute rotation. The local manager still maintains shared HBM accounting and process housekeeping.

The budget has these practical consequences:

- Each process gets the configured budget on each initialized device. Two worker processes in one group at `30` each get 6 Cube / 12 Vector on the example hardware; they do not divide one 6/12 budget.
- Core IDs are not reserved exclusively. Three processes at `33` have summed budgets of 21 Cube / 42 Vector on 20/40 hardware; CANN shares the actual resources. Rounding does not cause the request to be rejected.
- Operators must query and honor the resource budgets. A custom operator that tiles from the full hardware count may not be constrained. Confirm important operators with device traces, rather than relying only on successful budget readback.
- Use spawned or freshly executed worker processes after device initialization. NPU use in a fork child after the parent begins device initialization is refused.
- The CANN runtime must provide the intercepted device and stream resource APIs. Missing APIs, invalid initial resource counts, or budget application/readback failures terminate the process with exit status `78`. Non-default-stream resource overrides after multiple device initializations are refused because stream ownership cannot be verified safely.

Graph execution, cross-device stream use, and direct lower-level resource changes need separate workload validation. For the API contract and implementation details, see the [spatial design](spatial-policy.md).

## Scope: process, group, and device

For the core library, a shared `NPU_LOCAL_SHM_PATH` identifies one resource-accounting group. All participating processes must load the library, use matching resource settings and policy, and share one PID namespace with access to each other's `/proc/<pid>/stat`. The group can be a single application or related worker processes; its boundaries are determined by these shared resources, not by a deployment label. Source code also calls this group a Pod.

| Resource | `dynamic`, `fixed`, `overdraft` | `spatial` |
| --- | --- | --- |
| Normal compute allowance | Shared admission pool across the local SHM group's processes | Separate budget for each process and device |
| Extra borrowing | Only `overdraft` adds IDLE tickets | None; execution follows CANN resource budgets |
| HBM limit | Shared across the local SHM group's processes and devices | The same shared HBM limit |

`NPU_MEMORY_LIMIT_MB=20480` means a total 20 GiB HBM allowance for the group, not 20 GiB for each process or each selected card. Memory control is independent of the compute policy. A compute setting of `100` retains memory control and the selected policy's hooks; it does not remove the library.

Time-scheduled workloads coordinate only with participants sharing their global registry. Spatial workloads and applications without the hooks do not join that time rotation. Co-locating them requires capacity planning; one time-scheduled group's percentage does not constrain its neighbors.

## Configure and change a policy

After preparing the shared files and library, configure one participating workload explicitly:

```bash
export NPU_GLOBAL_SHM_PATH=/hami-shared-region/device-7-registry
export NPU_LOCAL_SHM_PATH=/tmp/vnpu-local
export NPU_MEMORY_LIMIT_MB=20480
export NPU_COMPUTE_LIMIT=30
export NPU_SCHED_POLICY=spatial
LD_PRELOAD=/opt/hami/libvnpu.so python3 your_model.py
```

The paths are examples: the global path must resolve to the same shared file for workloads that coordinate, while the local file must be shared only by this group's participating processes. The registry filename does not select the application's device. See [configuration](configuration.md) for defaults, units, logging, and migration from older variable names.

Use `NPU_SCHED_POLICY=dynamic`, `fixed`, or `overdraft` to select another policy. For a three-workload comparison, use three separate local SHM groups and set their compute values to `33/33/33` or `10/30/60`. For Dynamic and Overdraft these are relative shares; for Fixed they are individual run/rest targets; for Spatial they produce independently rounded per-process core budgets.

Configuration is read during process/manager initialization, not watched for live changes. To change policies, percentages, or shared-memory layouts, stop all processes sharing the local SHM file, deploy matching library versions and settings, and restart them using a fresh local SHM file. Do not mix old and new processes or remove an active SHM file. Keep the shared global registry in place while other coordinated workloads use it; an upgrade changing its layout requires a coordinated stop of all its users.
