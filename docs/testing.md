# Testing and CI / 测试与 CI

## English

### Run the same checks locally

Use Linux with Rust 1.94.0, Cargo, Python 3 and a C compiler (`cc`). A CANN SDK,
Ascend driver, NPU, Docker daemon and cluster credentials are not required.

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
Normal release builds still need the real CANN runtime and DCMI libraries.

### What runs

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
and manual dispatch. CPU jobs use native x86-64 and ARM64 Ubuntu 24.04 runners.
It uses a read-only repository token, does not retain checkout credentials, and
requires no secrets or access to a private environment. Repository settings can
require a maintainer to approve the first workflow run from a fork.

### What still needs an NPU

CPU checks establish software behavior against controlled backends. They do not
measure CANN/driver compatibility, hardware resource enforcement, operator
coverage, scheduling fairness, HBM accuracy on a device, or throughput. Before
releasing a core change, validate the deployment in the intended
CANN/driver environment, check the selected physical device and effective core
budgets, run allocation and cleanup checks, and measure the actual workload after
warmup. Test each selected policy and resource configuration; keep those
results separate from CPU CI. The historical [spatial validation](spatial-policy.md)
records its own hardware and workload scope.

The repository has existing formatting differences, so CI does not impose a
whole-repository `cargo fmt --check` gate in this change. Format Rust files when
changing them and keep formatting-only cleanup separate from behavior changes.

## 中文

### 本地复现

在 Linux 上准备 Rust 1.94.0、Cargo、Python 3 和 C 编译器（`cc`），运行：

```sh
bash scripts/ci.sh
python3 scripts/ci/check-doc-links.py
```

无需 CANN SDK、Ascend 驱动、NPU、Docker 服务或集群凭据。CPU 脚本在临时目录构建，
退出时清理，不替换已安装的库。脚本生成的 `libvnpu.so` 依赖测试用 runtime/DCMI
链接库，不能部署到业务环境；意外进入这些链接桩函数会立即中止测试。
空分测试使用独立的 fake ACL 后端。真实发布构建仍需真实 CANN runtime 和 DCMI 库。
单独执行 `cargo check --workspace --all-targets --locked` 可在没有 CANN 的环境进行
Rust 类型检查。

### 覆盖范围

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

[CI 工作流](../.github/workflows/ci.yml) 在 PR、`main` 推送和手动触发时执行，使用
原生 x86-64 与 ARM64 Ubuntu 24.04 运行器。Token 仅有仓库读取权限，不保留 checkout
凭据，不依赖 secrets 或私有环境；fork 的首次运行可能需要维护者按仓库设置批准。

### 真机验证

CPU 测试验证受控后端下的软件逻辑，不代表真实 CANN/驱动兼容性、硬件限额生效、
算子覆盖率、调度公平性、设备 HBM 统计准确度或吞吐结果。Core
发布前，应在目标环境核对实际选卡和核心预算，验证分配与清理，并在完整预热后测量
业务负载。验证所选策略和资源配置，将结果与 CPU CI 分开记录。
[已有空分验证](spatial-policy.md) 单独说明了其硬件与负载范围。

仓库已有格式差异，本次不增加全仓 `cargo fmt --check` 门禁。修改 Rust 文件时保持
格式整洁，并将纯格式清理与行为修改分开。
