use std::{env, fs, path::PathBuf};

fn main() {
    let source = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("../../crates/limiter/src").canonicalize().unwrap();
    let output = PathBuf::from(env::var("OUT_DIR").unwrap());
    let shared = fs::read_to_string(source.join("shmem/mod.rs")).unwrap();
    let futex = if env::var("CARGO_CFG_TARGET_OS").unwrap() == "linux" {
        format!("#[path = {:?}] pub mod futex;", source.join("shmem/futex.rs"))
    } else {
        "pub mod futex { pub fn wait(_: &std::sync::atomic::AtomicU32, _: u32) {} pub fn wait_timeout(_: &std::sync::atomic::AtomicU32, _: u32, _: u64) {} pub fn wake(_: &std::sync::atomic::AtomicU32, _: i32) {} pub fn wake_all(_: &std::sync::atomic::AtomicU32) {} }".into()
    };
    fs::write(output.join("shared.rs"), shared
        .replace("pub mod setup;", &format!("#[path = {:?}] pub mod setup;", source.join("shmem/setup.rs")))
        .replace("pub mod futex;", &futex)).unwrap();
    fs::write(output.join("source.rs"), format!(
        "#[path = {:?}] pub mod config; #[path = {:?}] pub mod worker;",
        source.join("config.rs"), source.join("worker.rs"))).unwrap();
    for name in ["config.rs", "worker.rs", "shmem/mod.rs", "shmem/setup.rs", "shmem/futex.rs"] {
        println!("cargo:rerun-if-changed={}", source.join(name).display());
    }
}
