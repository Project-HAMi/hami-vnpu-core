# Manager and DCMI CPU regressions

```sh
cargo test --manifest-path test/manager_cpu/Cargo.toml --locked
```

Run on Linux. This standalone crate compiles the repository manager, registry
lock, DCMI collector, config and shared-memory code without CANN or an NPU.
Tests cover bounded rounds after partial progress, polluted estimates, incomplete
reports, waiting heartbeats, manager-generation fencing, fixed-share handoff,
slot rotation, helper cancellation/timeouts, output bounds and protocol errors.
Shared-memory path tests cover missing files, symlink/hard-link aliases, and
supervisor rejection before the existing registry can be truncated. The helper
integration fixture additionally covers worker rejection, relative preload and
helper paths across `chdir`, and child reaping without `/proc/.../children`.
The periodic reaper is not started by these cases. Vendor calls are exercised
separately by the helper integration fixture and real-device validation.
