use std::io;
use std::os::unix::io::RawFd;
use std::task::Waker;
use std::time::{Duration, Instant};

/// _Reacts_ to specific events and wake tasks registered on them.
pub struct Reactor {
    /// Array used by [`syscall_poll`].
    /// Placed here so it can be reused between calls, reducing allocations.
    fd_event_storage: Vec<libc::pollfd>,
}

/// [`Reactor`] supported event types.
pub enum Event {
    /// Wake when `fd` has any event type in `event`. Used for IO.
    Fd { fd: RawFd, event: FdEvent },
    /// Wake when time reaches the given [`Instant`]. Used for timers.
    Time(Instant),
}

bitflags::bitflags!(
    /// Possible event types for file descriptors.
    /// These event types are those supported by [`libc::poll`].
    /// See `poll()` syscall manpage for details.
    pub struct FdEvent: libc::c_short {
        /// Data can be read
        const IN = libc::POLLIN;
        /// Data can be written
        const OUT = libc::POLLOUT;
        /// An error occurred
        const ERR = libc::POLLERR;
        // Other ones, not documented
        const PRI = libc::POLLPRI;
        const RD_NORM = libc::POLLRDNORM;
        const RD_BAND = libc::POLLRDBAND;
        const WR_NORM = libc::POLLWRNORM;
        const WR_BAND = libc::POLLWRBAND;
        const HUP = libc::POLLHUP;
        const NVAL = libc::POLLNVAL;
    }
);

impl Reactor {
    pub fn new() -> Reactor {
        Reactor {
            fd_event_storage: Vec::new(),
        }
    }

    pub fn register(&mut self, event: Event, waker: Waker) {
        unimplemented!()
    }

    /// Blocking wait for events.
    /// Returns the number of waken [`Waker`].
    pub fn wait(&mut self) -> Result<usize, io::Error> {
        // In case of interruption, retry
        unimplemented!()
    }
}

/// Safe wrapper around [`libc::ppoll()`] syscall
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
