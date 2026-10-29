pub mod config;
pub mod dcmi;
pub mod dcmi_protocol;
pub mod registry_lock;
pub use config::is_dcmi_helper;
pub mod dynamic;
pub mod externed_api;
pub mod manager;
pub mod overdraft;
pub mod shmem;
pub mod supervisor;
pub mod worker;

#[macro_export]
macro_rules! check_rts {
    ($call:expr) => {{
        let ret = $call as u32; // Unsafe calls must be acknowledged at the call site.
        if ret != 0 {
            println!(
                "RTS error: {}, from file '{}', line {} - function: `{}`",
                ret,
                file!(),
                line!(),
                stringify!($call),
            );
        } else {
            // add debug log if necessary
        }
        ret // Return the result (0 in this case) for further use if needed.
    }};
}

// Capture only cwd at load time; logging and threads remain deferred until NPU use.
#[ctor::ctor]
fn capture_load_directory() {
    dcmi::capture_load_directory();
}
