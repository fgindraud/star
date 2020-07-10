use crate::utils::PinWeak;
use core::cell::RefCell;
use core::future::Future;
use core::mem::ManuallyDrop;
use core::pin::Pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::collections::VecDeque;
use std::io;
use std::rc::{Rc, Weak};

#[derive(Clone)]
pub struct RuntimeHandle(Rc<RefCell<Runtime>>);

struct Runtime {
    ready_tasks: VecDeque<Pin<Rc<dyn TaskMakeProgress>>>,
}

impl RuntimeHandle {
    /// Creates a new runtime
    pub fn new() -> Self {
        Self(Rc::new(RefCell::new(Runtime {
            ready_tasks: VecDeque::new(),
        })))
    }

    /// Runs tasks until the given task finishes, and return its value
    pub fn block_on<F: Future + 'static>(&self, future: F) -> Result<F::Output, io::Error> {
        let task = self.spawn(future);
        // Run
        loop {
            // Written this way to ensure RefCell borrow ends before make_progress()
            let next_task = self.0.borrow_mut().ready_tasks.pop_front();
            match next_task {
                None => break,
                Some(task) => task.as_ref().make_progress(),
            }
        }
        // Check task has finished
        match task.0.join(None) {
            Poll::Ready(value) => Ok(value),
            Poll::Pending => Err(io::Error::from(io::ErrorKind::WouldBlock)),
        }
    }

    /// Creates a new task and return a struct representing completion.
    /// Only used inside async block to spawn new tasks ; to launch the runtime use block_on.
    pub fn spawn<F: Future + 'static>(&self, future: F) -> JoinHandle<F::Output> {
        let task = TaskState::new(future, &self.0);
        self.0.borrow_mut().ready_tasks.push_back(task.clone());
        JoinHandle(task)
    }
}

/// Creating a task returns this JoinHandle, which represents the task completion.
pub struct JoinHandle<T>(Pin<Rc<dyn TaskJoin<Output = T>>>);

/// Handles support asynchronous wait
impl<'r, T> Future for JoinHandle<T> {
    type Output = T;
    fn poll(self: Pin<&mut Self>, context: &mut Context) -> Poll<T> {
        self.0.join(Some(context.waker()))
    }
}

/// Task frame ; holds the future then the future's result.
/// This is allocated once in an Rc.
/// One handle is given to the runtime with the make_progress capability.
/// Another handle is given to the user to get the return value (JoinHandle).
enum TaskState<F: Future> {
    Running {
        // Future is the only pin-structural element
        future: F,
        // Waker of the task blocked on our JoinHandle
        wake_on_completion: Option<Waker>,
        // Information to implement Waker for the task
        // Re-scheduling the task requires handle to runtime + task handle (Rc<dyn TaskMakeProgress>).
        // Rc<dyn...> is a fat pointer, not convertible to *const() for RawWaker.
        // So use the shared_from_this pattern from C++ of storing a weak pointer to the concrete task frame.
        // This allows retrieving a Rc<concrete_task> which is a raw ptr for the Waker.
        // Both pointers are Weak to prevent Rc loop leak.
        runtime: Weak<RefCell<Runtime>>,
        self_ptr: PinWeak<RefCell<TaskState<F>>>,
    },
    Completed(Option<F::Output>),
}

