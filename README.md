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

The core defaults to `spatial` with `NPU_COMPUTE_LIMIT=1`. Set the compute limit
explicitly for your workload. Select `dynamic`, `fixed`, or `overdraft` explicitly
to use time-based scheduling.
The same number has different meanings across policies; it is not a universal
throughput percentage. See the [policy guide](docs/scheduling-policies.md) for
advantages, tradeoffs and examples.

## Build

The release CI and local builds share [one Dockerfile](dockerfiles/Dockerfile).
Install Git, Make, Docker and Docker Buildx, then run from the repository root:

```sh
make build-in-docker
cd dist && sha256sum -c SHA256SUMS
```

This builds the complete release library, verifies its APIs against real CANN
runtime/DCMI libraries, and exports `dist/libvnpu.so`, `dist/vnpu-dcmi`, `dist/build-info.json` and `dist/SHA256SUMS`.
The Dockerfile uses the official `quay.io/ascend/cann:8.5.2-910b-ubuntu22.04-py3.11`
image, pinned by digest, and Rust **1.94.0**. It downloads the official **HDK 25.5.1**
package, verifies its SHA256 and extracts the real driver user libraries for
API verification; it does not install a kernel driver. The build environment is
**CANN 8.5.2 on Ubuntu 22.04** and supports **Linux AMD64 and ARM64**. The Make
target selects the host architecture by default. Select a target and separate
output directories explicitly when building both:

```sh
make build-in-docker PLATFORM=linux/amd64 OUT_DIR=dist/amd64
make build-in-docker PLATFORM=linux/arm64 OUT_DIR=dist/arm64
```

Use a native builder for each target, or configure Docker Buildx emulation for
the other architecture. No NPU is required to compile.

Deploy `libvnpu.so` and the executable `vnpu-dcmi` together into a matching-architecture workload container with
compatible CANN and host-driver libraries. The HDK driver user-library version describes the
build environment; choose the host driver using the CANN/hardware compatibility
requirements.
A successful build does not establish the minimum supported CANN version or
hardware support for every policy. See [testing and CI](docs/testing.md) for
artifact provenance, verification and hardware-validation scope.

To build directly on a Linux host with Rust 1.94.0 and a C compiler, run
`cargo build --release --workspace --locked`. The library is then at
`target/release/libvnpu.so`, with the helper at `target/release/vnpu-dcmi`. Nonstandard SDK paths are covered in the
[configuration reference](docs/configuration.md).

See the [coordinated scheduler upgrade](docs/scheduler-upgrade.md) before replacing an existing installation.

## Run an application

First prepare an NPU-enabled Linux container with the Ascend devices and runtime
libraries available. Mount a writable host directory at `/hami-shared-region` in
each participating container on the node, and mount the built library at
`/usr/local/lib/libvnpu.so`.

Mount a container-specific file at `/etc/ld.so.preload` with this content:

```text
/usr/local/lib/libvnpu.so
```

Mount this file only into workload containers. The library resolves CANN/runtime
and DCMI on first NPU use, so ordinary commands such as `bash` and `ls` can start
before the CANN environment is prepared. NPU applications still need compatible
runtime/driver libraries. The supervisor and logger also start on first NPU use.

Inside the workload container, launch the framework with its normal command:

```sh
# Shared file for workloads coordinating on physical card 7; use a different file for another card.
export NPU_GLOBAL_SHM_PATH=/hami-shared-region/7_global_registry
# Local accounting file: share it only with this group's processes and PID namespace.
export NPU_LOCAL_SHM_PATH=/tmp/vnpu-local
export NPU_MEMORY_LIMIT_MB=10240
export NPU_COMPUTE_LIMIT=30
export NPU_SCHED_POLICY=spatial # Default policy; shown explicitly for clarity.
python3 your_model.py
```

For local diagnostics, `LD_PRELOAD=/usr/local/lib/libvnpu.so python3 your_model.py`
is also supported; it is not required with the container preload file.

The card number identifies the physical coordination domain; the application
still selects its container-local logical device, commonly `0`.
`NPU_GLOBAL_SHM_PATH` remains required for manager configuration, including
the default spatial policy.

This sets a shared HBM quota of 10 GiB on each card and, for `spatial`, per-process core budgets
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
| [Scheduler upgrade](docs/scheduler-upgrade.md) | Paired helper installation, bounded scheduling and coordinated shared-state migration |
| [Configuration reference](docs/configuration.md) | Environment variables, defaults, units, advanced tuning and migration |
| [Testing and CI](docs/testing.md) | Build release artifacts, reproduce CPU checks and understand their coverage |
| [Spatial design](docs/spatial-policy.md) | Detailed implementation and historical validation evidence |

## Development checks

```sh
cargo fmt --all -- --check
bash scripts/ci.sh
```

Use Rust 1.94.0 with `rustfmt` and `clippy`; run the CPU script on Linux. See
[testing and CI](docs/testing.md) for dependencies. CI checks formatting and
runs Clippy with warnings denied across the root workspace and all its targets.
The CPU regression jobs use fake ACL/DCMI backends and source-linked tests for
configuration, scheduling logic and hooks. Separate release jobs use the real
SDK and upload verified AMD64 and ARM64 libraries. Validate NPU execution and
workload performance separately in a compatible Ascend environment.
