# HAMi-vnpu-core

[English](README.md) | [简体中文](README.zh-CN.md)

HAMi-vnpu-core is an in-container resource controller for Ascend NPUs. Its Rust
library, `libvnpu.so`, intercepts runtime calls to account for shared HBM usage and
apply one of four compute scheduling policies. A manager is elected inside the
participating processes; no separate daemon is required.

## Choose a scheduling policy

| Policy | How it controls compute | When to consider it |
| --- | --- | --- |
| `dynamic` | Relative shares with idle-capacity borrowing | Shared workloads where utilization matters more than a strict ceiling |
| `fixed` | A configured run/rest ratio | Workloads that need a target time share |
| `overdraft` | Dynamic scheduling plus bounded admission borrowing while IDLE | Workloads that benefit from reducing admission stalls |
| `spatial` | Cooperative per-process Cube/Vector budgets | Operators that honor CANN resource limits and benefit from concurrent execution |

The core defaults to `dynamic` with `NPU_COMPUTE_LIMIT=1`. Set both explicitly.
The same number has different meanings across policies; it is not a universal
throughput percentage. See the [policy guide](docs/scheduling-policies.md) for
advantages, tradeoffs and examples.

## Build

Build on Linux with Rust 1.94 (the CI toolchain), a C compiler, and the Ascend
driver/CANN development libraries installed. Initialize the CANN environment
supplied with your installation. Hardware validation in this project has used
Ascend 910B.

```sh
cargo build --release --locked
```

Deploy `target/release/libvnpu.so`. Its architecture and runtime dependencies must
match the workload container. For nonstandard SDK paths, see the
[configuration reference](docs/configuration.md).

## Run an application

First prepare an NPU-enabled Linux container with the Ascend devices and runtime
libraries available. Mount a writable host directory at `/hami-shared-region` in
each participating container on the node, and mount the built library at
`/usr/local/lib/libvnpu.so`.

Inside the workload container:

```sh
# Shared file for workloads coordinating on physical card 7; use a different file for another card.
export NPU_GLOBAL_SHM_PATH=/hami-shared-region/7_global_registry
# Local accounting file: share it only with this group's processes and PID namespace.
export NPU_LOCAL_SHM_PATH=/tmp/vnpu-local
export NPU_MEMORY_LIMIT_MB=10240
export NPU_COMPUTE_LIMIT=30
export NPU_SCHED_POLICY=spatial
LD_PRELOAD=/usr/local/lib/libvnpu.so python3 your_model.py
```

The card number identifies the physical coordination domain; the application
still selects its container-local logical device, commonly `0`.

This sets a shared HBM quota of 10 GiB and, for `spatial`, per-process core budgets
of `ceil(physical cores × 30 / 100)`. On a 20-Cube/40-Vector device, that is 6 Cube
and 12 Vector cores. These budgets do not reserve exclusive physical cores.

Use the same policy, resource settings, library version, local file and PID
namespace for all processes participating in one local group. Independent groups must
use different local backing files. The library creates the SHM files; do not
precreate empty registry files. Before changing configuration or SHM layout,
stop the group's processes and use a fresh local file. Do not truncate or remove
files still mapped by running processes.

For `spatial`, select the device or context before operator initialization and
tiling. Use spawned NPU workers after parent device initialization. See the
[policy guide](docs/scheduling-policies.md) for operator compatibility and
process lifecycle requirements.

## Documentation

| Guide | Contents |
| --- | --- |
| [Scheduling policies](docs/scheduling-policies.md) | Four policies, advantages, tradeoffs and selection examples |
| [Configuration reference](docs/configuration.md) | Environment variables, defaults, units, advanced tuning and migration |
| [Testing and CI](docs/testing.md) | Reproduce the hardware-free checks and understand their coverage |
| [Spatial design](docs/spatial-policy.md) | Detailed implementation and historical validation evidence |

## Development checks

```sh
bash scripts/ci.sh
```

Run on Linux; see [testing and CI](docs/testing.md) for dependencies. CI uses fake
ACL/DCMI backends and source-linked CPU tests. These checks validate configuration,
scheduling logic and hooks; validate NPU execution and workload performance
separately in a compatible Ascend environment.
