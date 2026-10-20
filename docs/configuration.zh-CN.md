# 配置说明

[English](configuration.md) · [策略选择](scheduling-policies.zh-CN.md)

本文对应本分支的库。每个参与控制的应用进程都应加载匹配的 `libvnpu.so`，并在启动前配置环境变量。修改环境变量不会升级旧版库二进制。

## 资源与协同配置

| 环境变量 | 默认值 | 取值与含义 |
| --- | --- | --- |
| `NPU_GLOBAL_SHM_PATH` | 受控工作负载必填 | 位于可读写、宿主机共享目录中的非空文件路径。需要在同一设备上进行时分协调的工作负载应访问同一个底层注册表文件。库会创建文件，无须手动创建或调整大小。`spatial` 的 manager 配置也需要该变量。 |
| `NPU_LOCAL_SHM_PATH` | `/tmp/vnpu_local_shmem` | 由应用进程组成的一个记账/调度组的非空文件路径。组内参与进程须访问同一个底层文件；独立组使用不同文件。 |
| `NPU_MEMORY_LIMIT_MB` | `0` | 非负整数。虽然名称为 `_MB`，实际单位为 MiB，`1` 对应 `1,048,576` 字节。`0` 关闭 HBM 配额。转换为 `u64` 字节数时会溢出的值被拒绝。 |
| `NPU_COMPUTE_LIMIT` | `1` | `1`～`100` 的整数。`dynamic`/`overdraft` 中为相对份额，`fixed` 中为运行/休息目标百分比，`spatial` 中为每进程的 Cube/Vector 协作预算百分比。请按工作负载显式设置。 |
| `NPU_SCHED_POLICY` | `spatial` | 仅接受 `dynamic`、`fixed`、`overdraft`、`spatial`。去除首尾空白，core 的策略名区分大小写。详见[策略利弊](scheduling-policies.zh-CN.md)。 |

同一组环境变量分别作用于每张选中的物理卡。`NPU_MEMORY_LIMIT_MB=20480` 表示**每张卡各有 20 GiB**，由组内使用该卡的进程共享；卡 A 的用量不占用卡 B 的额度。不需要逐卡配置列表。每张卡的时分调度状态和令牌也独立。`spatial` 算力预算分别作用于每个进程/设备；增加 worker 进程不会在进程之间平分该预算。

准备好可写共享目录并安装匹配的库后，可这样启动：

```bash
export NPU_GLOBAL_SHM_PATH=/hami-shared-region/registry
export NPU_LOCAL_SHM_PATH=/tmp/vnpu-local
export NPU_MEMORY_LIMIT_MB=20480
export NPU_COMPUTE_LIMIT=30
export NPU_SCHED_POLICY=spatial
LD_PRELOAD=/opt/hami/libvnpu.so python3 your_model.py
```

应用通过 CANN 选择设备或 Context。两个路径都是**基础路径**，库会追加 `.device-<物理卡 ID>`，单卡也采用此规则。例如使用物理卡 6、7 时，会创建 `/tmp/vnpu-local.device-6` 和 `/tmp/vnpu-local.device-7`，各有 20 GiB 配额。库通过 CANN 将逻辑序号映射到物理卡，因此不同进程的可见设备顺序不同也不会记错卡。同卡竞争的时分负载应使用同一全局基础路径；独立记账组使用不同本地基础路径。


## 共享内存与进程生命周期

每张物理卡首次进入被拦截的 NPU 路径时，库为该卡延迟启动 supervisor。共享本地 SHM 的进程选出进程内 manager，无须另起 manager 程序。宿主进程至少运行三秒后才参加选举；worker 最多等待十秒，等待本地 SHM 完成初始化。

共享本地文件的进程必须采用相同策略、配额、全局注册表路径和 PID namespace，且能互相读取 `/proc/<pid>/stat`。库通过这些 PID 完成选举及退出进程清理。配置一致性需要部署保证，SHM 不会校验配置指纹。如果一个组跨越容器等隔离边界，需要显式安排共享文件访问和共享 PID namespace。

升级库、切换策略或配置时，先停止所有使用该本地文件的进程，再使用全新的本地 SHM 文件启动。参与进程仍在映射文件时，不要截断、删除或重建 SHM。追加字段保留了监控偏移，不代表新旧进程映射可以混用。打开全局注册表时只会等待文件至少达到当前布局大小，不识别兼容版本，也不校验 schema。全局布局兼容性需要部署保证；只有全部使用者停止后才能替换该注册表。

资源配置非法、无法确定当前物理卡或无法初始化必要的每卡 limiter 时，进程以状态码 `78` 退出。空分资源设置或读回校验失败也以 `78` 退出。

HBM 准入计数包含尚未完成的分配预留；分配失败时回退。切卡后释放已拦截的分配，仍扣减其原属卡的用量。每卡域的 DCMI 快照仅用于诊断，不覆盖这份记账。限额覆盖被拦截的分配，不覆盖绕过库的程序。每张卡上的不同 Context 使用各自的计时 stream/event。每卡域有 32 个由 Context 共享的计时/报告槽位和 64 个进程记账槽位；空分 worker 不占计时槽位。刚销毁的 Context 已完成的报告可能保留到当前批次消费完毕。

