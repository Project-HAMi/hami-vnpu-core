# Two-card hardware smoke

This manual CANN 8.5.2 / 910B3 probe expects exactly two visible, idle physical
cards 6 and 7, mapped to logical devices 0 and 1. Build with `bash build.sh`
inside an NPU-enabled CANN container. It is not part of the CPU-only CI suite.

Run `./multicard_probe native` for a native baseline. For each managed run, use
a fresh writable SHM base path and the matching release library:

```bash
export NPU_GLOBAL_SHM_PATH=/experiment/run/registry
export NPU_LOCAL_SHM_PATH=/experiment/run/local
export NPU_MEMORY_LIMIT_MB=512 NPU_COMPUTE_LIMIT=33
export NPU_SCHED_POLICY=spatial
LD_PRELOAD=/experiment/release/libvnpu.so ./multicard_probe spatial
```

Repeat with `dynamic`, `fixed`, and `overdraft` (both the environment and the
argument). Each run allocates 300 MiB per card, rejects a second 300 MiB on one
card, and accepts another 64 MiB on the other. It checks per-card memory reports,
sets/resets tracked spatial stream budgets after both cards are initialized,
checks target-card physical-memory allocation/free while another card is current,
and verifies every output of a 512×512 FP16 MatMul on each card. This is a
correctness smoke, not a throughput or fairness benchmark. For scope and SHM
migration, see [configuration](../../docs/configuration.md).
