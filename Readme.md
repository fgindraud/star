# Single Thread Asynchronous Runtime #

This is a simple runtime for std Futures, single thread only.
Intended usage is small workloads ; for example small desktop daemons having multiple IO sources (alternative to `select()`).
It should have a low overhead : no Arc/Mutex, no thread stacks.

## Motivation ##

This has been built mainly for personnal uses and to discover the *Rust async ecosystem*, in details.

Interesting points :
- Tasks use a single allocation (includes the waker)
- Task frame use trait objects for type erasure, avoiding using unsafe
- `JoinHandle<T>` is a nice API, taken from `async-task`
- uses `poll`, so async File IO should be possible

Unsafe uses :
- Building a `Waker` with the vtable. Tedious to do. Unavoidable in my case, as I cannot use the `From<Arc<W>> where W : Wake`.
- *Structural pinning* for the future in the pinned task frame.
- `PinWeak<T>` : missing support for creating `Pin<Weak<T>>` from `Pin<Rc<T>>`

## TODO ##

Reactor:
- time
- io read/write

Test with xstalker