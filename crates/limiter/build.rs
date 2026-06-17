fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rustc-link-search=native=/usr/local/Ascend/ascend-toolkit/latest/lib64");
    println!("cargo:rustc-link-search=native=/usr/local/Ascend/driver/lib64/driver");
    println!("cargo:rustc-link-lib=dcmi");
    Ok(())
}