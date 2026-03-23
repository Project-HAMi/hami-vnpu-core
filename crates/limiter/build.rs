use std::env;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rustc-link-search=native=/usr/local/Ascend/ascend-toolkit/latest/lib64");
    
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    tonic_build::configure()
        .file_descriptor_set_path(out_dir.join("limiter_descriptor.bin"))
        .compile_protos(&["proto/limiter.proto"], &["proto"])?;
        
    Ok(())
}