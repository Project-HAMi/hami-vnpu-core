# Memory ledger CPU regression tests

```sh
cargo test --manifest-path test/memory_ledger/Cargo.toml --locked -- --test-threads=1
```

This harness compiles the actual worker and shared-memory layout without editing
production source. CANN entry points are test doubles. Linux uses the actual
futex implementation; the macOS shim allows these non-scheduler tests to run.
The suite validates quota reservations, query/cleanup interleavings, allocation
rollback, failed frees, client lifetime and overflow. It is CPU evidence, not
Ascend runtime or performance validation.