impl<F: Future + 'static> TaskState<F> {
    fn new(f: F, runtime: &Rc<RefCell<Runtime>>) -> Pin<Rc<RefCell<TaskState<F>>>> {
        let task = Rc::pin(RefCell::new(TaskState::Running {
            future: f,
            wake_on_completion: None,
            runtime: Rc::downgrade(runtime),
            self_ptr: PinWeak::new(),
        }));
        // Update the shared_from_this pointer to the pinned rc location
        match &mut *task.borrow_mut() {
            TaskState::Running { self_ptr, .. } => *self_ptr = PinWeak::downgrade(task.clone()),
            _ => unreachable!(),
        }
        task
    }

    fn wake(self_ptr: Pin<Rc<RefCell<Self>>>) {
        let runtime = match &*self_ptr.borrow() {
            TaskState::Running { runtime, .. } => runtime.upgrade().unwrap(),
            _ => unreachable!(),
        };
        runtime.borrow_mut().ready_tasks.push_back(self_ptr);
    }

    /// RawWaker *const() ptr is Pin<Rc<RefCell<Self>>>
    fn make_waker(self_ptr: &PinWeak<RefCell<Self>>) -> Waker {
        let self_ptr = self_ptr.upgrade().unwrap();
        unsafe {
            Waker::from_raw(RawWaker::new(
                Rc::into_raw(Pin::into_inner_unchecked(self_ptr)) as *const (),
                &Self::WAKER_VTABLE,
            ))
        }
    }

    const WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
        Self::clone_fn,
        Self::wake_fn,
        Self::wake_by_ref_fn,
        Self::drop_fn,
    );
    unsafe fn clone_fn(ptr: *const ()) -> RawWaker {
        let self_ptr = ManuallyDrop::new(Pin::new_unchecked(Rc::from_raw(
            ptr as *const RefCell<Self>,
        )));
        RawWaker::new(
            Rc::into_raw(Pin::into_inner_unchecked(Pin::clone(&self_ptr))) as *const (),
            &Self::WAKER_VTABLE,
        )
    }

    unsafe fn wake_fn(ptr: *const ()) {
        let self_ptr = Pin::new_unchecked(Rc::from_raw(ptr as *const RefCell<Self>));
        Self::wake(self_ptr)
    }

    unsafe fn wake_by_ref_fn(ptr: *const ()) {
        let self_ptr = ManuallyDrop::new(Pin::new_unchecked(Rc::from_raw(
            ptr as *const RefCell<Self>,
        )));
        Self::wake(Pin::clone(&self_ptr))
    }

    unsafe fn drop_fn(ptr: *const ()) {
        drop(Pin::new_unchecked(Rc::from_raw(
            ptr as *const RefCell<Self>,
        )))
    }
}

/// Internal trait for the make_progress capability.
/// Used to get a type erased reference to the task for the runtime.
trait TaskMakeProgress {
    fn make_progress(self: Pin<&Self>);
}

impl<F: Future + 'static> TaskMakeProgress for RefCell<TaskState<F>> {
    fn make_progress(self: Pin<&Self>) {
        let mut state = self.borrow_mut();
        match &mut *state {
            TaskState::Completed(_) => panic!("Running completed task"),
            TaskState::Running {
                future,
                wake_on_completion,
                self_ptr,
                ..
            } => {
                // SAFETY : The future is not moved out until destruction when completed
                let future = unsafe { Pin::new_unchecked(future) };
                let self_waker = TaskState::make_waker(self_ptr);
                match future.poll(&mut Context::from_waker(&self_waker)) {
                    Poll::Pending => (),
                    Poll::Ready(value) => {
                        if let Some(waker) = wake_on_completion.take() {
                            waker.wake()
                        }
                        *state = TaskState::Completed(Some(value))
                    }
                }
            }
        }
    }
}

/// Internal trait for the testing task completion and return value extraction.
/// Allows a partially type erased (remove the F, keep the R) reference to the task frame.
/// Waker is optional: present for async wait, absent for blocking wait.
trait TaskJoin {
    type Output;
    fn join(&self, waker: Option<&Waker>) -> Poll<Self::Output>;
}

impl<F: Future> TaskJoin for RefCell<TaskState<F>> {
    type Output = F::Output;
    fn join(&self, waker: Option<&Waker>) -> Poll<F::Output> {
        match &mut *self.borrow_mut() {
            TaskState::Running {
                wake_on_completion, .. // SAFETY : future is not moved
            } => {
                *wake_on_completion = waker.cloned(); // Always update waker
                Poll::Pending
            }
            TaskState::Completed(value) => Poll::Ready(value.take().expect("Double join")),
        }
    }
}

#[test]
fn test() {
    let runtime = RuntimeHandle::new();

    assert_eq!(runtime.block_on(async { 42 }).expect("no sys error"), 42);

    assert_eq!(
        runtime
            .block_on({
                let runtime = runtime.clone();
                async move {
                    let a = runtime.spawn(async { 21 });
                    let b = runtime.spawn(async { 21 });
                    a.await + b.await
                }
            })
            .expect("no sys error"),
        42
    );
}
