# HAMi-vnpu-core

[English](README.md) | [简体中文](README.zh-CN.md)

HAMi-vnpu-core 是运行在容器内的昇腾 NPU 资源控制库。Rust 编写的
`libvnpu.so` 拦截运行时调用，维护共享 HBM 记账，并提供四种算力调度策略。
参与进程会在内部选举 manager，无需单独启动守护进程。

## 选择调度策略

| 策略 | 控制方式 | 适用需求 |
| --- | --- | --- |
| `dynamic` | 按相对份额调度，允许借用空闲算力 | 注重整体利用率的共享负载 |
| `fixed` | 按配置计算运行与休息比例 | 需要目标时间份额的负载 |
| `overdraft` | 在 dynamic 基础上，允许 IDLE 状态下有界的准入借用 | 希望减少准入等待的负载 |
| `spatial` | 配置每个进程的 Cube/Vector 协作式资源预算 | 遵守 CANN 资源限制、适合并行执行的算子 |

core 默认使用 `spatial`，`NPU_COMPUTE_LIMIT` 默认值仍为 `1`，请按负载显式设置算力值。
需要时分调度时，显式选择 `dynamic`、`fixed` 或 `overdraft`。
不同策略对数值的解释不同，不能统一理解为吞吐百分比。
详见[策略指南](docs/scheduling-policies.zh-CN.md)中的利弊、选型建议和示例。

## 构建

发布 CI 和本地构建共用[同一个 Dockerfile](dockerfiles/Dockerfile)。
准备 Git、Make、Docker 和 Docker Buildx，在仓库根目录执行：

```sh
make build-in-docker
cd dist && sha256sum -c SHA256SUMS
```

该流程使用真实的 CANN runtime/DCMI 库完整构建 release 版本，导出
`dist/libvnpu.so`、`dist/build-info.json` 和 `dist/SHA256SUMS`。
Dockerfile 使用官方 `quay.io/ascend/cann:8.5.2-910b-ubuntu22.04-py3.11`
镜像并固定摘要，Rust 固定为 **1.94.0**；下载官方 **HDK 25.5.1** 包并验证 SHA256，
只提取真实驱动用户态库用于链接，不安装内核驱动。
构建环境为 **CANN 8.5.2、Ubuntu 22.04**，支持 **Linux AMD64 和 ARM64**，Make
默认选择当前主机架构。构建两种架构时，显式选择目标并使用不同的产物目录：

```sh
make build-in-docker PLATFORM=linux/amd64 OUT_DIR=dist/amd64
make build-in-docker PLATFORM=linux/arm64 OUT_DIR=dist/arm64
```

建议使用目标架构的原生构建器，跨架构构建需配置 Docker Buildx 模拟。编译无需 NPU。

将 `libvnpu.so` 部署到架构匹配、具有兼容 CANN 和宿主机驱动库的业务容器。
HDK 驱动用户态库版本只说明构建环境，宿主机驱动应与业务 CANN 和硬件配套。
编译成功不能直接确定最低兼容 CANN 版本，也不能代替各策略的真机验证。
产物来源、检查内容和验证范围见[测试与 CI](docs/testing.md)。

如需在已安装真实 CANN/DCMI 开发库的 Linux 主机上直接构建，准备 Rust 1.94.0
和 C 编译器，按安装包说明初始化 CANN 环境后执行
`cargo build --release --workspace --locked`，产物为 `target/release/libvnpu.so`。
非标准 SDK 路径见[配置参考](docs/configuration.zh-CN.md)。

## 运行应用

先准备能正常使用 NPU 的 Linux 容器，确保设备和昇腾运行时库可用。
将同一个可写的宿主机共享目录挂载到参与容器的 `/hami-shared-region`，
将构建的库挂载到 `/usr/local/lib/libvnpu.so`。

在业务容器内执行：

```sh
# 同一物理卡 7 上需要协调的负载共享此文件，其他物理卡使用不同文件。
export NPU_GLOBAL_SHM_PATH=/hami-shared-region/7_global_registry
# 仅与同一本地记账组、同一 PID 命名空间内的参与进程共享。
export NPU_LOCAL_SHM_PATH=/tmp/vnpu-local
export NPU_MEMORY_LIMIT_MB=10240
export NPU_COMPUTE_LIMIT=30
export NPU_SCHED_POLICY=spatial # 默认策略，此处显式写出。
LD_PRELOAD=/usr/local/lib/libvnpu.so python3 your_model.py
```

文件名中的卡号表示物理设备协调范围；业务仍选择容器内的逻辑设备，通常为 `0`。
包括默认空分策略在内，manager 配置仍要求设置 `NPU_GLOBAL_SHM_PATH`。

该配置设置共享的 10 GiB HBM 配额。`spatial` 为每个进程设置
`ceil(物理核心数 × 30 / 100)` 的预算：在 20 Cube / 40 Vector 的设备上为
6 Cube / 12 Vector。这些预算不代表独占的物理核心分区。

同一本地共享组的进程必须使用相同的策略、资源配置、库版本、本地文件和 PID
命名空间；独立记账组必须使用不同的本地文件。SHM 文件由库创建，不要预先创建空的
registry 文件。切换配置或 SHM 布局前，先停止本组进程，再使用新的本地文件。
不要截断或删除仍被运行中进程映射的文件。

使用 `spatial` 时，在算子初始化与 tiling 前选择设备或 context。
父进程初始化设备后，NPU worker 应通过 spawn 启动。
算子兼容性和进程生命周期要求见[策略指南](docs/scheduling-policies.zh-CN.md)。

## 文档

| 指南 | 内容 |
| --- | --- |
| [调度策略](docs/scheduling-policies.zh-CN.md) | 四种策略的利弊、选型与示例 |
| [配置参考](docs/configuration.zh-CN.md) | 环境变量、默认值、单位、进阶调优与迁移 |
| [测试与 CI](docs/testing.md) | 发布产物构建、CPU 检查及其覆盖范围 |
| [空分设计](docs/spatial-policy.md) | 实现细节与历史验证记录 |

## 开发检查

```sh
bash scripts/ci.sh
```

在 Linux 上运行，依赖见[测试与 CI](docs/testing.md)。CPU 回归任务使用假的 ACL/DCMI
后端及直接引用生产源码的测试，检查配置、调度逻辑与 hook 行为。独立的发布构建任务
使用真实 SDK，分别上传通过检查的 AMD64 和 ARM64 库。NPU 执行和业务性能需要
在兼容的昇腾环境中另行验证。
