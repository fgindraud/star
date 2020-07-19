use crate::executor::{spawn, Executor};
use crate::reactor::Reactor;
use crate::utils::make_noop_waker;
use core::cell::RefCell;
use core::future::Future;
use core::pin::Pin;
use std::io;
use std::task::{Context, Poll};

/// Main runtime structure.
/// It is stored as an implicit thread_local, so it is only used through static methods.
pub struct Runtime {
    pub executor: Executor,
    pub reactor: Reactor,
}

impl Runtime {
    fn new() -> Self {
        Runtime {
            executor: Executor::new(),
            reactor: Reactor::new(),
        }
    }

    thread_local!(
        /// Use a thread local instance for simplicity of access to the runtime.
        /// This avoid having to store Rc<Runtime> in task futures.
        /// This limits to one Runtime per thread, but only one can run a time anyway due to block_on.
        /// Runtime is only started (Some) during [`block_on()`].
        static RUNTIME: RefCell<Option<Runtime>> = RefCell::new(None);
    );

    /// Creates (enables) the thread_local runtime instance, run f, then destroy the runtime.
    /// Not unwind safe.
    fn with_global_enabled<F, R>(f: F) -> R
    where
        F: FnOnce() -> R,
    {
        Self::RUNTIME.with(|ref_cell| match ref_cell.replace(Some(Runtime::new())) {
            None => (),
            Some(_) => panic!("global runtime already enabled"),
        });
        let r = f();
        Self::RUNTIME.with(|ref_cell| match ref_cell.replace(None) {
            None => panic!("global runtime already disabled"),
            Some(_) => (),
        });
        r
    }

    /// Runs `f` with mutable access to the global runtime instance.
    /// Panics if runtime is disabled or already being accessed (nested call).
    pub fn with_global_mut<F, R>(f: F) -> R
    where
        F: FnOnce(&mut Runtime) -> R,
    {
        Self::RUNTIME.with(move |ref_cell| {
            let mut borrow = ref_cell
                .try_borrow_mut()
                .expect("nested call to with_global_runtime_mut");
            let runtime = borrow
                .as_mut()
                .expect("global runtime used outside of block_on");
            f(runtime)
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
                Runtime::with_global_mut(|rt| rt.reactor.poll())?;
            } else {
                let waken = Runtime::with_global_mut(|rt| rt.reactor.wait())?;
                if waken == 0 {
                    // No more task to run, and no task awoke from a wait : stalled
                    break Err(io::Error::new(io::ErrorKind::Other, "Runtime has stalled"));
                }
            }
        }
    })
}
