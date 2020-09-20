//! # Single Thread Asynchronous Runtime #
//!
//! This is a simple basic async runtime.
//! Intended usage is for async applications requiring only limited performance.
//! This allows asynchonous IO without the overhead of threads (memory, sync primitives).
//!
//! The runtime itself is implicitely stored in a [`thread_local`] variable.
//! This simplifies usage, avoiding having to store references to it in every future.
//! The runtime is by default disabled and consumes little ressources.
//! It is only _activated_ during [`block_on()`] and _disabled_ afterwards.
//!
//! This runtime supports [`std::future::Future`].
//!
//! **Warning**: the `Future` trait requires using [`std::task::Waker`] which is [`Sync`] and [`Send`].
//! The current implementation of `Waker` uses [`std::rc::Rc`] which is neither of those.
//! As this runtime is single thread only, this is not a problem for normal async code.
//! The only way to break things without panicking is to have two threads running [`block_on()`],
//! extract a `Waker` from one of the future `poll` calls and transplant it in a future in the other runtime.
//! Solution for now : do not do that !
//!
//! # Executor #
//!
//! [`block_on()`] takes an [`std::future::Future`], creates a runtime, and runs the future to completion:
//! ```
//! let value = star::block_on(async { 42 }).expect("sys error");
//! assert_eq!(value, 42);
//! ```
//!
//! Inside [`block_on()`], [`spawn()`] can be used to launch additional tasks:
//! ```
//! let f = async {
//!     let a = star::spawn(async { 21 });
//!     let b = star::spawn(async { 21 });
//!     a.await + b.await
//! };
//! let value = star::block_on(f).expect("sys error");
//! assert_eq!(value, 42);
//! ```
//!
//! # Reactor #
//!
//! Event management is provided through two future types : [`WaitFdEvent`] and [`WaitTime`].
//! These have the minimum required functionnality : they complete when the event they represent has occurred.
//!
//! ```
//! use std::time::{Duration, Instant};
//! let wake_time = Instant::now() + Duration::from_millis(100);
//! star::block_on(star::WaitTime::instant(wake_time)).expect("sys error");
//! assert!(Instant::now() >= wake_time);
//! ```
//!
//! Async-io functions can be created by checking for `EWOULDBLOCK` and requesting a wait.
//!
//! These futures use an intrusive list design that requires no specific allocation for them.

mod intrusive_chain;
mod utils;

/// Executor: executes tasks, stores ready queue, taskframe definitions.
mod executor;

/// React to external events
mod reactor;

/// Overall Runtime
mod runtime;

// Main API entry points
pub use executor::{spawn, JoinHandle};
pub use reactor::{set_nonblocking, FdEventType, WaitFdEvent, WaitTime};
pub use runtime::block_on;
