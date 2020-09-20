use crate::intrusive_chain::{Chain, Link};
use crate::runtime::Runtime;
use pin_project::pin_project;
use std::cmp::max;
use std::convert::TryFrom;
use std::future::Future;
use std::io;
use std::os::unix::io::RawFd;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
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
                    Registered { registration } => Runtime::with_global_reactor(move |reactor| {
                        reactor
                            .project()
                            .fd_event_registrations
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
                        Registered { registration } => Runtime::with_global_reactor(|reactor| {
                            let registrations = reactor.project().time_registrations;
                            insert_sorted_by_key(
                                registrations.as_ref(),
                                registration.as_ref(),
                                |r| r.when,
                            )
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

fn insert_sorted_by_key<T, F, K>(chain: Pin<&Chain<T>>, link: Pin<&Link<T>>, mut f: F)
where
    F: FnMut(&T) -> K,
    K: Ord,
{
    let link_key = f(link.value());
    for l in chain.iter() {
        if link_key <= f(l.link().value()) {
            l.link().insert_prev(link);
            return;
        }
    }
    chain.push_back(link)
}

/// _Reacts_ to specific events and wake tasks registered on them.
#[pin_project]
pub struct Reactor {
    /// List of registered events not already triggered.
    #[pin]
    fd_event_registrations: Chain<FdEventRegistration>,
    /// List of time events not already triggered, sorted by increasing [`Instant`] values.
    #[pin]
    time_registrations: Chain<TimeRegistration>,
    /// Array used by [`syscall_poll`].
    /// Placed here so it can be reused between calls, reducing allocations.
    pollfds_storage: Vec<libc::pollfd>,
}

impl Reactor {
    pub fn new() -> Reactor {
        Reactor {
            fd_event_registrations: Chain::new(),
            time_registrations: Chain::new(),
            pollfds_storage: Vec::new(),
        }
    }

    // FIXME handle interrupts ? or in runtime loop ?

    /// Non-blocking check for events. Returns the number of waken [`Waker`].
    pub fn poll(self: Pin<&mut Self>) -> Result<usize, io::Error> {
        // Call poll with timeout = 0 so that it returns immediately
        self.call_poll_and_wake(Some(Duration::new(0, 0)))
    }

    /// Blocking wait for events. Returns the number of waken [`Waker`].
    pub fn wait(self: Pin<&mut Self>) -> Result<usize, io::Error> {
        let timeout =
            self.as_ref()
                .project_ref()
                .time_registrations
                .front()
                .map(|soonest_registration| {
                    max(
                        soonest_registration.link().value().when - Instant::now(),
                        Duration::new(0, 0),
                    )
                });
        self.call_poll_and_wake(timeout)
    }

    fn call_poll_and_wake(
        self: Pin<&mut Self>,
        timeout: Option<Duration>,
    ) -> Result<usize, io::Error> {
        let self_proj = self.project();

        let n_fd_events = {
            let pollfds = self_proj.pollfds_storage;
            let registrations = self_proj.fd_event_registrations;

            // Setup libc::pollfd buffer
            pollfds.clear();
            for registration in registrations.iter() {
                let value = registration.link().get_ref().value();
                pollfds.push(libc::pollfd {
                    fd: value.fd,
                    events: value.event_type.bits(),
                    revents: FdEventType::empty().bits(),
                })
            }

            // Call poll() only if needed (fds, or timeout)
            if !pollfds.is_empty() || timeout != Some(Duration::new(0, 0)) {
                if syscall_poll(pollfds, timeout)? > 0 {
                    // Check pollfds for triggered registrations
                    Iterator::zip(registrations.iter(), pollfds.iter())
                        .filter(|(_, pollfd)| {
                            let events = FdEventType::from_bits_truncate(pollfd.events);
                            let revents = FdEventType::from_bits_truncate(pollfd.revents);
                            events.intersects(revents)
                        })
                        .map(|(registration, _)| {
                            let registration = registration.link();
                            registration.unlink();
                            registration.value().waker.wake_by_ref();
                        })
                        .count()
                } else {
                    0
                }
            } else {
                0
            }
        };

        let n_time_events = {
            let registrations = self_proj.time_registrations.iter();
            if registrations.peek().is_some() {
                // Check for triggered time registrations
                let now = Instant::now();
                registrations
                    .take_while(|registration| registration.link().value().when <= now)
                    .map(|registration| {
                        let registration = registration.link();
                        registration.unlink();
                        registration.value().waker.wake_by_ref();
                    })
                    .count()
            } else {
                0
            }
        };

        Ok(n_fd_events + n_time_events)
    }
}

/// Safe wrapper around [`libc::ppoll()`] syscall.
///
/// `None` timeout means infinite.
fn syscall_poll(fds: &mut [libc::pollfd], timeout: Option<Duration>) -> Result<usize, io::Error> {
    let into_invalid_input = |_| io::Error::from(io::ErrorKind::InvalidInput);

    let return_code = unsafe {
        libc::ppoll(
            fds.as_mut_ptr(),
            libc::nfds_t::try_from(fds.len()).map_err(into_invalid_input)?,
            match timeout {
                None => std::ptr::null(),
                Some(t) => &libc::timespec {
                    tv_sec: libc::c_long::try_from(t.as_secs()).map_err(into_invalid_input)?,
                    tv_nsec: libc::c_long::from(t.subsec_nanos()),
                },
            },
            std::ptr::null(),
        )
    };
    match return_code {
        -1 => Err(io::Error::last_os_error()),
        n => Ok(usize::try_from(n).unwrap()),
    }
}

/// Utility function : sets `fd` to _non blocking_ mode.
///
/// Non blocking mode is required for using [`WaitFdEvent`] future correctly on a file descriptor.
pub fn set_nonblocking(fd: RawFd) -> Result<(), io::Error> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    if flags & libc::O_NONBLOCK == 0 {
        let flags = flags | libc::O_NONBLOCK;
        if unsafe { libc::fcntl(fd, libc::F_SETFL, flags) } == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
