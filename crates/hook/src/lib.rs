use once_cell::sync::Lazy;

use limiter::worker::SchedulerClient;

pub static NPU_LIMITER: Lazy<SchedulerClient> = Lazy::new(SchedulerClient::new);

macro_rules! passthrough {
    ($name:expr, ($($sig:tt)*), $($arg:expr),*) => {
        {
            static REAL: ::once_cell::sync::Lazy<extern "C" fn($($sig)*) -> u64> = 
                ::once_cell::sync::Lazy::new(|| unsafe {
                    let ptr = libc::dlsym(libc::RTLD_NEXT, concat!($name, "\0").as_ptr() as *const libc::c_char);
                    if ptr.is_null() {
                        panic!("cannot find original function: {}", $name);
                    }
                    std::mem::transmute(ptr)
                });
            // println!("in func {:?}", $name);
            (*REAL)($($arg),*)
        }
    };
}

pub(crate) use passthrough;

mod hook;
mod signal_compat;