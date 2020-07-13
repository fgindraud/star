//! # Single Thread Asynchronous Runtime #
//!
//! This is a simple basic async runtime.
//! Intended usage is for async applications requiring only limited performance.
//! This allows asynchonous IO without the overhead of threads (memory, sync primitives).
//!
//! This runtime supports [`std::future::Future`].
//!
//! **Warning**: the `Future` trait requires using [`std::task::Waker`] which is [`Sync`] and [`Send`].
//! The current implementation of `Waker` uses [`std::rc::Rc`] which is neither of those.
//! As this runtime is single thread only, this is not a problem for normal async code.
//! The only way to break things without panicking is to have two threads running [`Runtime::block_on()`],
//! extract a `Waker` from one of the future `poll` calls and transplant it in a future in the other runtime.
//! Solution for now : do not do that !
//!
//! # Examples #
//!
//! [`Runtime::block_on()`] takes an [`std::future::Future`], creates a runtime, and runs the future to completion:
//! ```
//! use star::Runtime;
//! let value = Runtime::block_on(async { 42 }).expect("sys error");
//! assert_eq!(value, 42);
//! ```
//!
//! Inside [`Runtime::block_on()`], [`Runtime::spawn()`] can be used to launch additional tasks:
//! ```
//! use star::Runtime;
//! let f = async {
//!     let a = Runtime::spawn(async { 21 });
//!     let b = Runtime::spawn(async { 21 });
//!     a.await + b.await
//! };
//! let value = Runtime::block_on(f).expect("sys error");
//! assert_eq!(value, 42);
//! ```

mod utils;

/// React to external events
mod reactor;

/// Executor, task definition, overall Runtime
mod runtime;

// TODO reactor
// TODO timer (with cfg flag)
// TODO file IO (with cfg flag)

// Main API entry point
pub use runtime::{JoinHandle, Runtime};
// Exported to allow external future implementations to talk to the reactor (which is private).
pub use reactor::{Event, FdEvent};
