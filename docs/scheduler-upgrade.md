# Scheduler and DCMI upgrade / 调度与 DCMI 升级

## English

Install the matching `libvnpu.so` and executable `vnpu-dcmi` from the same build
in one directory. `NPU_DCMI_HELPER_PATH` can override the helper location; relative
paths are anchored to the library's load-time working directory. A missing helper
disables hardware observations, while Hook memory accounting remains active.

DCMI runs in a fresh query process supervised by a dedicated subreaper. The
supervisor cancels and reaps driver descendants on timeout, including detached
children. Queries resolve each physical NPU to its actual card/chip pair; an
unavailable mapping never falls back to another card. Hardware observations do
not replace Hook reservations. Helper callbacks do not register workers or elect
managers, including when `/etc/ld.so.preload` injects the library into the helper.

Time-sliced admission rounds have a five-second upper bound and completion-report
waits are capped at 500 ms. Losing progress after work starts releases the turn;
idle, incomplete, untimed or delayed reports do not train execution-time estimates.
The five-millisecond reporting-delay check measures each worker's entry into
measurement, before any SDK synchronization. Device-drain time after entry may
exceed five milliseconds without losing the sample, but must still complete
within the 500 ms report wait. Missing or stale entry timestamps reject the sample.
Waiting managers refresh their own liveness without extending another owner's
heartbeat. Generation checks fence replaced managers and reused slots. Spatial
managers maintain memory state without joining compute rotation. These deadlines
do not preempt a kernel already submitted to the device.

Normal tickets atomically bind a batch ID to the remaining balance. A worker
paused across a round change cannot spend a replacement batch's tickets using
its old snapshot. A rejected stale attempt returns to state/context initialization.

An upgrade from the original queue protocol or fenced protocols 2/3/4/5 requires a
coordinated restart. Protocol 6 preserves the 2848-byte global layout and the
device-plugin process-memory offsets. The local layout grows to 6768 bytes. The normal
ticket word at local offset 40 now stores the balance in its low 21 bits and the
batch ID in its upper 43 bits. Diagnostic readers must decode that word; old
workers must not share these files. Batch tags are never reused: after the maximum
43-bit batch ID, admission stays closed until a coordinated fresh-state restart.
The 32 per-worker measurement-entry timestamps remain at offset 6256.
A 32-entry admitted-launch count array starts at offset 6512. Workers publish
these counts with their completed reports. Timing averages exclude debited
reservations rejected by the admission post-check.
Permanent publication failure stops the manager and releases its lease through
supervisor cleanup before the next bounded election retry.
Equal file sizes do not imply protocol compatibility.
An incompatible registry exits with code 78 instead of starting an unlimited
worker; repeatedly restarting a Pod cannot repair that registry.

1. Back up the workload configuration and installed artifacts. With KServe, pause
   the InferenceServices using the controller's stop mechanism.
2. Stop **all** participants sharing the physical card's registry. Verify that
   their processes and shared-memory mappings have exited.
3. Install both artifacts. Archive the old global registry and local state only
   after no process maps them. Never truncate or unlink live shared-memory files.
4. Resume the workloads and let the library create fresh state. Do not precreate
   empty registry files. Current paths append `.device-<physical-id>` to both base
   paths, including single-card deployments; derive them from actual runtime
   configuration rather than assuming logical device 0 is physical card 0.
5. Verify the loaded library hashes, Ready status, real inference, token/report
   progress and stable manager generations. Include concurrent requests and an
   idle-to-active transition for time-sliced policies.

For CPU coverage run `bash scripts/ci.sh`. The helper integration exercises the
actual binaries with a fake driver. In a disposable Ascend environment, run
`python3 test/ascend_dcmi_smoke.py --artifacts target/release --report smoke.json`;
also validate the intended framework and compute API paths on real hardware.

## 中文

将同一次构建的 `libvnpu.so` 和可执行的 `vnpu-dcmi` 安装到同一目录。
可用 `NPU_DCMI_HELPER_PATH` 指定 helper；相对路径以库加载时的工作目录为基准。
helper 缺失只停用硬件观测，Hook 内存记账继续生效。

DCMI 查询在全新进程中执行，由独立 subreaper 超时终止并回收驱动派生进程，
包括脱离进程组的子进程。每次查询先将物理 NPU 映射到真实 card/chip；映射失败
不会改查其他卡。硬件快照不覆盖 Hook 预留账本。helper 即使被系统预加载注入，
也不会注册 worker 或参与 manager 选举。

时分准入轮次最多五秒，完成报告等待最多 500 毫秒；开始后失去进展也会交出调度权。
空闲、不完整、未计时及延迟报告不更新执行时间估计。等待中的 manager 单独维持心跳，
代次校验防止被替换的 manager 修改继任者状态。spatial manager 只维护内存状态，
不参与计算轮转。这些期限不会抢占已提交的设备算子。
五毫秒延迟判断只计算 worker 进入测量前的等待；进入测量后的设备同步即使超过五毫秒，
仍可在 500 毫秒报告期限内提交有效样本。缺失或过期的测量入口时间戳会使样本失效。

普通令牌将批次号和余额绑定在同一个原子值中。跨轮次暂停的 worker 不能用旧快照
消耗新批次的令牌；过期尝试会重新检查状态并初始化当前批次的上下文。

旧队列协议及协议 2/3/4/5 升级到协议 6 必须协调停机。全局结构仍为 2848 字节，
本地结构增至 6768 字节，保留设备插件读取的进程显存偏移。本地偏移 40 的普通令牌字
改为低 21 位存余额、高 43 位存批次号；诊断工具必须解码，旧 worker 不能共享这些文件。
批次号不回绕复用，达到 43 位上限后停止准入，需要协调停机并重建共享状态。
32 个 worker 的测量入口时间戳仍从偏移 6256 开始。
偏移 6512 新增 32 个实际准入计数，与完成报告的 batch 一起发布。
耗时均值使用这些计数，排除扣款后被准入复核拒绝的预留。
永久发布失败会退出 manager，由 supervisor 清理租约后再按有界周期重试选主。
文件大小相同不代表协议兼容。
不兼容时进程以 78 退出，单独反复重启 Pod 无法恢复。

按上述步骤备份配置、暂停同卡全部服务、确认映射退出，再安装成对产物并归档旧共享状态。
恢复时让库重新创建文件，注意当前路径追加 `.device-<物理卡 ID>`。
最后验证真实推理、令牌/报告推进、manager 代次及进程实际加载的库，不能只看 Pod Ready。
