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
                // Register and wait
                state.set(WaitFdEventState::Registered {
                    registration: Link::new(FdEventRegistration {
                        fd,
                        event_type,
                        waker: context.waker().clone(),
                    }),
                });
                match state.project() {
                    Registered { registration } => Runtime::with_global_mut(move |rt| {
                        rt.project()
                            .reactor
                            .project()
                            .registered_fd_events
                            .as_ref()
                            .push_back(registration.as_ref())
                    }),
                    _ => unreachable!(),
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

/// Future which waits for a specific time.
/// ```
/// use star::WaitTime;
/// use std::time::Duration;
/// let fut = async {
///     println!("do things");
///     WaitTime::duration(Duration::from_secs(10)).await;
///     println!("do things later");
/// };
/// ```
#[pin_project]
pub struct WaitTime {
    #[pin]
    state: WaitTimeState,
}

struct TimeRegistration {
    when: Instant,
    waker: Waker,
}

#[pin_project(project = WaitTimeStateProj)]
enum WaitTimeState {
    Init {
        when: Instant,
    },
    Registered {
        /// The registration starts linked in the reactor chain.
        /// When triggered and woken by the reactor, it will be unregistered (unlinked).
        #[pin]
        registration: Link<TimeRegistration>,
    },
}

impl WaitTime {
    /// Wait until this specific instant.
    pub fn instant(instant: Instant) -> WaitTime {
        WaitTime {
            state: WaitTimeState::Init { when: instant },
        }
    }

    /// Wait for the given duration, starting now.
    pub fn duration(duration: Duration) -> WaitTime {
        WaitTime::instant(Instant::now() + duration)
    }
}

impl Future for WaitTime {
    type Output = ();
    fn poll(self: Pin<&mut Self>, context: &mut Context) -> Poll<()> {
        use WaitTimeStateProj::*;
        let mut state = self.project().state;
        match state.as_mut().project() {
            Init { when } => {
                let when = *when;
                if Instant::now() >= when {
                    Poll::Ready(())
                } else {
                    // Register and wait
                    state.set(WaitTimeState::Registered {
                        registration: Link::new(TimeRegistration {
                            when,
                            waker: context.waker().clone(),
                        }),
                    });
                    match state.project() {
                        Registered { registration } => Runtime::with_global_mut(|rt| {
                            let time_events = rt.project().reactor.project().registered_time_events;
                            // FIXME insert sorted by Instant
                            time_events.as_ref().push_back(registration.as_ref())
                        }),
                        _ => unreachable!(),
                    }
                    Poll::Pending
                }
            }
            Registered { registration } => {
                if !registration.is_linked() {
                    // Deregistered and woken by the reactor, no need to check time
                    Poll::Ready(())
                } else if Instant::now() >= registration.value().when {
                    // Not yet woken by the reactor, deregister
                    registration.unlink();
                    Poll::Ready(())
                } else {
                    Poll::Pending
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
    /// List of time events not already triggered, sorted by increasing [`Instant`] values.
    #[pin]
    registered_time_events: Chain<TimeRegistration>,
    /// Array used by [`syscall_poll`].
    /// Placed here so it can be reused between calls, reducing allocations.
    fd_event_storage: Vec<libc::pollfd>,
}

impl Reactor {
    pub fn new() -> Reactor {
        Reactor {
            registered_fd_events: Chain::new(),
            registered_time_events: Chain::new(),
            fd_event_storage: Vec::new(),
        }
    }

    /// Non-blocking check for events. Returns the number of waken [`Waker`].
    pub fn poll(self: Pin<&mut Self>) -> Result<usize, io::Error> {
        let self_proj = self.project();
        setup_pollfd_vec(
            self_proj.fd_event_storage,
            self_proj.registered_fd_events.as_ref(),
        );
        if self_proj.fd_event_storage.is_empty() {
            return Ok(0);
        }
        if syscall_poll(self_proj.fd_event_storage, Some(Duration::new(0, 0)))? == 0 {
            return Ok(0);
        }
        let n = wake_triggered_fd_events(
            self_proj.registered_fd_events.as_ref(),
            self_proj.fd_event_storage,
        );
        Ok(n)
    }

    /// Blocking wait for events. Returns the number of waken [`Waker`].
    pub fn wait(self: Pin<&mut Self>) -> Result<usize, io::Error> {
        // In case of interruption, retry
        unimplemented!()
    }
}

fn setup_pollfd_vec(
    pollfds: &mut Vec<libc::pollfd>,
    registrations: Pin<&Chain<FdEventRegistration>>,
) {
    pollfds.clear();
    for registration in registrations.iter() {
        let value = registration.link().value();
        pollfds.push(libc::pollfd {
            fd: value.fd,
            events: value.event_type.bits(),
            revents: FdEventType::empty().bits(),
        })
    }
}

fn wake_triggered_fd_events(
    registrations: Pin<&Chain<FdEventRegistration>>,
    pollfds: &[libc::pollfd],
) -> usize {
    let mut n_waken = 0;
    for (registration, pollfd) in Iterator::zip(registrations.iter(), pollfds.iter()) {
        let events = FdEventType::from_bits_truncate(pollfd.events);
        let revents = FdEventType::from_bits_truncate(pollfd.revents);
        if events.intersects(revents) {
            let link = registration.link();
            link.unlink();
            link.value().waker.wake_by_ref();
            n_waken += 1;
        }
    }
    n_waken
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
