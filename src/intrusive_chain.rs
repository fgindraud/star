//! This module implements a non-owning intrusive chain (list) with a Safe API.
//!
//! To have a safe API, we must prevent aliasing access to values and ensure values are alive during accesses.
//! Contrary to an owning intrusive list, we cannot statically link lifetime of elements to the list head.
//! An example is [`Drop::drop`] which gets `&mut` access, and could happen at any time.
//!
//! Thus the strategy is for each link to act like a [`core::cell::RefCell`], dynamically tracking its reference state.
//! Safety requires panicking during drop if the link was referenced.

use core::cell::{Cell, UnsafeCell};
use core::fmt;
use core::marker::{PhantomData, PhantomPinned};
use core::pin::Pin;
use std::error::Error;

/// Basic link that can **safely** form a doubly-linked circular chain with other pinned instances.
/// Conceptually, this struct owns the _participation in a chain_.
/// It only cares about creating and maintaining the chain pointer structure, and provides no iteration semantics on its own.
///
/// A link starts unlinked (`self.prev == self.next == null`).
/// **After being pinned** it can be placed _in a chain_ (maybe singleton with only self).
/// If in a chain, `self == self.prev.next == self.next.prev`, and all chain members are pinned.
/// It can only be in one chain.
///
/// This struct is [`!Unpin`](Unpin), and all link operations require [`Pin`] references.
/// This guarantuees that pointers to links are stable (no move allowed).
///
/// All inner field are **not pin structural**.
struct RawLink {
    /// `Pin<&RawLink>` if not null.
    prev: Cell<*const RawLink>,
    /// `Pin<&RawLink>` if not null.
    next: Cell<*const RawLink>,
    /// `>0` for shared references, or special values [`UNREFERENCED`] or [`EXCLUSIVE_REFERENCE`].
    references: Cell<i32>,
    link_type: RawLinkType,
    _pin: PhantomPinned,
}

const UNREFERENCED: i32 = 0;
const EXCLUSIVE_REFERENCE: i32 = -1;

enum RawLinkType {
    Chain,
    Link,
}

impl RawLink {
    /// Create a new unlinked raw link.
    fn new(link_type: RawLinkType) -> Self {
        RawLink {
            prev: Cell::new(core::ptr::null()),
            next: Cell::new(core::ptr::null()),
            references: Cell::new(UNREFERENCED),
            link_type,
            _pin: PhantomPinned,
        }
    }

    fn is_linked(&self) -> bool {
        !self.next.get().is_null()
    }

    // TODO replace (linked | unlinked) by (unpinned | singleton | chained) ?
    // TODO determine model to use for insert/unlink (singleton, of unpinned)
    fn is_pinned(&self) -> bool {
        !self.next.get().is_null()
    }
    fn is_singleton(&self) -> bool {
        self.next.get() == self
    }

    fn try_borrow<'l>(&'l self) -> Result<RawLinkBorrow<'l>, BorrowError> {
        RawLinkBorrow::new(self)
    }
    fn borrow<'l>(&'l self) -> RawLinkBorrow<'l> {
        self.try_borrow().unwrap()
    }

    fn try_borrow_mut<'l>(&'l self) -> Result<RawLinkBorrowMut<'l>, BorrowMutError> {
        RawLinkBorrowMut::new(self)
    }
    fn borrow_mut<'l>(&'l self) -> RawLinkBorrowMut<'l> {
        self.try_borrow_mut().unwrap()
    }

    /// If self is linked:
    /// ```text
    /// /--p->-self->-n--\ -> /--p->-n--\ + unlinked self
    /// \-------<--------/    \----<----/
    /// ```
    fn unlink(&self) {
        if self.is_linked() {
            let p_ptr = self.prev.get();
            let n_ptr = self.next.get();
            // if p == n == self: singleton, no need to fix p.next / n.prev
            if p_ptr != self {
                // SAFETY:
                // self in a chain => p & n are pinned raw links in a chain.
                // Set p.next = n & n.prev = p
                unsafe {
                    (*p_ptr).next.set(n_ptr);
                    (*n_ptr).prev.set(p_ptr);
                }
            }
            self.prev.set(core::ptr::null());
            self.next.set(core::ptr::null())
        }
    }

    /// ```text
    /// /--p->-self--\ | unlinked self + unlinked other -> /--p->-other->-self--\
    /// \------<-----/                                     \---------<----------/
    /// ```
    /// Unlinks `other` if it is linked.
    fn insert_prev(self: Pin<&Self>, other: Pin<&Self>) {
        other.unlink();

        let self_ptr: *const RawLink = self.get_ref();
        let other_ptr: *const RawLink = other.get_ref();

        if self.is_linked() {
            let p_ptr = self.prev.get();
            // SAFETY : self in chain => p_ptr is valid and pinned
            unsafe { (*p_ptr).next.set(other_ptr) };
            self.prev.set(other_ptr);
            other.prev.set(p_ptr);
            other.next.set(self_ptr)
        } else {
            self.prev.set(other_ptr);
            self.next.set(other_ptr);
            other.prev.set(self_ptr);
            other.next.set(self_ptr)
        }
    }
}

