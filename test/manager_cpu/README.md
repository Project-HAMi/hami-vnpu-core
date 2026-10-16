# Manager arithmetic CPU tests

```sh
cargo test --manifest-path test/manager_cpu/Cargo.toml --locked
```

Run on Linux. This standalone crate compiles the repository manager, config and shared-memory code; it does not require CANN or an NPU. The unused process-liveness dependency is replaced by a panic-on-call stub. Tests cover ordinary priorities and fixed-share budgets, overflowing timeout and report-end arithmetic, and report aggregation. They do not simulate NPU scheduling or establish throughput gains.
