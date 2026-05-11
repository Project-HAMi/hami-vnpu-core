pub mod worker;
pub mod manager;
pub mod shmem;
pub mod externed_api;
pub mod config;

use ctor::ctor;

#[ctor]
fn init_logger() {
    let _ = env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .parse_default_env() 
        .try_init();
}

#[macro_export]
macro_rules! check_rts {
    ($call:expr) => {
        {
            let ret = unsafe { 
                $call 
            } as u32; // Cast the result to `u32`.
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
        }
    };
}