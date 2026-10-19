# Testing and CI / 测试与 CI

## English

### Build the release library

The [release job](../.github/workflows/ci.yml) builds the complete Rust workspace
with real CANN runtime and DCMI libraries. It uses the same
[Dockerfile](../dockerfiles/Dockerfile) and [Make target](../Makefile) as local
builds, following the container-build approach used by
[HAMi-core](https://github.com/Project-HAMi/HAMi-core/blob/main/.github/workflows/build-src.yml).

Install Git, Make, Docker and Docker Buildx, then run from the repository root:

```sh
make build-in-docker
cd dist && sha256sum -c SHA256SUMS
```

Use a native Linux ARM64 builder for this image. Docker Buildx can also use ARM64
emulation if configured on the host. The build requires network access to pull
the SDK image and install the toolchain; it does not require an NPU, a driver
kernel module, cluster credentials or repository secrets.

| Build input | Pinned value |
| --- | --- |
| Official SDK image | `quay.io/ascend/cann:8.5.2-910b-ubuntu22.04-py3.11` |
| Image digest | `sha256:093e4003c81ef9795a0c1019f6df46e0823ef38ba15ab68c5ed2b236b4a1d64b` |
| CANN | `8.5.2` |
| SDK image system and target architecture | Ubuntu 22.04, Linux ARM64 |
| Official HDK driver user libraries | `25.5.1` |
| HDK package SHA256 | `1573563e63cb893cf3049ae31259eb8d6c31ec859be82c3c4a9c5846eea372b5` |
| Rust | `1.94.0` |
| Rust dependencies | Repository `Cargo.lock`, enforced with `--locked` |

The image digest identifies the exact SDK image even if its tag changes. The
[official CANN image source](https://github.com/Ascend/cann-container-image/blob/main/cann/8.5.2-910b-ubuntu22.04-py3.11/Dockerfile)
documents its contents. The build downloads the
[official HDK 25.5.1 ARM64 package](https://ascend-repo.obs.cn-east-2.myhuaweicloud.com/Ascend%20HDK/Ascend%20HDK%2025.5.1/Ascend-hdk-910b-npu-driver_25.5.1_linux-aarch64.run),
checks the package hash, and extracts only its user libraries with `--noexec`.
It does not install a kernel driver. HDK 25.5.x is listed in the
[official CANN 8.5.2 compatibility table](https://www.hiascend.com/document/detail/zh/canncommercial/900/releasenote/release-notes.md).

The build uses real CANN libraries under `/usr/local/Ascend/cann-8.5.2/lib64`
and DCMI under `/usr/local/Ascend/driver/lib64/driver`, with driver dependencies
under `lib64/driver` and `lib64/common`. Release verification checks that the
selected real libraries are resolved. The driver version above describes the
user libraries present at build time; choose the deployed host driver according
to the workload's CANN and hardware compatibility requirements. Set `OUT_DIR`
to change the export location, for example `make build-in-docker OUT_DIR=out`.

The exported directory contains:

| File | Purpose |
| --- | --- |
| `dist/libvnpu.so` | Release ARM64 shared library for deployment into a compatible workload container. |
| `dist/build-info.json` | Source commit, Rust/Cargo versions, pinned SDK image, CANN version metadata, SDK library hashes, ELF dependencies, hook exports and SDK API availability. |
| `dist/SHA256SUMS` | Checksums for the library and build metadata. |

Before export, [release verification](../scripts/ci/verify-release.py) checks the
ELF architecture and shared-object type, required hook and SDK exports, and
`ldd -r` resolution against the selected real runtime/DCMI libraries. It records
whether the CANN libraries provide the spatial-policy APIs. These checks do not
initialize CANN or DCMI and do not execute NPU kernels.

CI runs this build on native `ubuntu-24.04-arm` and uploads the three files as the
`libvnpu-cann8.5.2-arm64` artifact. The build container remains Ubuntu 22.04. Deploy
the resulting library with compatible ARM64 system, CANN and driver libraries;
the artifact does not bundle the SDK or a driver.

### Run CPU regressions locally

Use Linux with Rust 1.94.0, Cargo, Python 3 and a C compiler (`cc`). A CANN SDK,
Ascend driver, NPU, Docker daemon and cluster credentials are not required for
these regression checks.

```sh
bash scripts/ci.sh
python3 scripts/ci/check-doc-links.py
```

The CPU script builds in a temporary directory and removes it on exit. It never
replaces an installed library. Its `libvnpu.so` is linked against test-only
runtime/DCMI dependencies and must not be deployed. Any unexpected call into
those link dependencies aborts; the spatial runner supplies its own fake ACL
backend for the calls under test. Standalone workspace `cargo check --workspace
--all-targets --locked` performs Rust type checking without a CANN installation.

| Check | Coverage |
| --- | --- |
| Workspace `cargo check` and `cargo test` | Both Rust crates; configuration defaults, bounds and migration errors; atomic overdraft pool behavior. |
| `test/manager_cpu` | Production manager arithmetic, fixed rest calculation, report aggregation and spatial exclusion from time rotation. The unused process scan is a panic-on-call test double. |
| `test/memory_cpu` | Production memory scan/quota paths, rollback, shared-memory ABI and bounded fork cleanup. Procfs reads and CANN calls use explicit test doubles; Linux futexes are real. |
| `test/worker_cpu/run.py` | Source-linked worker and hook admission cases with observed lock/futex boundaries. Each case runs in a separate process with a timeout. |
| `test/process_local.sh` | Actual per-process cache, concurrent initialization and real fork cases. |
| `test/spatial_cpu/run.py` | Actual built Rust hook library preloaded into a C client with fake ACL: ceil budgets, resets, API failures, device/context switching and fork handling. |
| Documentation links | Local Markdown link destinations in the root READMEs and `docs/`; no external requests or rendered-anchor checks. |

The [workflow](../.github/workflows/ci.yml) runs on pull requests, pushes to `main`
and manual dispatch. CPU jobs use native x86-64 and ARM64 Ubuntu 24.04 runners,
separate from the release job. It uses a read-only repository token and does not
retain checkout credentials. Repository settings can require a maintainer to
approve the first workflow run from a fork.

### Hardware validation

Release CI establishes compilation and dynamic linkage against the recorded SDK;
CPU checks establish software behavior against controlled backends. Neither
establishes the minimum supported CANN version, hardware resource enforcement,
operator coverage, scheduling fairness, HBM accuracy on a device, or throughput.
The CANN 8.5.2 build version alone is not a claim of spatial hardware validation on
that runtime.

Before releasing a core change, validate the deployment in the intended
CANN/driver environment, check the selected physical device and effective core
budgets, run allocation and cleanup checks, and measure the actual workload after
warmup. Test each selected policy and resource configuration; keep those results
separate from CI. The historical [spatial validation](spatial-policy.md) records
its own hardware and workload scope.

The repository has existing formatting differences, so CI does not impose a
whole-repository `cargo fmt --check` gate in this change. Format Rust files when
changing them and keep formatting-only cleanup separate from behavior changes.

## 中文

### 构建发布库

[发布构建任务](../.github/workflows/ci.yml) 使用真实 CANN runtime 和 DCMI 库完整编译
Rust 工作区，与本地构建共用 [Dockerfile](../dockerfiles/Dockerfile) 和
[Make 入口](../Makefile)，采用与
[HAMi-core](https://github.com/Project-HAMi/HAMi-core/blob/main/.github/workflows/build-src.yml)
相同的容器构建方式。

准备 Git、Make、Docker 和 Docker Buildx，在仓库根目录执行：

```sh
make build-in-docker
cd dist && sha256sum -c SHA256SUMS
```

该镜像为 Linux ARM64，建议使用原生 ARM64 构建器；其他架构需先配置 Docker Buildx
的 ARM64 模拟。构建需要联网获取 SDK 镜像及工具链，无需 NPU、驱动内核模块、集群
凭据或仓库 secrets。

构建环境固定使用官方 `quay.io/ascend/cann:8.5.2-910b-ubuntu22.04-py3.11`
镜像的不可变摘要、CANN **8.5.2**、Ubuntu 22.04、HDK 驱动用户态库 **25.5.1** 和
Rust **1.94.0**，完整镜像地址及摘要见上方构建输入表。Rust 依赖通过 `Cargo.lock`
和 `--locked` 固定。即使镜像标签变化，摘要仍确定同一 SDK 镜像。

Dockerfile 下载官方 HDK 包并验证 SHA256，通过 `--noexec` 只提取用户态库，
不安装内核驱动；官方配套表包含 CANN 8.5.2 与 HDK 25.5.x，镜像源码、下载地址及
配套表链接见英文部分。构建使用 `/usr/local/Ascend/cann-8.5.2/lib64` 下的真实
runtime 库，以及 `/usr/local/Ascend/driver/lib64/driver` 下的 DCMI 库和依赖，
产物检查会核对实际解析路径。这里的驱动版本只说明构建时的用户态库；宿主机驱动
应与业务 CANN 和硬件配套。通过 `make build-in-docker OUT_DIR=out` 可修改导出目录。

导出产物包括：

| 文件 | 用途 |
| --- | --- |
| `dist/libvnpu.so` | 可部署到兼容业务容器的 ARM64 release 库。 |
| `dist/build-info.json` | 源码提交、Rust/Cargo 版本、SDK 镜像、CANN 版本信息、SDK 库哈希、ELF 依赖、hook 导出和 SDK API 可用性。 |
| `dist/SHA256SUMS` | 库和构建信息的校验和。 |

导出前，[产物检查脚本](../scripts/ci/verify-release.py) 核对 ELF 架构与共享库类型、
hook 与 SDK 导出符号，以及 `ldd -r` 是否正确解析到指定的真实 runtime/DCMI 库，
并记录 CANN 是否提供空分所需 API。这些检查不会初始化 CANN/DCMI 或执行 NPU 算子。

CI 使用原生 `ubuntu-24.04-arm` 运行器构建，容器内部仍为 Ubuntu 22.04，并将三个
文件上传为 `libvnpu-cann8.5.2-arm64` artifact。业务容器需提供兼容的 ARM64 系统库、
CANN 和驱动库；产物不包含 SDK 或驱动。

### 本地运行 CPU 回归

在 Linux 上准备 Rust 1.94.0、Cargo、Python 3 和 C 编译器（`cc`），运行：

```sh
bash scripts/ci.sh
python3 scripts/ci/check-doc-links.py
```

这些回归检查无需 CANN SDK、Ascend 驱动、NPU、Docker 服务或集群凭据。
CPU 脚本在临时目录构建，退出时清理，不替换已安装的库。脚本生成的 `libvnpu.so`
依赖测试用 runtime/DCMI 链接库，不能部署到业务环境；意外进入这些链接桩函数会
立即中止测试。空分测试使用独立的 fake ACL 后端。
单独执行 `cargo check --workspace --all-targets --locked` 可在没有 CANN 的环境
进行 Rust 类型检查。

- 工作区检查和单元测试：配置默认值、范围、旧变量迁移错误，以及 overdraft 原子票池。
- Manager：实际源码的份额计算、fixed 休息时间、报告聚合、spatial 不参与时间轮转。
- Memory：内存扫描、配额、回滚、共享内存 ABI、带超时的 fork 清理；procfs 和 CANN 使用
  明确的测试替身，Linux futex 使用真实实现。
- Worker：源码链接的准入与 hook 路径，观测锁和 futex 边界，每个用例独立进程并限制超时。
- Process-local：真实缓存实现的并发初始化和 fork。
- Spatial：预加载实际构建的 Rust 库，通过 fake ACL 验证向上取整、重置、API 错误、
  设备/上下文切换及 fork 行为。
- 文档：检查根目录 README 与 `docs/` 的本地 Markdown 链接目标；不访问外部链接，
  不检查渲染后的锚点。

[CI 工作流](../.github/workflows/ci.yml) 在 PR、`main` 推送和手动触发时执行。
CPU 任务使用原生 x86-64 与 ARM64 Ubuntu 24.04 运行器，与发布构建任务分开。
Token 仅有仓库读取权限，不保留 checkout 凭据；fork 的首次运行可能需要维护者
按仓库设置批准。

### 真机验证

发布 CI 验证记录 SDK 下的完整编译与动态链接；CPU 测试验证受控后端下的软件逻辑。
二者均不能确定最低支持的 CANN 版本，也不代表硬件限额生效、算子覆盖率、调度
公平性、设备 HBM 统计准确度或吞吐结果。仅使用 CANN 8.5.2 构建，不代表已在该版本
运行时完成空分真机验证。

Core 发布前，应在目标 CANN/驱动环境核对实际选卡和核心预算，验证分配与清理，
并在完整预热后测量业务负载。验证所选策略和资源配置，将结果与 CI 分开记录。
[已有空分验证](spatial-policy.md) 单独说明了其硬件与负载范围。

仓库已有格式差异，本次不增加全仓 `cargo fmt --check` 门禁。修改 Rust 文件时保持
格式整洁，并将纯格式清理与行为修改分开。
