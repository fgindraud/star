//! This module implements a non-owning intrusive chain (list) with a Safe API.
//!
//! To have a safe API, we must prevent aliasing access to values and ensure values are alive during accesses.
//! Contrary to an owning intrusive list, we cannot statically link lifetime of elements to the list head.
//! An example is [`Drop::drop`] which gets `&mut` access, and could happen at any time.
//!
//! Thus the strategy is for each link to act like an [`Rc`](std::rc::Rc).
//! A reference counter tracks how many references exist to the link.
//! The [`Drop`] implementation will panic if the link was referenced.
//! Operations alsmost never return _naked_ references ; those who do return a _borrow guard_ ([`RawLinkBorrow`], [`LinkBorrow`]) to ensure the reference count is accurate.
//!
//! The current implementation only supports what is needed by the runtime.
//! It could be extended to provide mutation (with a [`RefCell`](core::cell::RefCell)-like API), iterators, ...
//!
//! Current usage should never have an unbounded number of references to any link.
//! Thus the current API will panic if the reference count overflows.

use core::cell::Cell;
use core::marker::{PhantomData, PhantomPinned};
use core::pin::Pin;
use core::ptr::NonNull;
use pin_project::pin_project;
use std::thread;

/// Basic link that can **safely** form a doubly-linked circular chain with other pinned instances.
/// Conceptually, this struct owns the _participation in a chain_.
///
/// A link starts unlinked (`self.prev == self.next == null`).
/// **After being pinned** it can be placed _in a chain_ (maybe singleton with only self).
/// If in a chain, `self == self.prev.next == self.next.prev`, and all chain members are pinned.
/// It can only be in one chain.
///
/// This struct is [`!Unpin`](Unpin).
/// All operations creating a chain (insert) require [`Pin`] references to ensure stable pointers (move forbidden).
struct RawLink {
    /// `Pin<&RawLink>` if not null.
    prev: Cell<*const RawLink>,
    /// `Pin<&RawLink>` if not null.
    next: Cell<*const RawLink>,
    /// Number of references to the link.
    references: Cell<i32>,
    link_type: RawLinkType,
    _pin: PhantomPinned,
}

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
            references: Cell::new(0),
            link_type,
            _pin: PhantomPinned,
        }
    }

    fn is_linked(&self) -> bool {
        !self.next.get().is_null()
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
/// Panics if the link is referenced.
impl Drop for RawLink {
    fn drop(&mut self) {
        self.unlink();
        // Panic if the link is referenced.
        if self.references.get() > 0 {
            panic!("Drop on referenced RawLink")
        }
    }
}

/// Borrow guard for a [`RawLink`].
/// Holds a `+1` value in the reference count sum.
///
/// This guard has no lifetime linking it to the `RawLink`.
/// But it guarantees that the `RawLink` exists as long as the guard exists, due to:
/// 1. The `RawLink` destructor must run before destruction, as `new` takes a pinned `RawLink`.
/// 2. `RawLink` destructor panics if references still exist.
struct RawLinkBorrow {
    link: NonNull<RawLink>,
}

impl RawLinkBorrow {
    fn new(link: Pin<&RawLink>) -> Self {
        let link = link.get_ref();
        let ref_count = link.references.get();
        if ref_count >= 0 {
            link.increment_ref_count();
            RawLinkBorrow { link: link.into() }
        } else {
            panic!("RawLink reference count overflowed")
        }
    }

    fn link(&self) -> Pin<&RawLink> {
        // SAFETY : link is alive due to reference count preventing, and pinned due to new().
        unsafe { Pin::new_unchecked(self.link.as_ref()) }
    }

    fn next(&self) -> Option<Self> {
        let link = self.link();
        if link.is_linked() {
            // SAFETY: linked, so `next` points to valid pinned raw_link
            let next = unsafe { Pin::new_unchecked(&*link.next.get()) };
            Some(RawLinkBorrow::new(next))
        } else {
            None
        }
    }

    fn prev(&self) -> Option<Self> {
        let link = self.link();
        if link.is_linked() {
            // SAFETY: linked, so `prev` points to valid pinned raw_link
            let prev = unsafe { Pin::new_unchecked(&*link.prev.get()) };
            Some(RawLinkBorrow::new(prev))
        } else {
            None
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
        // When a reference RawLink is dropped, RawLink::drop() will panic.
        // This does not prevent drop of containing structs (like Box), so the RawLink memory may be invalid.
        // Thus to prevent accessing the RawLink memory, do not decrement if in a panic context.
        // Due to the intrusive list stuff being !Sync !Send, this Borrow is in the same thread as the RawLink.
        // A consequence is that the RawLink will be poisoned on panic, as it will be still referenced.
        if !thread::panicking() {
            self.link().decrement_ref_count()
        }
    }
}

/// A value that can be threaded in a chain.
/// Each [`Link`] can be in only one [`Chain`] at a time, and only once.
#[pin_project]
#[repr(C)]
pub struct Link<T: ?Sized> {
    /// RawLink is first element with repr(C) to allow static casting between raw <-> self
    #[pin]
    raw: RawLink,
    #[pin]
    value: T,
}

impl<T> Link<T> {
    pub fn new(value: T) -> Link<T> {
        Link {
            raw: RawLink::new(RawLinkType::Link),
            value,
        }
    }

    /// Access link value.
    /// SAFETY : the borrow on self prevents destruction, either statically, or through a [`LinkBorrow`] which will panic on drop.
    pub fn value(&self) -> &T {
        &self.value
    }

    /// Is the link part of a chain ?
    pub fn is_linked(&self) -> bool {
        self.raw.is_linked()
    }

    /// Remove the link from the chain.
    pub fn unlink(&self) {
        self.raw.unlink()
    }

    /// Borrow the link (and its value) ; allows
    pub fn borrow(self: Pin<&Self>) -> LinkBorrow<T> {
        let raw_guard = RawLinkBorrow::new(self.project_ref().raw);
        unsafe { LinkBorrow::new(raw_guard) }
    }
}

/// Chain head
#[pin_project]
pub struct Chain<T: ?Sized> {
    /// Has a link like others, but no value
    #[pin]
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

    /// Insert the `link` to the back of the chain. Unlinks it beforehand.
    pub fn push_back(self: Pin<&Self>, link: Pin<&Link<T>>) {
        self.project_ref().raw.insert_prev(link.project_ref().raw)
    }

    /// Borrow the first [`Link<T>`](Link), if there is one.
    pub fn borrow_front(self: Pin<&Self>) -> Option<LinkBorrow<T>> {
        let raw_guard = RawLinkBorrow::new(self.project_ref().raw).next()?;
        unsafe { LinkBorrow::new_or_chain(raw_guard) }
    }

    /// Borrow the last [`Link<T>`](Link), if there is one.
    pub fn borrow_back(self: Pin<&Self>) -> Option<LinkBorrow<T>> {
        let raw_guard = RawLinkBorrow::new(self.project_ref().raw).prev()?;
        unsafe { LinkBorrow::new_or_chain(raw_guard) }
    }

    /// Iterator (forward) over the chain.
    pub fn iter(self: Pin<&Self>) -> Iter<T> {
        Iter {
            next_link: self.borrow_front(),
        }
    }
}

/// Represents a borrow of a [`Link`].
/// Will generate a panic if the referenced link is dropped.
///
/// If a panic occurs, this borrow will **not** remove itself from the reference count (required by safety).
/// This makes the Link **poisoned**, in a sense that it will panic on destruction.
/// Thus it is advised to borrow links for the shortest time possible.
pub struct LinkBorrow<T> {
    raw_guard: RawLinkBorrow,
    _marker: PhantomData<*const T>,
}

impl<T> LinkBorrow<T> {
    /// Upgrade a [`RawLinkBorrow`] to a [`LinkBorrow`].
    /// Safety : the raw link must be one from a `Link<T>`, in a chain with only `Link<T>` and `Chain<T>` nodes.
    unsafe fn new(raw_guard: RawLinkBorrow) -> Self {
        LinkBorrow {
            raw_guard,
            _marker: PhantomData,
        }
    }

    /// Upgrade a [`RawLinkBorrow`] to a [`LinkBorrow`] if it is a link.
    /// Safety : the raw link must be from a chain with only `Link<T>` and `Chain<T>` nodes.
    unsafe fn new_or_chain(raw_guard: RawLinkBorrow) -> Option<Self> {
        match raw_guard.link().link_type {
            RawLinkType::Link => Some(LinkBorrow::new(raw_guard)),
            RawLinkType::Chain => None,
        }
    }

    /// Access the referenced [`Link<T>`](Link).
    /// Safe as the link cannot be destroyed (will panic) as long as the borrow exist.
    pub fn link(&self) -> &Link<T> {
        // SAFETY : repr(C) and RawLink first element of Link<T>
        unsafe {
            let raw_p = self.raw_guard.link.as_ref() as *const RawLink;
            &*(raw_p as *const Link<T>)
        }
    }

    pub fn next(&self) -> Option<LinkBorrow<T>> {
        unsafe { LinkBorrow::new_or_chain(self.raw_guard.next()?) }
    }
    pub fn prev(&self) -> Option<LinkBorrow<T>> {
        unsafe { LinkBorrow::new_or_chain(self.raw_guard.prev()?) }
    }
}

/// Iterator over a chain
pub struct Iter<T> {
    next_link: Option<LinkBorrow<T>>,
}

impl<T> Iterator for Iter<T> {
    type Item = LinkBorrow<T>;
    fn next(&mut self) -> Option<LinkBorrow<T>> {
        let next_link = self.next_link.take();
        if let Some(link) = &next_link {
            self.next_link = link.next();
        }
        next_link
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

#[test]
#[should_panic]
fn test_borrow_direct_panic() {
    let link = Box::pin(Link::new(0));
    let _borrow = link.as_ref().borrow();
    drop(link)
}

#[test]
#[should_panic]
fn test_borrow_indirect_panic() {
    let chain = Box::pin(Chain::new());
    let link0 = Box::pin(Link::new(0));
    let link1 = Box::pin(Link::new(1));
    chain.as_ref().push_back(link0.as_ref());
    chain.as_ref().push_back(link1.as_ref());

    let _link0_borrow = chain.as_ref().borrow_front().unwrap();
    drop(link0)
}

#[test]
fn test_chain() {
    let chain = Box::pin(Chain::new());
    let link0 = Box::pin(Link::new(0));
    let link1 = Box::pin(Link::new(1));
    chain.as_ref().push_back(link0.as_ref());
    chain.as_ref().push_back(link1.as_ref());

    let link1_borrow = chain.as_ref().borrow_front().unwrap().next().unwrap();
    assert_eq!(link1_borrow.link().value(), &1);

    let values: Vec<i32> = chain.as_ref().iter().map(|b| *b.link().value()).collect();
    assert_eq!(&values, &[0, 1]);
}
