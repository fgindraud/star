use core::pin::Pin;
use std::rc::{Rc, Weak};

/// Weak pointer to Pin<Rc<T>>
pub struct PinWeak<T>(Weak<T>);

impl<T> PinWeak<T> {
    pub fn new() -> PinWeak<T> {
        PinWeak(Weak::new())
    }

    pub fn downgrade(rc: Pin<Rc<T>>) -> Self {
        // SAFETY : will always be restored to a Pin<Rc<T>>
        PinWeak(Rc::downgrade(&unsafe { Pin::into_inner_unchecked(rc) }))
    }

    pub fn upgrade(&self) -> Option<Pin<Rc<T>>> {
        // SAFETY : can only be built using a Pin<Rc<T>>
        self.0.upgrade().map(|rc| unsafe { Pin::new_unchecked(rc) })
    }
}
