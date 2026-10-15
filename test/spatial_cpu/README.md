# Spatial policy CPU integration tests

```sh
cargo build --release --locked -p hook
python3 test/spatial_cpu/run.py target/release/libvnpu.so
```

Run on Linux in the normal CANN build environment. The runner preloads the actual
built Rust library into a C client linked to a fake ACL backend. No device or
compute workload is used. Existing runtime/DCMI link dependencies must be
resolvable. The fake backend and all test processes are temporary and bounded.

Cases cover ceil arithmetic and refusal, original error returns, optional API
absence, readback failures, pass-through for other policies, stream/device resets,
reference-counted nonfinal reset, distinct physical counts on two devices,
concurrent repeated initialization, and fork before/during/after initialization.
An explicit stream resource override after selecting multiple devices is refused
because CANN 9.1 exposes no general public stream-to-device query. These tests
verify hooks and configuration; they do not establish hardware enforcement or
custom-kernel coverage.
