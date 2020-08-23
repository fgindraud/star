use crate::intrusive_chain::{Chain, Link};
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::io;
use std::os::unix::io::RawFd;
use std::time::{Duration, Instant};

bitflags::bitflags!(
    /// Possible event types for file descriptors.
    /// These event types are those supported by [`libc::poll`].
    /// See `poll()` syscall manpage for details.
    pub struct FdEventType: libc::c_short {
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
        // Indicates an invalid file descriptor -> removes entries linked to this Fd ?
        const NVAL = libc::POLLNVAL;
    }
);

/// Future which waits until a specific event happens to a file descriptor.
/// This is a minimal interface to handling async IO.
///
/// This can be used as a building block for async IO:
/// ```
/// use std::io;
/// use star::{FdEventType, WaitFdEvent};
/// use std::os::unix::io::AsRawFd;
///
/// async fn async_read<F: io::Read + AsRawFd>(f: &mut F, buf: &mut [u8]) -> io::Result<usize> {
///     loop {
///         match f.read(buf) {
///             Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
///                 WaitFdEvent::new(f.as_raw_fd(), FdEventType::IN | FdEventType::ERR).await
///             }
///             r => break r,
///         }
///     }
/// }
/// ```
pub struct WaitFdEvent(WaitFdEventState);

struct FdEventRegistration {
    fd: RawFd,
    event_type: FdEventType,
    waker: Waker,
}

enum WaitFdEventState {
    Init {
        fd: RawFd,
        event_type: FdEventType,
    },
    Registered {
        registration: Link<FdEventRegistration>,
    },
}

impl WaitFdEvent {
    pub fn new(fd: RawFd, event_type: FdEventType) -> WaitFdEvent {
        WaitFdEvent(WaitFdEventState::Init { fd, event_type })
    }
}

impl Future for WaitFdEvent {
    type Output = ();
    fn poll(self: Pin<&mut Self>, context: &mut Context) -> Poll<()> {
        unimplemented!()
    }
}

/// _Reacts_ to specific events and wake tasks registered on them.
pub struct Reactor {
    /// Array used by [`syscall_poll`].
    /// Placed here so it can be reused between calls, reducing allocations.
    fd_event_storage: Vec<libc::pollfd>,

    registered_fd_events: Chain<FdEventRegistration>,
}

impl Reactor {
    pub fn new() -> Reactor {
        Reactor {
            fd_event_storage: Vec::new(),
            registered_fd_events: Chain::new(),
        }
    }

    /// Non-blocking check for events. Returns the number of waken [`Waker`].
    pub fn poll(&mut self) -> Result<usize, io::Error> {
        unimplemented!()
    }

    /// Blocking wait for events. Returns the number of waken [`Waker`].
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
                None => core::ptr::null(),
                Some(t) => &libc::timespec {
                    tv_sec: t.as_secs() as libc::c_long,
                    tv_nsec: t.subsec_nanos() as libc::c_long,
                },
            },
            core::ptr::null(),
        )
    };
    match return_code {
        -1 => Err(io::Error::last_os_error()),
        n => Ok(n as usize),
    }
}
