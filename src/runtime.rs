use crate::reactor::{Event, Reactor};
use crate::utils::PinWeak;
use core::cell::RefCell;
use core::future::Future;
use core::mem::ManuallyDrop;
use core::pin::Pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::collections::VecDeque;
use std::io;
use std::rc::Rc;

/// Task frame ; holds the future then the future's result.
/// This is allocated once in an Rc, and two handles are generated from it:
/// - one `Rc<dyn TaskMakeProgress>` for runtime queues
/// - one `Rc<dyn TaskPollJoin<F::Output>>` for the `JoinHandle<F::Output>`
enum TaskState<F: Future> {
    Running {
        /// Future ; SAFETY: this is the only pin-structural element
        future: F,
        /// [`Waker`] of the task blocked on our [`JoinHandle`].
        wake_on_completion: Option<Waker>,
        /// `C++`-like `shared_from_this` weak pointer to self.
        /// This is used to build the task [`Waker`].
        /// Re-scheduling the task with the waker requires a `Rc<dyn TaskMakeProgress>`.
        /// `Rc<dyn TaskMakeProgress>` is a fat pointer, not convertible to *const() for RawWaker.
        /// Using this self_ptr, we can recreate the Rc to the concrete Task and then generate the Rc<dyn TaskMakeProgress>.
        self_ptr: PinWeak<RefCell<TaskState<F>>>,
        /// Indicates if the task is already scheduled.
        /// This is used in [`Runtime::schedule_task`] to prevent multiple references in the `ready_queue`.
        in_ready_queue: bool,
    },
    Completed(Option<F::Output>),
}

impl<F: Future + 'static> TaskState<F> {
    /// Creates a new task on the heap, setting up the Weak self_ptr
    fn new(f: F) -> Pin<Rc<RefCell<TaskState<F>>>> {
        let task = Rc::pin(RefCell::new(TaskState::Running {
            future: f,
            wake_on_completion: None,
            self_ptr: PinWeak::new(),
            in_ready_queue: false,
        }));
        // Update the shared_from_this pointer to the pinned rc location
        match &mut *task.borrow_mut() {
            TaskState::Running { self_ptr, .. } => *self_ptr = PinWeak::downgrade(task.clone()),
            _ => unreachable!(),
        }
        task
    }
}

/// Internal trait: advance a task state with type erasure.
trait TaskMakeProgress {
    /// Requires Pinned Self to propagate Pin reference to the Future.
    fn make_progress(self: Pin<&Self>);
}

impl<F: Future + 'static> TaskMakeProgress for RefCell<TaskState<F>> {
    fn make_progress(self: Pin<&Self>) {
        let mut task_state = self.borrow_mut();
        match &mut *task_state {
            TaskState::Completed(_) => panic!("make_progress on completed task"),
            TaskState::Running {
                future,
                wake_on_completion,
                self_ptr,
                in_ready_queue,
            } => {
                // Not in ready queue anymore
                *in_ready_queue = false;
                // SAFETY : The future is not moved out until destruction when completed
                let future = unsafe { Pin::new_unchecked(future) };
                let self_waker = TaskState::make_waker(self_ptr);
                match future.poll(&mut Context::from_waker(&self_waker)) {
                    Poll::Pending => (),
                    Poll::Ready(value) => {
                        if let Some(waker) = wake_on_completion.take() {
                            waker.wake()
                        }
                        // SAFETY : future is destroyed there
                        *task_state = TaskState::Completed(Some(value))
                    }
                }
            }
        }
    }
}

/// Internal trait: test task completion and return output value with type erasure.
trait TaskPollJoin {
    /// Return type of the task future
    type Output;

    /// Waker is optional: present for async wait, absent for blocking wait.
    fn poll_join(&self, waker: Option<&Waker>) -> Poll<Self::Output>;
}

impl<F: Future> TaskPollJoin for RefCell<TaskState<F>> {
    type Output = F::Output;
    fn poll_join(&self, waker: Option<&Waker>) -> Poll<F::Output> {
        match &mut *self.borrow_mut() {
            TaskState::Running {
                wake_on_completion, ..
            } => {
                update_waker(wake_on_completion, waker);
                Poll::Pending
            }
            TaskState::Completed(value) => {
                Poll::Ready(value.take().expect("try_join: value already consumed"))
            }
        }
    }
}

/// Replace stored waker, only if not waking the same task
fn update_waker(stored: &mut Option<Waker>, replacement: Option<&Waker>) {
    match replacement {
        Some(replacement) => match stored {
            Some(stored) => {
                if !replacement.will_wake(stored) {
                    *stored = replacement.clone()
                }
            }
            None => *stored = Some(replacement.clone()),
        },
        None => *stored = None,
    }
}

// Waker for task
impl<F: Future + 'static> TaskState<F> {
    /// Make waker from self_ptr.
    /// The [`RawWaker`] `* const()` ptr is `Pin<Rc<RefCell<Self>>>`.
    /// RawWaker thus has ownership of the task as one Rc handle.
    fn make_waker(self_ptr: &PinWeak<RefCell<Self>>) -> Waker {
        let self_ptr = self_ptr.upgrade().unwrap();
        unsafe {
            Waker::from_raw(RawWaker::new(
                Rc::into_raw(Pin::into_inner_unchecked(self_ptr)) as *const (),
                &Self::RAWWAKER_VTABLE,
            ))
        }
    }

    const RAWWAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
        Self::rawwaker_clone,
        Self::rawwaker_wake,
        Self::rawwaker_wake_by_ref,
        Self::rawwaker_drop,
    );
    unsafe fn rawwaker_clone(ptr: *const ()) -> RawWaker {
        let self_ptr = ManuallyDrop::new(Pin::new_unchecked(Rc::from_raw(
            ptr as *const RefCell<Self>,
        )));
        RawWaker::new(
            Rc::into_raw(Pin::into_inner_unchecked(Pin::clone(&self_ptr))) as *const (),
            &Self::RAWWAKER_VTABLE,
        )
    }

    unsafe fn rawwaker_wake(ptr: *const ()) {
        let self_ptr = Pin::new_unchecked(Rc::from_raw(ptr as *const RefCell<Self>));
        Runtime::schedule_task(self_ptr)
    }

    unsafe fn rawwaker_wake_by_ref(ptr: *const ()) {
        let self_ptr = ManuallyDrop::new(Pin::new_unchecked(Rc::from_raw(
            ptr as *const RefCell<Self>,
        )));
        Runtime::schedule_task(Pin::clone(&self_ptr))
    }

    unsafe fn rawwaker_drop(ptr: *const ()) {
        drop(Pin::new_unchecked(Rc::from_raw(
            ptr as *const RefCell<Self>,
        )))
    }
}

/// Main runtime structure.
///
/// It is stored as an implicit thread_local, so it is only used through static methods:
/// - [`Runtime::spawn`]
/// - [`Runtime::block_on`]
///
/// See the [`crate`] root page for examples.
pub struct Runtime {
    ready_tasks: VecDeque<Pin<Rc<dyn TaskMakeProgress>>>,
    reactor: Reactor,
}

// Internal stuff
impl Runtime {
    /// Creates a new runtime. Not public as the runtime is accessed through thread_local instance.
    fn new() -> Self {
        Runtime {
            ready_tasks: VecDeque::new(),
            reactor: Reactor::new(),
        }
    }

    thread_local!(
        /// Use a thread local instance for simplicity of access to the runtime.
        /// This avoid having to store Rc<Runtime> in task futures.
        /// This limits to one Runtime per thread, but only one can run a time anyway due to block_on.
        /// Runtime is only started (Some) during [`block_on()`].
        static INSTANCE: RefCell<Option<Runtime>> = RefCell::new(None);
    );

    /// Create the runtime, run f, then destroy the runtime. Not unwind safe.
    fn with_runtime_enabled<F, R>(f: F) -> R
    where
        F: FnOnce() -> R,
    {
        Self::INSTANCE.with(|ref_cell| match ref_cell.replace(Some(Runtime::new())) {
            None => (),
            Some(_) => panic!("runtime already enabled"),
        });
        let r = f();
        Self::INSTANCE.with(|ref_cell| match ref_cell.replace(None) {
            None => panic!("runtime already disabled"),
            Some(_) => (),
        });
        r
    }

    /// Runs `f` with mutable access to the runtime. Panics if runtime is disabled.
    fn access_mut<F, R>(f: F) -> R
    where
        F: FnOnce(&mut Runtime) -> R,
    {
        Self::INSTANCE.with(move |ref_cell| {
            let mut borrow = ref_cell.borrow_mut();
            let runtime = borrow.as_mut().expect("runtime used outside of block_on");
            f(runtime)
        })
    }

    /// Put a task in the `ready_queue` if it is not already there.
    fn schedule_task<F: Future + 'static>(task: Pin<Rc<RefCell<TaskState<F>>>>) {
        let already_in_ready_queue = match &mut *task.borrow_mut() {
            TaskState::Running { in_ready_queue, .. } => std::mem::replace(in_ready_queue, true),
            TaskState::Completed(_) => true, // A completed task should not be scheduled
        };
        if !already_in_ready_queue {
            Self::access_mut(|rt| rt.ready_tasks.push_back(task))
        }
    }
}

// Main public API
impl Runtime {
    /// Runs until `future` finishes, and return its value.
    ///
    /// Must not be called inside itself, or it will panic:
    /// ```should_panic
    /// use star::Runtime;
    /// Runtime::block_on(async {
    ///     Runtime::block_on(async {}); // panics !
    /// });
    /// ```
    pub fn block_on<F: Future + 'static>(future: F) -> Result<F::Output, io::Error> {
        Self::with_runtime_enabled(move || {
            let task = Self::spawn(future);
            // Run
            loop {
                match Self::access_mut(|rt| rt.ready_tasks.pop_front()) {
                    None => break,
                    Some(task) => task.as_ref().make_progress(),
                }
                // TODO reactor and check root task
            }
            // Check task has finished
            match task.0.poll_join(None) {
                Poll::Ready(value) => Ok(value),
                Poll::Pending => Err(io::Error::from(io::ErrorKind::WouldBlock)),
            }
        })
    }

    /// Creates a new task and return a struct representing completion.
    ///
    /// Must be used inside a call to [`Runtime::block_on()`], or it will panic:
    /// ```should_panic
    /// use star::Runtime;
    /// Runtime::spawn(async {}); // panics !
    /// ```
    pub fn spawn<F: Future + 'static>(future: F) -> JoinHandle<F::Output> {
        // TODO optim with boxed future if too large ?
        let task = TaskState::new(future);
        Self::schedule_task(task.clone());
        JoinHandle(task)
    }
}

/// `JoinHandle<T>` represents the completion (and return value) of a spawned Task.
///
/// It implements [`Future`] to support asynchronously waiting for completion:
/// ```
/// use star::Runtime;
/// let f = async {
///     let handle = Runtime::spawn(async { 42 });
///     handle.await
/// };
/// ```
///
/// Completion can be manually tested in a non-blocking way:
/// ```
/// use star::Runtime;
/// Runtime::block_on(async {
///     let handle = Runtime::spawn(async { 42 });
///     let test = handle.try_join();
///     assert!(test.is_err()); // Should not have time to run
/// }).unwrap();
/// ```
pub struct JoinHandle<T>(Pin<Rc<dyn TaskPollJoin<Output = T>>>);

impl<T> JoinHandle<T> {
    /// Test task completion.
    /// If complete, return the task output, consuming the handle.
    /// If not complete, gives back the handle.
    pub fn try_join(self) -> Result<T, Self> {
        match self.0.poll_join(None) {
            Poll::Ready(value) => Ok(value),
            Poll::Pending => Err(self),
        }
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = T;
    fn poll(self: Pin<&mut Self>, context: &mut Context) -> Poll<T> {
        self.0.poll_join(Some(context.waker()))
    }
}

impl Runtime {
    /// Register a [`Waker`] to be waken if the given [`Event`] occurs.
    /// This is mainly used in basic IO / time related [`Future`] implementations.
    /// When a future must wait on a event, it calls this method with it, and returns `Poll::Pending`.
    /// This ensures the task will be rescheduled when the event occurs.
    ///
    /// This panics if not run inside [`Runtime::block_on()`].
    pub fn wake_on_event(event: Event, waker: Waker) {
        Self::access_mut(|rt| rt.reactor.register(event, waker))
    }
}
