# Single Thread Asynchronous Runtime #

This is a simple runtime for std Futures, single thread only.

Intended usage is small workloads ; for example small desktop daemons having multiple IO sources.
The goal is to provide an alternative to a loop with `select()` with a much nicer API and semantics.

This supports only `unix` targets due to use of `poll`, `RawFd` and `libc`.

## Motivation / technical ##

This has been built mainly for personnal uses and to discover the *Rust async ecosystem*, in details.

It is designed with low overhead in mind:
- no `Arc`/`Mutex` or _atomics_ due to single thread.
- no thread stacks.
- very few memory allocations.
- low dependency count (mostly `libc`).

**Structural pinning** :
One of the goal was to minimize dependencies for low compile-time overhead.
I tried to avoid using any `proc_macro` dependent crate.
However handling structural pinning is very tedious to do manually, and requires many uses of `unsafe`.
In the end I gave in and used `pin-project` ; this also improves code documentation by having a clear `#[pin]` tag instead of doc + unsafe impls.
The recent `pin-project-lite` was tried, but it does not support enums nor docstrings in structs.

## Interesting points / features ##
- Tasks use a single allocation (including the `Waker`).
- Task frame use trait objects for type erasure, avoiding using unsafe.
- `JoinHandle<T>` is a nice API, taken from `async-task`.
- uses `poll`, so async File IO should be possible.
- A very minimal API for interacting with events (reactor) : a pair of runtime-specific `Future` types that complete on event trigger.
- Use of an intrusive list design -> those future types require no memory allocation (in place).

## Unsafe usage ##
- Building a `Waker` with the vtable. Tedious to do. Unavoidable in my case, as I cannot use the `From<Arc<W>> where W : Wake`.
- `PinWeak<T>` : missing support for creating `Pin<Weak<T>>` from `Pin<Rc<T>>`.
- `PinCell<T>` : a `RefCell` like structure propagating pinning.
- Wrapper to `poll()` syscall.
- The internal intrusive list, which in my opinion is the least robust unsafe use.

## TODO ##

Test with applications : xstalker, ...