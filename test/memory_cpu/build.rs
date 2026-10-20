use std::{env, fs, path::PathBuf};
fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let repository = env::var_os("HAMI_SOURCE_ROOT").map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("../.."));
    let source = repository.join("crates/limiter/src").canonicalize().expect("Set HAMI_SOURCE_ROOT to the actual candidate repository root");
    let output = PathBuf::from(env::var("OUT_DIR").unwrap());
    println!("cargo:rerun-if-env-changed=HAMI_SOURCE_ROOT");
    let shared = fs::read_to_string(source.join("shmem/mod.rs")).unwrap();
    let futex = if env::var("CARGO_CFG_TARGET_OS").unwrap() == "linux" {
        format!("#[path = {:?}] pub mod futex;", source.join("shmem/futex.rs"))
    } else {
        "pub mod futex { pub fn wait(_: &std::sync::atomic::AtomicU32, _: u32) {} pub fn wait_timeout(_: &std::sync::atomic::AtomicU32, _: u32, _: u64) {} pub fn wake(_: &std::sync::atomic::AtomicU32, _: i32) {} pub fn wake_all(_: &std::sync::atomic::AtomicU32) {} }".into()
    };
    fs::write(output.join("shared.rs"), shared.replace("pub mod setup;", &format!("#[path = {:?}] pub mod setup;", source.join("shmem/setup.rs"))).replace("pub mod futex;", &futex)).unwrap();
    let worker = fs::read_to_string(source.join("worker.rs")).unwrap();
    assert_eq!(worker.matches("use std::fs;").count(), 1, "worker fs import changed; review the test seam before building");
    let instrumented = worker.replacen("use std::fs;", "#[cfg(test)] use crate::procfs_test_double as fs;\n#[cfg(not(test))] use std::fs;", 1);
    fs::write(output.join("worker-under-test.rs"), instrumented).unwrap();
    fs::write(output.join("source.rs"), format!("#[path = {:?}] pub mod config; #[path = {:?}] pub mod overdraft; pub mod worker {{ include!({:?}); #[cfg(test)] mod memory_scan {{ include!({:?}); }} }}", source.join("config.rs"), source.join("overdraft.rs"), output.join("worker-under-test.rs"), manifest.join("src/memory_scan.rs").canonicalize().unwrap())).unwrap();
    let mut generated = fs::read_to_string(output.join("source.rs")).unwrap();
    generated.push_str(&format!("#[path = {:?}] pub mod manager; #[cfg(test)] #[path = {:?}] mod quota_units;", source.join("manager.rs"), manifest.join("src/quota_units.rs")));
    fs::write(output.join("source.rs"), generated).unwrap();
    fs::write(output.join("test-seam.txt"), format!("source={}\nOnly worker's std::fs import is redirected during cfg(test); proc checks are deterministic test-double calls, not real syscall measurements.\n", source.join("worker.rs").display())).unwrap();
    println!("cargo:warning=memory scan audit: actual worker at {}; only cfg(test) fs import redirected", source.join("worker.rs").display());
    for name in ["config.rs", "overdraft.rs", "worker.rs", "manager.rs", "shmem/mod.rs", "shmem/setup.rs", "shmem/futex.rs"] {
        println!("cargo:rerun-if-changed={}", source.join(name).display());
    }
    for name in ["src/memory_scan.rs", "src/procfs_test_double.rs", "src/quota_units.rs"] {
        println!("cargo:rerun-if-changed={}", manifest.join(name).display());
    }
}
