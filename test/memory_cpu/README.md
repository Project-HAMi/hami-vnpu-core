# Memory quota CPU tests

Run on Linux with Rust 1.85 or newer:

```sh
cargo test --manifest-path test/memory_cpu/Cargo.toml --locked -- --test-threads=1
```

The harness compiles the repository worker, shared-memory structures and setup code. Only the worker's `std::fs` import is redirected to a deterministic procfs test double; CANN calls are mocked. Linux uses the real futex implementation and a bounded real-fork test. These tests cover scan, quota, rollback and ABI behavior, not NPU performance or concurrent allocation-accounting redesign. `HAMI_SOURCE_ROOT` can select a baseline checkout for negative controls.
