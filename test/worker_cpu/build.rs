use std::{env, fs, path::PathBuf};
fn main() {
    assert_eq!(env::var("CARGO_CFG_TARGET_OS").unwrap(), "linux", "worker CPU tests require Linux futex");
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest.join("../..").canonicalize().unwrap();
    let src = root.join("crates/limiter/src");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let shared = fs::read_to_string(src.join("shmem/mod.rs")).unwrap()
        .replace("pub mod setup;", &format!("#[path = {:?}] pub mod setup;", src.join("shmem/setup.rs")))
        .replace("pub mod futex;", &format!("#[path = {:?}] pub mod futex;", src.join("shmem/futex.rs")));
    fs::write(out.join("shared.rs"), shared).unwrap();
    fs::write(out.join("source.rs"), format!(
        "#[path = {:?}] pub mod config; pub mod worker {{ include!({:?}); #[cfg(test)] pub mod audit {{ include!({:?}); }} }} #[path = {:?}] pub mod actual_hook;",
        src.join("config.rs"), src.join("worker.rs"), manifest.join("src/cases.rs"),
        root.join("crates/hook/src/hook.rs"))).unwrap();
    for name in ["config.rs", "worker.rs", "shmem/mod.rs", "shmem/setup.rs", "shmem/futex.rs"] {
        println!("cargo:rerun-if-changed={}", src.join(name).display());
    }
    println!("cargo:rerun-if-changed={}", root.join("crates/hook/src/hook.rs").display());
    println!("cargo:rerun-if-changed={}", manifest.join("src/cases.rs").display());
}
