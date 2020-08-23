use crate::intrusive_chain::{Chain, Link};
use crate::runtime::Runtime;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use pin_project::pin_project;
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
#[pin_project]
pub struct WaitFdEvent {
    /// Hidden enum state
    #[pin]
    state: WaitFdEventState,
}

struct FdEventRegistration {
    fd: RawFd,
    event_type: FdEventType,
    waker: Waker,
}

#[pin_project(project = WaitFdEventStateProj)]
enum WaitFdEventState {
    Init {
        fd: RawFd,
        event_type: FdEventType,
    },
    Registered {
        /// The registration starts linked (in the Reactor chain).
        /// After the event is triggerred, the reactor unlinks the registration.
        #[pin]
        registration: Link<FdEventRegistration>,
    },
}

impl WaitFdEvent {
    pub fn new(fd: RawFd, event_type: FdEventType) -> WaitFdEvent {
        WaitFdEvent {
            state: WaitFdEventState::Init { fd, event_type },
        }
    }
}

impl Future for WaitFdEvent {
    type Output = ();
    fn poll(self: Pin<&mut Self>, context: &mut Context) -> Poll<()> {
        use WaitFdEventStateProj::*;
        let mut state = self.project().state;
        match state.as_mut().project() {
            Init { fd, event_type } => {
                let (fd, event_type) = (*fd, *event_type);
                state.set(WaitFdEventState::Registered {
                    registration: Link::new(FdEventRegistration {
                        fd,
                        event_type,
                        waker: context.waker().clone(),
                    }),
                });
                match state.project() {
                    Init { .. } => unreachable!(),
                    Registered { registration } => Runtime::with_global_mut(move |rt| {
                        rt.project()
                            .reactor
                            .project()
                            .registered_fd_events
                            .as_ref()
                            .push_back(registration.as_ref())
                    }),
                }
                Poll::Pending
            }
            Registered { registration } => {
                // When triggered, the registration will be unlinked.
                if registration.is_linked() {
                    Poll::Pending
                } else {
                    Poll::Ready(())
                }
            }
        }
    }
}

/// _Reacts_ to specific events and wake tasks registered on them.
#[pin_project]
pub struct Reactor {
    /// List of registered events not already triggered.
    #[pin]
    registered_fd_events: Chain<FdEventRegistration>,
    /// Array used by [`syscall_poll`].
    /// Placed here so it can be reused between calls, reducing allocations.
    fd_event_storage: Vec<libc::pollfd>,
}

impl Reactor {
    pub fn new() -> Reactor {
        Reactor {
            registered_fd_events: Chain::new(),
            fd_event_storage: Vec::new(),
        }
    }

    /// Non-blocking check for events. Returns the number of waken [`Waker`].
    pub fn poll(self: Pin<&mut Self>) -> Result<usize, io::Error> {
        let self_proj = self.project();
        self_proj.fd_event_storage.clear();

        let iter = self_proj
            .registered_fd_events
            .as_ref()
            .try_borrow_front()
            .unwrap();

        Ok(0)
    }

    /// Blocking wait for events. Returns the number of waken [`Waker`].
    pub fn wait(self: Pin<&mut Self>) -> Result<usize, io::Error> {
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
