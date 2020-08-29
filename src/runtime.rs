use crate::executor::{spawn, Executor};
use crate::reactor::Reactor;
use crate::utils::make_noop_waker;
use core::cell::{Cell, RefCell};
use core::future::Future;
use core::pin::Pin;
use std::io;
use std::task::{Context, Poll};

/// Main runtime structure.
/// It is stored as an implicit thread_local, so it is only used through static methods.
pub struct Runtime {
    enabled: Cell<bool>,
    executor: RefCell<Executor>,
    /// Reactor needs pinning which is not guaranteed by thread_local, so box it.
    reactor: RefCell<Pin<Box<Reactor>>>,
}

impl Runtime {
    fn new() -> Self {
        Runtime {
            enabled: Cell::new(false),
            executor: RefCell::new(Executor::new()),
            reactor: RefCell::new(Box::pin(Reactor::new())),
        }
    }

    thread_local!(
        /// Use a thread local instance for simplicity of access to the runtime.
        /// This avoid having to store Rc<Runtime> in task futures.
        /// This limits to one Runtime per thread, but only one can run a time anyway due to block_on.
        static RUNTIME: Runtime = Runtime::new();
    );

    /// Enables the thread_local runtime instance only during f.
    fn with_global_enabled<F, R>(f: F) -> R
    where
        F: FnOnce() -> R,
    {
        Self::RUNTIME.with(|rt| {
            if rt.enabled.get() {
                panic!("global runtime already enabled")
            }
            rt.enabled.set(true);

            let r = f();

            if !rt.enabled.get() {
                panic!("global runtime already disabled")
            }
            // Cleanup leftover tasks and registrations
            rt.executor.replace(Executor::new());
            rt.reactor.borrow_mut().set(Reactor::new());
            rt.enabled.set(false);

            r
        })
    }

    /// Runs `f` with mutable access to the global executor.
    pub fn with_global_executor<F, R>(f: F) -> R
    where
        F: FnOnce(&mut Executor) -> R,
    {
        Self::RUNTIME.with(move |rt| {
            if !rt.enabled.get() {
                panic!("Runtime is disabled")
            }
            f(&mut rt
                .executor
                .try_borrow_mut()
                .expect("Nested with_global_executor"))
        })
    }

    /// Runs 'f' with mutable access to the global reactor
    pub fn with_global_reactor<F, R>(f: F) -> R
    where
        F: FnOnce(Pin<&mut Reactor>) -> R,
    {
        Self::RUNTIME.with(move |rt| {
            if !rt.enabled.get() {
                panic!("Runtime is disabled")
            }
            f(rt.reactor
                .try_borrow_mut()
                .expect("Nested with_global_reactor")
                .as_mut())
        })
    }
}

/// Runs `n` tasks from the global runtime.
/// Returns true if there are more tasks to run.
fn run_ready_tasks_batch_global_runtime(n: usize) -> bool {
    for _ in 0..n {
        if !Executor::run_next_ready_task() {
            return false;
        }
    }
    true
}

/// Runs until `future` finishes, and return its value.
///
/// Must not be called inside itself, or it will panic:
/// ```should_panic
/// star::block_on(async {
///     star::block_on(async {}); // panics !
/// });
/// ```
pub fn block_on<F: Future + 'static>(future: F) -> Result<F::Output, io::Error> {
    Runtime::with_global_enabled(move || {
        let mut root_task = spawn(future);

        let noop_waker = make_noop_waker();
        let mut noop_context = Context::from_waker(&noop_waker);

        loop {
            let more_tasks = run_ready_tasks_batch_global_runtime(16);

            if let Poll::Ready(value) = Pin::new(&mut root_task).poll(&mut noop_context) {
                break Ok(value);
            }

            if more_tasks {
                // Check for events to balance computation VS event driven tasks
                Runtime::with_global_reactor(|reactor| reactor.poll())?;
            } else {
                let waken = Runtime::with_global_reactor(|reactor| reactor.wait())?;
                if waken == 0 {
                    // No more task to run, and no task awoke from a wait : stalled
                    break Err(io::Error::new(io::ErrorKind::Other, "Runtime has stalled"));
                }
            }
        }
    })
}
