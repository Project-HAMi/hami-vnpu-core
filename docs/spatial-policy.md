# Spatial compute policy: design and validation

`NPU_SCHED_POLICY=spatial` selects cooperative CANN resource budgets. It uses the existing `NPU_COMPUTE_LIMIT` percentage and `NPU_MEMORY_LIMIT_MB` memory configuration. No configuration variables, dependencies, or SHM fields are added by this policy.

## Resource model

For each process and device, Cube and Vector budgets are calculated independently:

```text
target = physical_count * percent
budget = ceil(target / 100)
reject if 10 * (100 * budget - target) > target
```

The tolerance is at most 10% relative integer-rounding error for each resource. It is not a measured-utilization or throughput-error guarantee. On 20 Cube / 40 Vector hardware:

| Requested percentage | Cube budget | Vector budget | Result |
| --- | ---: | ---: | --- |
| 30 | 6 | 12 | Exact |
| 60 | 12 | 24 | Exact |
| 33 | 7 | 14 | 6.06% relative rounding error |
| 31 | — | — | Rejected: Cube rounding exceeds 10% |

Budgets do not reserve disjoint physical core IDs. Each process receives its own budget, including multiple processes sharing one Pod's local SHM. HBM accounting remains shared. There is no Pod-wide aggregate compute limiter or priority ordering. Three rounded 33% budgets sum to 21 Cube / 42 Vector on 20 / 40 hardware; CANN still shares the physical device.

## Initialization and lifecycle

The ACL hooks call the real `aclrtSetDevice` or `aclrtSetCurrentContext`, then apply and read back the budgets before the call returns. Operators must initialize and perform resource-aware tiling afterwards. Intercepted allocation/submission entry points also check initialization, but that fallback cannot change a kernel that was already tiled.

The process/device cache retains original resource counts separately from the applied state, so a reference-counted device reset does not scale an already reduced count again. ACL resource resets restore the configured budgets. Later ACL device/stream setters are clamped to the cap; lower values are permitted. Required API, representation, application, and readback failures terminate with status 78.

The implementation uses PID-local bookkeeping and a thread-local recursion guard. It refuses device use in a fork child after parent device initialization, including inherited callbacks. Spawn fresh workers and select the device before operator initialization. Non-default-stream overrides after multiple device initializations are refused because supported ACL APIs do not provide a general stream-to-device ownership query.

Spatial workers bypass time admission, NPU timing events, overdraft tickets, and global rotation. Their local manager does not register a global compute slot or modify another policy's turn. It retains local heartbeat, process reaping, DCMI discovery, and the existing HBM accounting. Retaining that code does not resolve its inherited correctness issues below.

## Compute-focused validation

These measurements use the implementation at `99ccd065cdd4a6958316d5497f149f76c3d06e0e`, library SHA-256 `fbecc746beb1afb82bbaca8a850d405ec141273bf90c5eeeee61c4e0328a9388`. The pinned image is `sha256:d07f64804fe607058318139c929ee5acb6ff3ffa98ae582b425cfeda25c86ea2`, with Torch 2.10.0+cpu / torch_npu 2.10.0.post4 and CANN 9.1. The device is one Ascend 910B3, physical 20 Cube / 40 Vector.

Each case uses fresh processes, identical seeded BF16 inputs, preallocated output, 50 warmup calls per size, and four unscored pilot calls to calibrate approximately 50 ms batches. It runs one unprofiled 20-second formal window per size. Dual workers share a future monotonic start/deadline. Only complete batches inside that fixed window contribute to the primary rate; the crossing batch is retained as an upper bound. No replacement window or prorated FLOPs are used. A multiply-add counts as two operations, giving `2*N^3` FLOPs per square GEMM.

Only the existing six NPU resource/visibility variables are manually supplied. No framework, OMP/Torch thread, affinity, clock, or sysctl tuning is introduced. Outside the measured interval, a CPU FP32 reference using the full K dimension checks the output's top-left 16×16 elements. All sample correctness checks passed; this is not an exhaustive output check.

| Execution | GEMM M=N=K | 30% TFLOP/s | 60% TFLOP/s | 60% / 30% |
| --- | ---: | ---: | ---: | ---: |
| Separate solo runs | 4096 | 83.12 | 165.67 | 1.993× |
| Separate solo runs | 8192 | 82.68 | 160.09 | 1.936× |
| Concurrent 30% + 60% | 4096 | 75.52 | 151.07 | 2.001× |
| Concurrent 30% + 60% | 8192 | 69.49 | 138.92 | 1.999× |

Measured concurrent throughput is 8.81–9.15% lower for 4096 and 13.22–15.96% lower for 8192 than the corresponding sequential solo measurements. Aggregate concurrent rates are 226.59 and 208.41 TFLOP/s. This experiment does not isolate bandwidth, cache, scheduling, or clock effects. These differences are not overhead relative to native: no unlimited native baseline is included.

For every worker and size, crossing-batch FLOPs are less than 0.32% of complete-batch FLOPs; dual complete-batch intervals overlap for 99.61% of the common window. This coverage describes the measurement, not device utilization. Neither boundary accounting nor using Event spans instead of drained wall time removes the observed scaling or parallel-run decrease. Event spans can include stream idle time and are not automatically kernel-only time.

