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
use core::ptr::NonNull;
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

    fn try_borrow(self: Pin<&Self>) -> Result<RawLinkBorrow, BorrowError> {
        RawLinkBorrow::new(self)
    }
    fn borrow(self: Pin<&Self>) -> RawLinkBorrow {
        self.try_borrow().unwrap()
    }

    fn increment_ref_count(&self) {
        self.references.set(self.references.get().wrapping_add(1))
    }
    fn decrement_ref_count(&self) {
        self.references.set(self.references.get().wrapping_sub(1))
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
        // This is only allowed if the link is not referenced.
        // Otherwise panic as there is no way to indicate an error of stop drop().
        if self.references.get() != UNREFERENCED {
            panic!("Drop on referenced RawLink")
        }
        self.unlink()
    }
}

/// Shared immutable borrow guard for a [`RawLink`].
///
/// Holds a `+1` value in the reference count sum.
/// In case of overflow, use wrap-around which will cause next borrow to fail (no UB).
///
/// This guard has no lifetime linking it to the `RawLink`.
/// But it guarantees that the `RawLink` exists as long as the guard exists, due to:
/// 1. The `RawLink` destructor must run before destruction, as `new` takes a pinned `RawLink`.
/// 2. `RawLink` destructor panics if references still exist.
struct RawLinkBorrow {
    link: NonNull<RawLink>,
}

impl RawLinkBorrow {
    fn new(link: Pin<&RawLink>) -> Result<Self, BorrowError> {
        let link = link.get_ref();
        let ref_count = link.references.get();
        if ref_count >= 0 {
            link.increment_ref_count();
            Ok(RawLinkBorrow { link: link.into() })
        } else {
            Err(BorrowError)
        }
    }

    fn link(&self) -> Pin<&RawLink> {
        // SAFETY : link is alive due to reference count preventing, and pinned due to new().
        unsafe { Pin::new_unchecked(self.link.as_ref()) }
    }

    fn next(&self) -> Result<Self, BorrowError> {
        let link = self.link();
        if link.is_linked() {
            // SAFETY: linked, so `next` points to valid pinned raw_link
            RawLinkBorrow::new(unsafe { Pin::new_unchecked(&*link.next.get()) })
        } else {
            Err(BorrowError)
        }
    }

    fn prev(&self) -> Result<Self, BorrowError> {
        let link = self.link();
        if link.is_linked() {
            // SAFETY: linked, so `prev` points to valid pinned raw_link
            RawLinkBorrow::new(unsafe { Pin::new_unchecked(&*link.prev.get()) })
        } else {
            Err(BorrowError)
        }
    }
}

impl Clone for RawLinkBorrow {
    fn clone(&self) -> Self {
        self.link().increment_ref_count();
        RawLinkBorrow { link: self.link }
    }
}

impl Drop for RawLinkBorrow {
    fn drop(&mut self) {
        self.link().decrement_ref_count()
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

/// Exclusive mutable borrow guard for a [`RawLink`].
///
/// Only one can exist at anytime.
/// TODO: model is upgrading from a shared borrow, checking ref_count == 1.
struct RawLinkBorrowMut<'b> {
    borrow: &'b mut RawLinkBorrow,
}

impl<'b> RawLinkBorrowMut<'b> {
    fn new(borrow: &'b mut RawLinkBorrow) -> Result<Self, BorrowMutError> {
        let link = borrow.link();
        if link.references.get() == 1 {
            link.references.set(EXCLUSIVE_REFERENCE);
            Ok(RawLinkBorrowMut { borrow })
        } else {
            Err(BorrowMutError)
        }
    }
}

impl<'b> Drop for RawLinkBorrowMut<'b> {
    fn drop(&mut self) {
        self.borrow.link().references.set(1)
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

    fn pinned_raw(self: Pin<&Self>) -> Pin<&RawLink> {
        // SAFETY : raw is pin-structural
        unsafe { Pin::new_unchecked(&self.get_ref().raw) }
    }

    pub fn try_borrow(self: Pin<&Self>) -> Result<LinkBorrow<T>, BorrowError> {
        Ok(LinkBorrow {
            raw_guard: RawLinkBorrow::new(self.pinned_raw())?,
            _marker: PhantomData,
        })
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

    fn pinned_raw(self: Pin<&Self>) -> Pin<&RawLink> {
        // SAFETY : raw is pin-structural
        unsafe { Pin::new_unchecked(&self.get_ref().raw) }
    }

    pub fn push_front(chain: Pin<&Self>, link: Pin<&Link<T>>) {
        unimplemented!()
    }

    pub fn try_borrow_front(self: Pin<&Self>) -> Result<Option<LinkBorrow<T>>, BorrowError> {
        let raw_guard = RawLinkBorrow::new(self.pinned_raw())?.next()?;
        Ok(match raw_guard.link().link_type {
            RawLinkType::Chain => None,
            RawLinkType::Link => Some(unsafe { LinkBorrow::new(raw_guard) }),
        })
    }

    pub fn try_borrow_back(self: Pin<&Self>) -> Result<Option<LinkBorrow<T>>, BorrowError> {
        let raw_guard = RawLinkBorrow::new(self.pinned_raw())?.prev()?;
        Ok(match raw_guard.link().link_type {
            RawLinkType::Chain => None,
            RawLinkType::Link => Some(unsafe { LinkBorrow::new(raw_guard) }),
        })
    }
}

/// Represents a shared borrow of a [`Link`].
pub struct LinkBorrow<T> {
    raw_guard: RawLinkBorrow,
    _marker: PhantomData<*const T>,
}

impl<T> LinkBorrow<T> {
    /// Upgrade a [`RawLinkBorrow`] to a [`LinkBorrow`].
    /// Safety : the raw link must be one from a `Link<T>`.
    unsafe fn new(raw_guard: RawLinkBorrow) -> Self {
        LinkBorrow {
            raw_guard,
            _marker: PhantomData,
        }
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
