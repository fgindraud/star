use crate::runtime::Runtime;
use crate::utils::{make_noop_waker, PinCell, PinWeak};
use core::future::Future;
use core::mem::ManuallyDrop;
use core::pin::Pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::collections::VecDeque;
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
    /// This is used in [`Executor::schedule_task`] to prevent multiple references in the `ready_queue`.
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
    fn poll_join(self: Pin<&mut Self>, waker: &Waker) -> Poll<Self::Output>;
}

impl<F: Future> TaskPollJoin for TaskState<F> {
    type Output = F::Output;
    fn poll_join(self: Pin<&mut Self>, waker: &Waker) -> Poll<F::Output> {
        match self.proj() {
            TaskStateProj::Running {
                wake_on_completion, ..
            } => {
                match wake_on_completion {
                    Some(woc) if woc.will_wake(waker) => (),
                    woc => *woc = Some(waker.clone()),
                }
                Poll::Pending
            }
            TaskStateProj::Completed(value) => {
                Poll::Ready(value.take().expect("poll_join: value already consumed"))
            }
        }
    }
}

// Waker for task
impl<F: Future + 'static> TaskFrame<TaskState<F>> {
    /// Make waker from self_ptr.
    /// The [`RawWaker`] `* const()` ptr is `Pin<Rc<PinCell<Self>>>`: it has ownership of the task.
    fn make_waker(self_ptr: &PinWeak<PinCell<Self>>) -> Waker {
        let self_ptr = self_ptr.upgrade().unwrap();
        // SAFETY: Implementation should be ok, except for being !Send & !Sync.
        // A warning of this has been put on the main page.
        // FIXME way to detect violations ?
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
        Executor::schedule_task(Self::reconstruct_owned(ptr))
    }
    unsafe fn rawwaker_wake_by_ref(ptr: *const ()) {
        Executor::schedule_task(Self::reconstruct_cloned(ptr))
    }
    unsafe fn rawwaker_drop(ptr: *const ()) {
        drop(Self::reconstruct_owned(ptr))
    }
}

/// `JoinHandle<T>` represents the completion (and return value) of a spawned Task.
///
/// It implements [`Future`] to support asynchronously waiting for completion:
/// ```
/// let f = async {
///     let handle = star::spawn(async { 42 });
///     handle.await
/// };
/// ```
///
/// Completion can be manually tested in a non-blocking way:
/// ```
/// star::block_on(async {
///     let handle = star::spawn(async { 42 });
///     let test = handle.try_join();
///     assert!(test.is_err()); // Should not have time to run
/// }).unwrap();
/// ```
pub struct JoinHandle<T>(Pin<Rc<PinCell<TaskFrame<dyn TaskPollJoin<Output = T>>>>>);

impl<T> JoinHandle<T> {
    /// Internal helper, just forward to [`TaskPollJoin::poll_join`] buried deep in the struct.
    fn poll_join(&self, waker: &Waker) -> Poll<T> {
        let mut task_borrow = self.0.as_ref().borrow_mut();
        task_borrow.as_mut().proj().state.poll_join(waker)
    }

    /// Test task completion.
    /// If complete, return the task output, consuming the handle.
    /// If not complete, gives back the handle.
    pub fn try_join(self) -> Result<T, Self> {
        match self.poll_join(&make_noop_waker()) {
            Poll::Ready(value) => Ok(value),
            Poll::Pending => Err(self),
        }
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = T;
    fn poll(self: Pin<&mut Self>, context: &mut Context) -> Poll<T> {
        self.poll_join(context.waker())
    }
}

/// Executor: manages the list of ready tasks.
/// Used through the global runtime instance, hence lots of static methods.
pub struct Executor {
    ready_tasks: VecDeque<Pin<Rc<PinCell<TaskFrame<dyn TaskMakeProgress>>>>>,
}

impl Executor {
    pub fn new() -> Executor {
        Executor {
            ready_tasks: VecDeque::new(),
        }
    }

    /// Schedule a task in the global runtime executor.
    /// Put the task in its `ready_queue` if it is not already there.
    fn schedule_task(task: Pin<Rc<PinCell<TaskFrame<dyn TaskMakeProgress>>>>) {
        let already_in_ready_queue = {
            let mut task_borrow = task.as_ref().borrow_mut();
            let in_ready_queue = task_borrow.as_mut().proj().in_ready_queue;
            let already_in_ready_queue = *in_ready_queue;
            *in_ready_queue = true;
            already_in_ready_queue
        };
        if !already_in_ready_queue {
            Runtime::with_global_mut(move |rt| rt.executor.ready_tasks.push_back(task))
        }
    }

    /// Runs the next task in the global runtime executor.
    /// Returns false if there was no task to run.
    pub fn run_next_ready_task() -> bool {
        match Runtime::with_global_mut(|rt| rt.executor.ready_tasks.pop_front()) {
            Some(task) => {
                let mut task_borrow = task.as_ref().borrow_mut();
                let task_frame = task_borrow.as_mut().proj();
                *task_frame.in_ready_queue = false;
                task_frame.state.make_progress();
                true
            }
            None => false,
        }
    }
}

/// Creates a new task and return a struct representing completion.
///
/// Must be used inside a call to [`crate::block_on()`], or it will panic:
/// ```should_panic
/// star::spawn(async {}); // panics !
/// ```
pub fn spawn<F: Future + 'static>(future: F) -> JoinHandle<F::Output> {
    // TODO optim with boxed future if too large ?
    let task = TaskFrame::new(future);
    Executor::schedule_task(task.clone());
    JoinHandle(task)
}
