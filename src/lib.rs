//! # Single Thread Asynchronous Runtime #
//!
//! This is a simple basic async runtime.
//! Intended usage is for async applications requiring only limited performance.
//! This allows asynchonous IO without the overhead of threads (memory, sync primitives).
//!
//! This runtime supports [`std::future::Future`].
//!
//! # Examples #
//!
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

pub use runtime::{JoinHandle, Runtime};
