use core::time::Duration;
use std::io;

// use std::os::unix::io::RawFd;

/// Safe wrapper around poll() syscall
fn syscall_poll(fds: &mut [libc::pollfd], timeout: Option<Duration>) -> Result<usize, io::Error> {
    let return_code = unsafe {
        libc::ppoll(
            fds.as_mut_ptr(),
            fds.len() as libc::nfds_t,
            match timeout {
                None => std::ptr::null(),
                Some(t) => &libc::timespec {
                    tv_sec: t.as_secs() as libc::c_long,
                    tv_nsec: t.subsec_nanos() as libc::c_long,
                },
            },
            std::ptr::null(),
        )
    };
    match return_code {
        -1 => Err(io::Error::last_os_error()),
        n => Ok(n as usize),
    }
}
