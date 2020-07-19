pub use noop_waker::make_noop_waker;
pub use pin_cell::PinCell;
pub use pin_weak::PinWeak;

mod pin_weak {
    use core::pin::Pin;
    use std::rc::{Rc, Weak};

    /// [`Weak`] pointer to `Pin<Rc<T>>`.
    pub struct PinWeak<T>(Weak<T>);

    impl<T> PinWeak<T> {
        /// Create a new `Pin<Weak<T>>` with no target (upgrade fails).
        pub fn new() -> PinWeak<T> {
            PinWeak(Weak::new())
        }

        /// Creates a `Pin<Weak<T>>` pointing to `rc`.
        pub fn downgrade(rc: Pin<Rc<T>>) -> Self {
            // SAFETY : will always be restored to a Pin<Rc<T>>
            PinWeak(Rc::downgrade(&unsafe { Pin::into_inner_unchecked(rc) }))
        }

        /// Try restoring the `Pin<Rc<T>>` pointer.
        pub fn upgrade(&self) -> Option<Pin<Rc<T>>> {
            // SAFETY : can only be built using a Pin<Rc<T>>
            self.0.upgrade().map(|rc| unsafe { Pin::new_unchecked(rc) })
        }
    }
}

mod pin_cell {
    use core::cell::{BorrowMutError, RefCell, RefMut};
    use core::pin::Pin;

    /// [`RefCell`] that propagates pinning.
    /// To be safe, it is not possible to get a `&mut T`, only `Pin<&mut T>`.
    /// Only implements what is needed in the runtime.
    pub struct PinCell<T: ?Sized>(RefCell<T>);

    impl<T> PinCell<T> {
        pub fn new(value: T) -> PinCell<T> {
            PinCell(RefCell::new(value))
        }
    }

    impl<T: ?Sized> PinCell<T> {
        pub fn try_borrow_mut<'a>(self: Pin<&'a Self>) -> Result<PinRefMut<'a, T>, BorrowMutError> {
            let ref_mut = Pin::get_ref(self).0.try_borrow_mut()?;
            // SAFETY: PinCell does not give access to &mut T, nor does it move the content itself
            Ok(PinRefMut(unsafe { Pin::new_unchecked(ref_mut) }))
        }

        pub fn borrow_mut<'a>(self: Pin<&'a Self>) -> PinRefMut<'a, T> {
            self.try_borrow_mut().expect("already borrowed")
        }
    }

    /// Mut reference guard, only give access to `Pin<&mut T>`
    pub struct PinRefMut<'a, T: ?Sized>(Pin<RefMut<'a, T>>);

    impl<'a, T: ?Sized> PinRefMut<'a, T> {
        pub fn as_mut<'b>(&'b mut self) -> Pin<&'b mut T> {
            self.0.as_mut()
        }
    }

    #[test]
    fn check_types() {
        use core::marker::PhantomPinned;
        struct NotUnpin(i32, PhantomPinned);

        let pc = Box::pin(PinCell::new(NotUnpin(42, PhantomPinned)));
        let mut borrow = pc.as_ref().borrow_mut();
        let _ref_mut: Pin<&mut NotUnpin> = borrow.as_mut();
        // Does not work: let _ref_mut: &mut NotUnpin = &mut *borrow;
    }
}

mod noop_waker {
    use core::task::{RawWaker, RawWakerVTable, Waker};

    /// Waker that does nothing.
    /// Used for calling `poll()` outside of an await context.
    pub fn make_noop_waker() -> Waker {
        unsafe { Waker::from_raw(noop_rawwaker_new()) }
    }

    fn noop_rawwaker_new() -> RawWaker {
        RawWaker::new(core::ptr::null(), &NOOP_WAKER_VTABLE)
    }

    const NOOP_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_ptr| noop_rawwaker_new(), // clone
        |_ptr| (),                  // wake
        |_ptr| (),                  // wake_by_ref
        |_ptr| (),                  // drop
    );
}
