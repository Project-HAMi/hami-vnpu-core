pub mod config;
pub mod dcmi;
pub mod dcmi_protocol;
pub mod externed_api;
pub mod manager;
pub mod registry_lock;
pub mod shmem;
pub mod supervisor;
pub mod worker;

use ctor::ctor;
use std::sync::atomic::{AtomicU8, Ordering};

// No OnceLock/environment mutex: a dependency's constructor can call a hook
// before our own constructor runs. The mode is immutable for this exec image.
static DCMI_HELPER_PROCESS: AtomicU8 = AtomicU8::new(0);

pub fn is_dcmi_helper() -> bool {
    let mut mode = DCMI_HELPER_PROCESS.load(Ordering::Acquire);
    if mode == 0 {
        let helper = unsafe {
            let value = libc::getenv(c"NPU_DCMI_HELPER_PROCESS".as_ptr());
            !value.is_null() && *value == b'1' as libc::c_char && *value.add(1) == 0
        };
        mode = if helper { 2 } else { 1 };
        DCMI_HELPER_PROCESS.store(mode, Ordering::Release);
    }
    mode == 2
}

#[ctor]
fn init_logger() {
    dcmi::capture_load_directory();
    let _ = env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .parse_default_env()
        .try_init();
}

#[macro_export]
macro_rules! check_rts {
    ($call:expr) => {{
        let ret = unsafe { $call } as u32; // Cast the result to `u32`.
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
