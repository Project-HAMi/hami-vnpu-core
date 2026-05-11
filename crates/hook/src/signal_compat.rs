use once_cell::sync::Lazy;

static REAL_SIGACTION: Lazy<extern "C" fn(libc::c_int, *const libc::sigaction, *mut libc::sigaction) -> libc::c_int> = Lazy::new(|| unsafe {
    let ptr = libc::dlsym(libc::RTLD_NEXT, b"sigaction\0".as_ptr() as *const libc::c_char);
    if ptr.is_null() {
        panic!("Cannot find original sigaction function!");
    }
    std::mem::transmute(ptr)
});

// workaround to solve python error
#[unsafe(no_mangle)]
pub extern "C" fn sigaction(
    signum: libc::c_int,
    act: *const libc::sigaction,
    oldact: *mut libc::sigaction,
) -> libc::c_int {
    let ret = REAL_SIGACTION(signum, act, oldact);

    if ret == 0 && !oldact.is_null() {
        unsafe {
            let handler = (*oldact).sa_sigaction;
            if handler != libc::SIG_DFL && handler != libc::SIG_IGN {
                (*oldact).sa_sigaction = libc::SIG_DFL;
            }
        }
    }
    
    ret
}