/// Disconnect from any chain on destruction
/// Part of SAFETY ; being in a chain => pinned => destructor will run before repurposing memory.
/// Thus pointers to self in neighbouring links are valid (removed before memory is repurposed).
///
/// Panics if the link is referenced, as drop is equivalent to getting exclusive access (`&mut`).
impl Drop for RawLink {
    fn drop(&mut self) {
        // Drop has borrowed self mutably.
        // Check with dynamic borrow that it could actually do so before continuing.
        // If not, panic as there is no way to indicate an error of stop drop().
        let _mut_guard = self.borrow_mut();
        self.unlink()
    }
}

/// Shared immutable borrow guard.
///
/// Holds a `+1` value in the reference count sum.
/// In case of overflow, use wraparound which will cause next borrow to fail (no UB).
struct RawLinkBorrow<'l> {
    link: &'l RawLink,
}

impl<'l> RawLinkBorrow<'l> {
    fn new(link: &'l RawLink) -> Result<Self, BorrowError> {
        let ref_count = link.references.get();
        if ref_count >= 0 {
            link.references.set(ref_count.wrapping_add(1));
            Ok(RawLinkBorrow { link })
        } else {
            Err(BorrowError)
        }
    }
}

impl<'l> Drop for RawLinkBorrow<'l> {
    fn drop(&mut self) {
        let ref_count = self.link.references.get();
        self.link.references.set(ref_count.wrapping_sub(1))
    }
}

/// Represents failure to borrow a link (shared immutable borrow).
#[derive(Debug)]
pub struct BorrowError;
impl fmt::Display for BorrowError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        "Cannot borrow link (shared)".fmt(f)
    }
}
impl Error for BorrowError {}

/// Exclusive mutable borrow guard.
///
/// Only one can exist at anytime.
struct RawLinkBorrowMut<'l> {
    link: &'l RawLink,
}

impl<'l> RawLinkBorrowMut<'l> {
    fn new(link: &'l RawLink) -> Result<Self, BorrowMutError> {
        if link.references.get() == UNREFERENCED {
            link.references.set(EXCLUSIVE_REFERENCE);
            Ok(RawLinkBorrowMut { link })
        } else {
            Err(BorrowMutError)
        }
    }
}

impl<'l> Drop for RawLinkBorrowMut<'l> {
    fn drop(&mut self) {
        self.link.references.set(UNREFERENCED)
    }
}

/// Represents failure to borrow a link (exclusive mutable borrow).
#[derive(Debug)]
pub struct BorrowMutError;
impl fmt::Display for BorrowMutError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        "Cannot borrow link (exclusive)".fmt(f)
    }
}
impl Error for BorrowMutError {}

/// A value that can be threaded in a chain.
/// Each [`Link`] can be in only one [`Chain`] at a time, and only once.
#[repr(C)]
pub struct Link<T: ?Sized> {
    /// RawLink is first element with repr(C) to allow static casting between raw <-> self
    raw: RawLink,
    value: UnsafeCell<T>,
}

impl<T> Link<T> {
    pub fn new(value: T) -> Link<T> {
        Link {
            raw: RawLink::new(RawLinkType::Link),
            value: UnsafeCell::new(value),
        }
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
            raw: RawLink::new(RawLinkType::Chain),
            _marker: PhantomData,
        }
    }

    pub fn push_front(chain: Pin<&mut Self>, link: Pin<&mut Link<T>>) {
        unimplemented!()
    }
}

#[test]
fn test_raw() {
    assert_eq!(
        core::mem::size_of::<RawLink>(),
        3 * core::mem::size_of::<*const ()>()
    );
    let link0 = Box::pin(RawLink::new(RawLinkType::Link));
    let link1 = Box::pin(RawLink::new(RawLinkType::Link));
    assert!(!link0.is_linked());
    assert!(!link1.is_linked());
    link0.as_ref().insert_prev(link1.as_ref());
    assert!(link0.is_linked());
    assert!(link1.is_linked());
    link1.as_ref().unlink();
    assert!(link0.is_linked());
    assert!(!link1.is_linked());
    link1.as_ref().insert_prev(link0.as_ref());
    assert!(link0.is_linked());
    assert!(link1.is_linked());
    drop(link0);
    assert!(link1.is_linked());
}