SHM 字段偏移保持不变，但文件名现在表示物理卡域。监控程序须读取带后缀的文件，每个文件中的 `hbm_used[0]` 表示该卡用量。升级时停止旧参与进程并使用新的基础路径；旧的无后缀文件不会与此版本协调。


显式 Stream 资源修改按记录的归属卡处理。Kernel 提交仍遵守 CANN 的要求：Stream 所属 Context 必须是当前 Context，请先切换再提交。设备重置会使已跟踪的 Stream 归属失效；重新选择设备/Context 后重新创建 Stream。

## 高级调度与日志

除非针对具体时分工作负载做测量，建议保留调度参数默认值。这些参数影响时分策略，不改变空分核心预算。

| 环境变量 | 默认值 | 作用与解析方式 |
| --- | --- | --- |
| `NPU_TOKEN_SCALE` | `100.0` | admission 批次大小及 `fixed` 休息时长的浮点倍率。解析后的值最小为 `0.1`；字符串无法解析时回退为 `1.0`，与未设置时的默认值不同。使用有限正数。 |
| `NPU_AVG_ALPHA` | `0.7` | 更新平均 kernel 时长时，新测量值的浮点权重；值越大响应越快。解析失败回退为 `0.7`；解析后的值限制在 `0.05`～`0.95`。使用有限数值。 |
| `RUST_LOG` | `info` | `env_logger` 日志过滤器，例如 `RUST_LOG=limiter=debug,hook=debug`。空分初始化及致命错误也会直接写入 stderr。 |
| `RUST_LOG_STYLE` | `auto` | `env_logger` 颜色样式：`auto`、`always`、`never`。不影响直接写入 stderr 的诊断。 |

`overdraft` 的内部默认值为 10 个共享 admission ticket、每次 admission 调用累计阻塞 5,000 µs 后允许借用、1,000 µs 轮询。这些参数没有环境变量。已经打开的池耗尽后可在同一轮内再次补充；10 表示池容量，不表示每轮累计上限或运行时长预算。通过 `NPU_SCHED_POLICY=overdraft` 启用。

虚拟 HBM 空闲显存查询会预留 128 MiB 开销。该值为内部常量，不可通过环境变量配置；已配置的配额仍是分配上限。

## 库加载与构建路径

| 环境变量 | 生效范围 | 含义 |
| --- | --- | --- |
| `LD_PRELOAD` | 应用的 Linux 动态加载器 | `libvnpu.so` 的绝对路径，例如 `/opt/hami/libvnpu.so`。启动每个参与控制的应用进程时设置。 |
| `LD_LIBRARY_PATH` | 应用/构建加载器 | Linux 通用依赖搜索路径。需能找到匹配的 CANN runtime 和 Ascend driver 库；它是加载器配置，不是配额控制。 |
| `ASCEND_TOOLKIT_HOME` | 仅构建时 | Toolkit 根目录，默认 `/usr/local/Ascend/ascend-toolkit/latest`；构建添加 `<root>/lib64` 原生库搜索路径。 |
| `ASCEND_DRIVER_HOME` | 仅构建时 | Driver 根目录，默认 `/usr/local/Ascend/driver`；构建添加 `<root>/lib64/driver` 并链接 `dcmi`。 |

库还会链接 CANN `runtime`。请面向目标 Linux 架构构建，并在部署中提供兼容的 runtime/driver 依赖。设置两个构建目录变量不会自动安装这些依赖，也不会设置应用的运行时库搜索路径。

## 旧变量迁移

默认调度策略已改为 `spatial`，`NPU_COMPUTE_LIMIT` 默认值仍为 `1`。此前通过不设置
策略来使用 Dynamic 的部署，应显式设置 `NPU_SCHED_POLICY=dynamic` 以保留原有行为。
所有策略都仍须配置 `NPU_GLOBAL_SHM_PATH`。

必须完全删除旧变量；即使设为空字符串，或同时存在新变量，当前解析器仍会拒绝其存在。

| 已移除变量 | 替代配置 |
| --- | --- |
| `NPU_MEM_QUOTA` | `NPU_MEMORY_LIMIT_MB`，单位 MiB |
| `NPU_PRIORITY` | `NPU_COMPUTE_LIMIT`，`1`～`100` 的整数 |
| `NPU_FIXED_SHARE_RATIO` | 原 fixed-share 行为使用 `NPU_SCHED_POLICY=fixed`；相对共享使用 `dynamic` |
| `NPU_OVERDRAFT_ENABLED` | `NPU_SCHED_POLICY=overdraft` |

库不会读取 `NPU_LOCAL_SHM_NAME`。应通过 `NPU_LOCAL_SHM_PATH` 指定文件路径，
选择本地记账组；只设置名称不会覆盖默认的 `/tmp/vnpu_local_shmem`。

实现来源：[资源解析](../crates/limiter/src/config.rs)、[manager 参数](../crates/limiter/src/manager.rs)、[supervisor](../crates/limiter/src/supervisor.rs)、[worker 与 HBM 记账](../crates/limiter/src/worker.rs)、[日志](../crates/limiter/src/lib.rs)、[构建路径](../crates/limiter/build.rs)。
