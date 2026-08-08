---
name: Bug Report
about: Report a problem encountered while using hami-vnpu-core
labels: bug
---

<!-- Please use this template while reporting a bug and provide as much info as possible. Not doing so may result in your bug not being addressed in a timely manner. Thanks!
-->

**What happened**:

**What you expected to happen**:

**How to reproduce it (as minimally and precisely as possible)**:

**Anything else we need to know?**:

- Relevant excerpts from `npu-smi info`; mask NPU serial numbers, device identifiers, and host or node identifiers
- Relevant, time-bounded excerpts from the workload and `libvnpu.so` logs or Rust backtraces
- Relevant `NPU_GLOBAL_SHM_PATH`, `NPU_MEM_QUOTA`, `NPU_PRIORITY`, and `LD_PRELOAD` values
- Relevant container configuration, shared-region mount, and permission details
- Relevant, time-bounded Ascend driver or kernel output

Before posting, remove or mask credentials, tokens, NPU identifiers, process or workload identifiers, host paths, node or host identifiers, internal image names, and other sensitive data from configuration and logs.

**Environment**:
- hami-vnpu-core version or commit:
- Ascend NPU model:
- Ascend driver and CANN version:
- Workload framework and version:
- Container runtime, image, and tag:
- CPU architecture and operating system:
- Rust toolchain version, if building locally:
- Kernel version from `uname -a`:
- Others:
