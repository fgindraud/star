use crate::reactor::{Event, Reactor};
use crate::utils::{PinCell, PinWeak};
use core::cell::RefCell;
use core::future::Future;
use core::mem::ManuallyDrop;
use core::pin::Pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::collections::VecDeque;
use std::io;
use std::rc::Rc;

/// Task frame. Holds the future (or future's result), and metadata.
///
/// This is allocated once as `Rc<PinCell<TaskFrame<TaskState<F>>>`, and two type erased handles are generated from it:
/// - one `Rc<PinCell<TaskFrame<dyn TaskMakeProgress>>>` for runtime queues
/// - one `Rc<PinCell<TaskFrame<dyn TaskPollJoin<F::Output>>>>` for the `JoinHandle<F::Output>`
///
/// Metadata directly in this struct (not `state`) can be accessed from type erased handles, reducing template use.
struct TaskFrame<S: ?Sized> {
    /// Indicates if the task is already scheduled.
    /// This is used in [`Runtime::schedule_task`] to prevent multiple references in the `ready_queue`.
    in_ready_queue: bool,
    /// [`TaskState`] or type erased versions of it: [`TaskMakeProgress`], [`TaskPollJoin`].
    state: S,
}

/// Holds the future then the future's result.
enum TaskState<F: Future> {
    Running {
        /// [`Waker`] of the task blocked on our [`JoinHandle`].
        wake_on_completion: Option<Waker>,
        /// Pointer to concrete self, used to create a [`Waker`].
        ///
        /// The executor (ready_queue) manipulates `Rc<.. dyn ..>` which are fat pointers.
        /// RawWaker can only store a single pointer, so it cannot fit these.
        ///
        /// Thus the [`Waker`] object of a task is semantically a `Pin<Rc<PinCell<TaskFrame<Self>>>>`.
        /// It cannot be generated from the executor `Rc<.. dyn ..>`.
        /// The solution is to store a copy of the concrete pointer ([`PinWeak`] to avoid Rc loop) inside the task,
        /// and to use this concrete pointer in to generate the [`Waker`] instance.
        self_ptr: PinWeak<PinCell<TaskFrame<Self>>>,
        future: F,
    },
    Completed(Option<F::Output>),
}

/// Pin projected version of TaskFrame.
struct TaskFrameProj<'a, S: ?Sized> {
    in_ready_queue: &'a mut bool,
    state: Pin<&'a mut S>,
}

impl<S: ?Sized> TaskFrame<S> {
    /// Pinning projection of TaskState.
    fn proj<'a>(self: Pin<&'a mut Self>) -> TaskFrameProj<'a, S> {
        // SAFETY: only state is structural
        unsafe {
            let ref_mut = Pin::get_unchecked_mut(self);
            TaskFrameProj {
                in_ready_queue: &mut ref_mut.in_ready_queue,
                state: Pin::new_unchecked(&mut ref_mut.state),
            }
        }
    }
}

/// Pin projected version of TaskState.
enum TaskStateProj<'a, F: Future> {
    Running {
        wake_on_completion: &'a mut Option<Waker>,
        self_ptr: &'a mut PinWeak<PinCell<TaskFrame<TaskState<F>>>>,
        future: Pin<&'a mut F>, // Structural
    },
    Completed(&'a mut Option<F::Output>),
}

impl<F: Future> TaskState<F> {
    /// Pinning projection of TaskState.
    fn proj<'a>(self: Pin<&'a mut Self>) -> TaskStateProj<'a, F> {
        // SAFETY: only the future is structural
        unsafe {
            match Pin::get_unchecked_mut(self) {
                TaskState::Running {
                    wake_on_completion,
                    self_ptr,
                    future,
                } => TaskStateProj::Running {
                    wake_on_completion,
                    self_ptr,
                    future: Pin::new_unchecked(future),
                },
                TaskState::Completed(inner) => TaskStateProj::Completed(inner),
            }
        }
    }
}

impl<F: Future> TaskFrame<TaskState<F>> {
    /// Creates a new task on the heap
    fn new(f: F) -> Pin<Rc<PinCell<TaskFrame<TaskState<F>>>>> {
        let task = Rc::pin(PinCell::new(TaskFrame {
            in_ready_queue: false,
            state: TaskState::Running {
                wake_on_completion: None,
                self_ptr: PinWeak::new(), // Placeholder
                future: f,
            },
        }));
        // Update placeholder
        match task.as_ref().borrow_mut().as_mut().proj().state.proj() {
            TaskStateProj::Running { self_ptr, .. } => *self_ptr = PinWeak::downgrade(task.clone()),
            _ => unreachable!(),
        }
        task
    }
}

/// Internal trait: advance a task state. For type erasure.
trait TaskMakeProgress {
    /// Requires Pinned Self to propagate Pin reference to the Future.
    fn make_progress(self: Pin<&mut Self>);
}

impl<F: Future + 'static> TaskMakeProgress for TaskState<F> {
    fn make_progress(mut self: Pin<&mut Self>) {
        match self.as_mut().proj() {
            TaskStateProj::Completed(_) => panic!("make_progress on completed task"),
            TaskStateProj::Running {
                wake_on_completion,
                self_ptr,
                future,
            } => {
                let waker = TaskFrame::make_waker(self_ptr);
                match future.poll(&mut Context::from_waker(&waker)) {
                    Poll::Pending => (),
                    Poll::Ready(value) => {
                        if let Some(waker) = wake_on_completion.take() {
                            waker.wake()
                        }
                        self.set(TaskState::Completed(Some(value)))
                    }
                }
            }
        }
    }
}

/// Internal trait: test task completion and return output value. For type erasure.
trait TaskPollJoin {
    type Output;

    /// Waker is optional: present for async wait, absent for blocking wait.
    fn poll_join(self: Pin<&mut Self>, waker: Option<&Waker>) -> Poll<Self::Output>;
}

impl<F: Future> TaskPollJoin for TaskState<F> {
    type Output = F::Output;
    fn poll_join(self: Pin<&mut Self>, waker: Option<&Waker>) -> Poll<F::Output> {
        match self.proj() {
            TaskStateProj::Running {
                wake_on_completion, ..
            } => {
                update_waker(wake_on_completion, waker);
                Poll::Pending
            }
            TaskStateProj::Completed(value) => {
                Poll::Ready(value.take().expect("poll_join: value already consumed"))
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
impl<F: Future + 'static> TaskFrame<TaskState<F>> {
    /// Make waker from self_ptr.
    /// The [`RawWaker`] `* const()` ptr is `Pin<Rc<PinCell<Self>>>`: it has ownership of the task.
    fn make_waker(self_ptr: &PinWeak<PinCell<Self>>) -> Waker {
        let self_ptr = self_ptr.upgrade().unwrap();
        unsafe { Waker::from_raw(Self::into_rawwaker(self_ptr)) }
    }

    // Conversion utils
    unsafe fn into_rawwaker(rc: Pin<Rc<PinCell<Self>>>) -> RawWaker {
        RawWaker::new(
            Rc::into_raw(Pin::into_inner_unchecked(rc)) as *const (),
            &Self::RAWWAKER_VTABLE,
        )
    }
    unsafe fn reconstruct_owned(ptr: *const ()) -> Pin<Rc<PinCell<Self>>> {
        Pin::new_unchecked(Rc::from_raw(ptr as *const PinCell<Self>))
    }
    unsafe fn reconstruct_cloned(ptr: *const ()) -> Pin<Rc<PinCell<Self>>> {
        let referenced = ManuallyDrop::new(Self::reconstruct_owned(ptr));
        Pin::clone(&referenced)
    }

    const RAWWAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
        Self::rawwaker_clone,
        Self::rawwaker_wake,
        Self::rawwaker_wake_by_ref,
        Self::rawwaker_drop,
    );
    unsafe fn rawwaker_clone(ptr: *const ()) -> RawWaker {
        Self::into_rawwaker(Self::reconstruct_cloned(ptr))
    }
    unsafe fn rawwaker_wake(ptr: *const ()) {
        Runtime::schedule_task(Self::reconstruct_owned(ptr))
    }
    unsafe fn rawwaker_wake_by_ref(ptr: *const ()) {
        Runtime::schedule_task(Self::reconstruct_cloned(ptr))
    }
    unsafe fn rawwaker_drop(ptr: *const ()) {
        drop(Self::reconstruct_owned(ptr))
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
    ready_tasks: VecDeque<Pin<Rc<PinCell<TaskFrame<dyn TaskMakeProgress>>>>>,
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
    fn schedule_task(task: Pin<Rc<PinCell<TaskFrame<dyn TaskMakeProgress>>>>) {
        let already_in_ready_queue = {
            let mut borrow = task.as_ref().borrow_mut();
            let TaskFrameProj { in_ready_queue, .. } = borrow.as_mut().proj();
            let already_in_ready_queue = *in_ready_queue;
            *in_ready_queue = true;
            already_in_ready_queue
        };
        if !already_in_ready_queue {
            Self::access_mut(|rt| rt.ready_tasks.push_back(task))
        }
    }

    /// Runs the next `n` tasks in the ready queue.
    /// Returns false if we stopped due to an empty queue.
    fn run_ready_task_batch(n: usize) -> bool {
        for _ in 0..n {
            match Self::access_mut(|rt| rt.ready_tasks.pop_front()) {
                Some(task) => {
                    let mut borrow = task.as_ref().borrow_mut();
                    let task_frame = borrow.as_mut().proj();
                    *task_frame.in_ready_queue = false;
                    task_frame.state.make_progress();
                }
                None => return false,
            }
        }
        true
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
            let root_task = Self::spawn(future);
            loop {
                const BATCH_SIZE: usize = 16;
                let more_tasks = Self::run_ready_task_batch(BATCH_SIZE);

                if let Poll::Ready(value) = root_task.poll_join(None) {
                    break Ok(value);
                }

                if more_tasks {
                    continue;
                }

                let waken = Self::access_mut(|rt| rt.reactor.wait())?;
                if waken == 0 {
                    // No more task to run, and no task awoke from a wait : stalled
                    break Err(io::Error::new(io::ErrorKind::Other, "Runtime has stalled"));
                }
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
        let task = TaskFrame::new(future);
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
pub struct JoinHandle<T>(Pin<Rc<PinCell<TaskFrame<dyn TaskPollJoin<Output = T>>>>>);

impl<T> JoinHandle<T> {
    /// Internal helper, just forward to [`TaskPollJoin::poll_join`] buried deep in the struct.
    fn poll_join(&self, waker: Option<&Waker>) -> Poll<T> {
        self.0
            .as_ref()
            .borrow_mut()
            .as_mut()
            .proj()
            .state
            .poll_join(waker)
    }

    /// Test task completion.
    /// If complete, return the task output, consuming the handle.
    /// If not complete, gives back the handle.
    pub fn try_join(self) -> Result<T, Self> {
        match self.poll_join(None) {
            Poll::Ready(value) => Ok(value),
            Poll::Pending => Err(self),
        }
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = T;
    fn poll(self: Pin<&mut Self>, context: &mut Context) -> Poll<T> {
        self.poll_join(Some(context.waker()))
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
