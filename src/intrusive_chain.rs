use core::fmt;
use core::hint::unreachable_unchecked;
use core::marker::{PhantomData, PhantomPinned};
use core::ops::Deref;
use core::pin::Pin;
use core::ptr::NonNull;

/// Basic link that can **safely** form a doubly-linked circular chain with other pinned instances.
/// Conceptually, this struct owns the _participation in a chain_ if linked.
/// It only cares about creating and maintaining the chain pointer structure, and provides no iteration semantics on its own.
///
/// It is either unlinked, or in a chain (maybe singleton with only self).
/// If in a chain: `self == self.prev.next == self.next.prev`.
/// It can only be put in a chain if it is pinned, thus all chain members are pinned.
/// It can only be in one chain.
///
/// This struct is [`Unpin`], and all link operations require [`Pin`] references.
/// This guarantuees that pointers to links are stable (no move allowed).
struct RawLink {
    /// Not pin structural.
    state: RawLinkState,
    _pin: PhantomPinned,
}

enum RawLinkState {
    /// Each pointer is a `Pin<&mut RawLink>`.
    InChain {
        prev: NonNull<RawLink>,
        next: NonNull<RawLink>,
    },
    Unlinked,
}

impl RawLink {
    /// New unlinked raw link.
    fn new() -> Self {
        RawLink {
            state: RawLinkState::Unlinked,
            _pin: PhantomPinned,
        }
    }

    fn is_linked(&self) -> bool {
        match self.state {
            RawLinkState::Unlinked => false,
            _ => true,
        }
    }

    /// If self is linked:
    /// ```text
    /// /--p->-self->-n--\ -> /--p->-n--\ + unlinked self
    /// \-------<--------/    \----<----/
    /// ```
    fn unlink(self: Pin<&mut Self>) {
        unsafe {
            // Inner content is not pin-structural
            let self_mut = self.get_unchecked_mut();
            if let RawLinkState::InChain {
                prev: mut p_ptr,
                next: mut n_ptr,
            } = self_mut.state
            {
                // self in a chain => p & n are pinned raw links in a chain.
                // Set p.next = n & n.prev = p
                // if p == n == self: singleton, no need to change pointers.
                if p_ptr != NonNull::new_unchecked(self_mut) {
                    match &mut p_ptr.as_mut().state {
                        RawLinkState::InChain { next, .. } => *next = n_ptr,
                        _ => unreachable_unchecked(),
                    }
                    match &mut n_ptr.as_mut().state {
                        RawLinkState::InChain { prev, .. } => *prev = p_ptr,
                        _ => unreachable_unchecked(),
                    }
                }
                self_mut.state = RawLinkState::Unlinked;
            }
        }
    }

    /// ```text
    /// /--p->-self--\ | unlinked self + unlinked other -> /--p->-other->-self--\
    /// \------<-----/                                     \---------<----------/
    /// ```
    /// Unlinks `other` if it is linked.
    fn insert_prev(self: Pin<&mut Self>, mut other: Pin<&mut Self>) {
        other.as_mut().unlink();
        // SAFETY: we can chain them as they are pinned.
        unsafe {
            let self_mut = self.get_unchecked_mut();
            let other = other.get_unchecked_mut();
            let self_ptr = NonNull::new_unchecked(self_mut);
            let other_ptr = NonNull::new_unchecked(other);

            match &mut self_mut.state {
                RawLinkState::InChain {
                    prev: self_prev, ..
                } => {
                    // p & self in a chain
                    let mut p_ptr = *self_prev;
                    match &mut p_ptr.as_mut().state {
                        RawLinkState::InChain { next, .. } => *next = other_ptr,
                        _ => unreachable_unchecked(),
                    }
                    *self_prev = other_ptr;
                    other.state = RawLinkState::InChain {
                        prev: p_ptr,
                        next: self_ptr,
                    }
                }
                self_state => {
                    // self unlinked
                    *self_state = RawLinkState::InChain {
                        prev: other_ptr,
                        next: other_ptr,
                    };
                    other.state = RawLinkState::InChain {
                        prev: self_ptr,
                        next: self_ptr,
                    }
                }
            }
        }
    }
}

/// Disconnect from any chain on destruction
/// Part of SAFETY ; being in a chain => pinned => destructor will run before repurposing memory.
/// Thus pointers to self in neighbouring links are valid (removed before memory is repurposed).
impl Drop for RawLink {
    fn drop(&mut self) {
        unsafe { Pin::new_unchecked(self).unlink() }
    }
}

/// A value that can be threaded in a chain.
/// Each [`Link`] can be in only one [`Chain`] at a time, and only once.
#[repr(C)]
pub struct Link<T: ?Sized> {
    /// RawLink is first element with repr(C) to allow static casting between raw <-> self
    raw: RawLink,
    value: T,
}

impl<T> Link<T> {
    pub fn new(value: T) -> Link<T> {
        Link {
            raw: RawLink::new(),
            value,
        }
    }
}

impl<T: ?Sized> Deref for Link<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for Link<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.deref().fmt(f)
    }
}

/// Chain access point.
pub struct Chain<T: ?Sized> {
    /// Has a link like others, but no value
    raw: RawLink,
    _marker: PhantomData<*const T>,
}

impl<T> Chain<T> {
    pub fn new() -> Self {
        Chain {
            raw: RawLink::new(),
            _marker: PhantomData,
        }
    }

    pub fn insert(chain: Pin<&mut Self>, link: Pin<&mut Link<T>>) {
        unimplemented!()
    }
}

#[test]
fn test_raw() {
    assert_eq!(
        core::mem::size_of::<RawLink>(),
        2 * core::mem::size_of::<*const ()>()
    );
    let mut link0 = Box::pin(RawLink::new());
    let mut link1 = Box::pin(RawLink::new());
    assert!(!link0.is_linked());
    assert!(!link1.is_linked());
    link0.as_mut().insert_prev(link1.as_mut());
    assert!(link0.is_linked());
    assert!(link1.is_linked());
    link1.as_mut().unlink();
    assert!(link0.is_linked());
    assert!(!link1.is_linked());
    link1.as_mut().insert_prev(link0.as_mut());
    assert!(link0.is_linked());
    assert!(link1.is_linked());
    drop(link0);
    assert!(link1.is_linked());
}
