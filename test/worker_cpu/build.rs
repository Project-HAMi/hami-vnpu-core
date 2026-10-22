use std::{env, fs, path::PathBuf};
fn main() {
    assert_eq!(env::var("CARGO_CFG_TARGET_OS").unwrap(), "linux", "worker CPU tests require Linux futex");
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest.join("../..").canonicalize().unwrap();
    let src = root.join("crates/limiter/src");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let shared = fs::read_to_string(src.join("shmem/mod.rs")).unwrap()
        .replace("pub mod setup;", &format!("#[path = {:?}] pub mod setup;", src.join("shmem/setup.rs")))
        .replace("pub mod futex;", "pub use crate::wait_observer::futex;");
    fs::write(out.join("shared.rs"), shared).unwrap();
    // Only redirect the lock type so tests can publish a state change exactly
    // after a real write guard unlocks. No worker branch or atomic is rewritten.
    let worker = fs::read_to_string(src.join("worker.rs")).unwrap();
    let import = "use std::sync::{Arc, Mutex, RwLock};";
    assert_eq!(worker.matches(import).count(), 1);
    fs::write(out.join("worker.rs"), worker.replace(import,
        "use std::sync::{Arc, Mutex}; use crate::wait_observer::RwLock;")).unwrap();
    fs::write(out.join("source.rs"), format!(
        "#[path = {:?}] pub mod raw_futex; #[path = {:?}] pub mod config; #[path = {:?}] pub mod overdraft; pub mod worker {{ include!({:?}); #[cfg(test)] pub mod audit {{ include!({:?}); }} }} #[path = {:?}] pub mod actual_hook; #[path = {:?}] pub(crate) mod process; #[path = {:?}] pub mod spatial; #[path = {:?}] pub(crate) mod ownership; #[path = {:?}] pub(crate) mod memory; #[path = {:?}] pub mod actual_memory_extra; #[path = {:?}] pub mod actual_memory_physical;",
        src.join("shmem/futex.rs"), src.join("config.rs"), src.join("overdraft.rs"), out.join("worker.rs"), manifest.join("src/cases.rs"),
        root.join("crates/hook/src/hook.rs"), root.join("crates/hook/src/process.rs"),
        root.join("crates/hook/src/spatial.rs"), root.join("crates/hook/src/ownership.rs"),
        root.join("crates/hook/src/memory.rs"), root.join("crates/hook/src/memory_extra.rs"),
        root.join("crates/hook/src/memory_physical.rs"))).unwrap();
    for name in ["config.rs", "overdraft.rs", "worker.rs", "shmem/mod.rs", "shmem/setup.rs", "shmem/futex.rs"] {
        println!("cargo:rerun-if-changed={}", src.join(name).display());
    }
    println!("cargo:rerun-if-changed={}", root.join("crates/hook/src/hook.rs").display());
    println!("cargo:rerun-if-changed={}", root.join("crates/hook/src/process.rs").display());
    println!("cargo:rerun-if-changed={}", root.join("crates/hook/src/spatial.rs").display());
    println!("cargo:rerun-if-changed={}", root.join("crates/hook/src/ownership.rs").display());
    println!("cargo:rerun-if-changed={}", root.join("crates/hook/src/memory.rs").display());
    println!("cargo:rerun-if-changed={}", root.join("crates/hook/src/memory_extra.rs").display());
    println!("cargo:rerun-if-changed={}", root.join("crates/hook/src/memory_physical.rs").display());
    println!("cargo:rerun-if-changed={}", manifest.join("src/cases.rs").display());
    println!("cargo:rerun-if-changed={}", manifest.join("src/memory_hooks.rs").display());
    println!("cargo:rerun-if-changed={}", manifest.join("src/runtime_memory_extra.rs").display());
    println!("cargo:rerun-if-changed={}", manifest.join("src/runtime_memory_physical.rs").display());
}