After all formal workers exit, separate fresh solo processes capture NPU-only diagnostics: 20 MatMuls per size and quota, with framework shapes/stack/memory instrumentation disabled. Parsing happens only after compute stops, on copied raw data in network-disabled containers exposing the management device without compute devices or preload.

| Quota | GEMM M=N=K | Mean MatMul device time | Cube blocks, all 20 tasks | Own-process compute gap |
| --- | ---: | ---: | ---: | ---: |
| 30% | 4096 | 1.647434 ms | 6 | 0.001335% |
| 30% | 8192 | 13.281001 ms | 6 | 0.000196% |
| 60% | 4096 | 0.828470 ms | 12 | 0.003138% |
| 60% | 8192 | 6.710022 ms | 12 | 0.000343% |

All 80 exported BF16 MatMulV3 tasks have unique hardware-task/launch connections; each quota has exactly 20 calls per size. Device and stream readbacks are 6 Cube / 12 Vector at 30% and 12 Cube / 24 Vector at 60%. The gap is the interval without an exported compute kernel of this process, within each size's first-start to last-end span. It is not whole-card idle time. These solo profiles do not directly measure overlap or explain interference between concurrent processes, and their timings never replace the formal score.

The results support near-proportional Cube scaling for these cooperating MatMuls with negligible submission gaps. Small-batch eager Qwen decode can remain limited by its Host execution/submission path, so similar token rates at 30% and 60% do not establish equal compute budgets. The GEMM results do not supersede the separate Qwen throughput, tail-latency, or native-relative acceptance results reported in the PR.

The [structured benchmark summary](benchmarks/spatial-gemm-20261016.json) records exact rates and source/evidence hashes. Run ID: `c803dee631ca`. The retained raw archive has 423 regular members, including 422 individually verified data files; archive SHA-256: `86ab9602d397636d482370bff49a5ceb5808306d975efa9999d358ba85cc371c`. Independent integer/Fraction accounting, all 80 trace connections, and 13 protocol/integrity checks passed. Compute/parse containers were removed and deployed library hashes stayed unchanged.

## Review findings and deployment boundaries

The following findings describe the measured implementation; the documentation update does not change its runtime code.

| Priority | Finding | Scope and consequence |
| --- | --- | --- |
| P1 | In-flight HBM reservations can be erased by reconciliation. | Inherited from the base implementation. `check_memory_quota` reserves against `memory_used`, while `recalculate_usage` reconciles it against allocations already published in process slots. Before the first reservation is published, a second check can discard it. A deterministic CPU harness interleaving accepts two 80-byte reservations against a 100-byte quota. This is a hard-memory-limit correctness issue, not a Cube-budget regression. |
| P2 | Direct context creation is not an initialization hook. | `aclrtCreateContext` can select a current context, but only explicit `aclrtSetDevice` / `aclrtSetCurrentContext` are intercepted for pre-tiling application. A create-context-only path can reach tiling before the fallback. Explicitly select the device/context before operator initialization. This coverage risk is established by hook inventory and documented CANN semantics; no additional hardware reproduction is claimed. |
| P2 | Invalid device requests can terminate instead of returning the native parameter error. | Spatial resource setters/resetters initialize the budget first; an out-of-range device ID reaches the fatal check before the original setter. Original-error preservation applies to successfully initialized valid devices, not every malformed request. |
| P2 | A stale manager can resume after takeover without relinquishing ownership. | Inherited supervisor/manager lifecycle behavior. A live process paused beyond the heartbeat timeout can be replaced; its old manager and housekeeping loops do not check current ownership on resuming. Multiple housekeeping managers can remain. This review identifies the source path; it does not claim a new hardware reproduction. |

Code references: [HBM reservation](../crates/limiter/src/worker.rs#L444), [HBM reconciliation](../crates/limiter/src/worker.rs#L589), [ACL hook coverage](../crates/hook/src/hook.rs#L111), [device validation](../crates/hook/src/spatial.rs#L137), [resource setter](../crates/hook/src/spatial.rs#L218), [manager takeover](../crates/limiter/src/supervisor.rs#L94), and [spatial manager loop](../crates/limiter/src/manager.rs#L259). CANN's [context programming model](https://www.hiascend.com/doc_center/source/en/CANNCommunityEdition/900/programug/acldevg/runtime_doc_dev_0001.html) documents context creation and thread association.

Processes sharing local SHM must share one PID namespace as well as policy/configuration: manager election and process slots use numeric PIDs. The hardware tests use the host PID namespace and do not qualify arbitrary containers with colliding PIDs. Use a fresh local SHM file when switching policies or upgrading its layout.

Operators must honor CANN resource budgets. Arbitrary Vector/custom kernels, graph execution, direct lower-level resource resets, and cross-device stream ownership remain unqualified. Do not reduce arbitrary launch dimensions: this can skip work. This policy does not establish exclusive core partitions, strict utilization percentages, priority fairness, or production isolation. The inherited memory/manager issues require separate fixes before making those production claims; one formal GEMM round cannot establish production readiness or the original ≤5% native-relative throughput-loss target.
