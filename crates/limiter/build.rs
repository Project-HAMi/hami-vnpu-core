// Link the Ascend DCMI (+ CANN runtime) libs. Paths default to the standard Ascend
// install and can be overridden via env (CI, a relocated SDK, etc.) rather than hardcoded.
fn main() {
    let toolkit = std::env::var("ASCEND_TOOLKIT_HOME")
        .unwrap_or_else(|_| "/usr/local/Ascend/ascend-toolkit/latest".into());
    let driver =
        std::env::var("ASCEND_DRIVER_HOME").unwrap_or_else(|_| "/usr/local/Ascend/driver".into());
    println!("cargo:rustc-link-search=native={toolkit}/lib64");
    println!("cargo:rustc-link-search=native={driver}/lib64/driver");
    println!("cargo:rustc-link-lib=dcmi");
    println!("cargo:rerun-if-env-changed=ASCEND_TOOLKIT_HOME");
    println!("cargo:rerun-if-env-changed=ASCEND_DRIVER_HOME");
}
