// Copyright 2016 Amanieu d'Antras
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

#[cfg(feature = "nightly")]
use std::sync::atomic::{AtomicU8, ATOMIC_U8_INIT, Ordering, fence};
#[cfg(feature = "nightly")]
type U8 = u8;
#[cfg(not(feature = "nightly"))]
use stable::{AtomicU8, ATOMIC_U8_INIT, Ordering, fence};
#[cfg(not(feature = "nightly"))]
type U8 = usize;
use std::mem;
use parking_lot_core::{self, SpinWait, DEFAULT_PARK_TOKEN, DEFAULT_UNPARK_TOKEN};
use util::UncheckedOptionExt;

const DONE_BIT: U8 = 1;
const POISON_BIT: U8 = 2;
const LOCKED_BIT: U8 = 4;
const PARKED_BIT: U8 = 8;

/// State yielded to the `call_once_force` method which can be used to query
/// whether the `Once` was previously poisoned or not.
pub struct OnceState(bool);

impl OnceState {
    /// Returns whether the associated `Once` has been poisoned.
    ///
    /// Once an initalization routine for a `Once` has panicked it will forever
    /// indicate to future forced initialization routines that it is poisoned.
    #[inline]
    pub fn poisoned(&self) -> bool {
        self.0
    }
}

/// A synchronization primitive which can be used to run a one-time
/// initialization. Useful for one-time initialization for globals, FFI or
/// related functionality.
///
/// # Differences from the standard library `Once`
///
/// - Only requires 1 byte of space, instead of 1 word.
/// - Not required to be `'static`.
/// - Relaxed memory barriers in the fast path, which can significantly improve
///   performance on some architectures.
/// - Efficient handling of micro-contention using adaptive spinning.
///
/// # Examples
///
/// ```
/// use parking_lot::{Once, ONCE_INIT};
///
/// static START: Once = ONCE_INIT;
///
/// START.call_once(|| {
///     // run initialization here
/// });
/// ```
pub struct Once(AtomicU8);

/// Initialization value for static `Once` values.
pub const ONCE_INIT: Once = Once(ATOMIC_U8_INIT);

impl Once {
    /// Creates a new `Once` value.
    #[cfg(feature = "nightly")]
    #[inline]
    pub const fn new() -> Once {
        Once(AtomicU8::new(0))
    }

    /// Creates a new `Once` value.
    #[cfg(not(feature = "nightly"))]
    #[inline]
    pub fn new() -> Once {
        Once(AtomicU8::new(0))
    }

    /// Performs an initialization routine once and only once. The given closure
    /// will be executed if this is the first time `call_once` has been called,
    /// and otherwise the routine will *not* be invoked.
    ///
    /// This method will block the calling thread if another initialization
    /// routine is currently running.
    ///
    /// When this function returns, it is guaranteed that some initialization
    /// has run and completed (it may not be the closure specified). It is also
    /// guaranteed that any memory writes performed by the executed closure can
    /// be reliably observed by other threads at this point (there is a
    /// happens-before relation between the closure and code executing after the
    /// return).
    ///
    /// # Examples
    ///
    /// ```
    /// use parking_lot::{Once, ONCE_INIT};
    ///
    /// static mut VAL: usize = 0;
    /// static INIT: Once = ONCE_INIT;
    ///
    /// // Accessing a `static mut` is unsafe much of the time, but if we do so
    /// // in a synchronized fashion (e.g. write once or read all) then we're
    /// // good to go!
    /// //
    /// // This function will only call `expensive_computation` once, and will
    /// // otherwise always return the value returned from the first invocation.
    /// fn get_cached_val() -> usize {
    ///     unsafe {
    ///         INIT.call_once(|| {
    ///             VAL = expensive_computation();
    ///         });
    ///         VAL
    ///     }
    /// }
    ///
    /// fn expensive_computation() -> usize {
    ///     // ...
    /// # 2
    /// }
    /// ```
    ///
    /// # Panics
    ///
    /// The closure `f` will only be executed once if this is called
    /// concurrently amongst many threads. If that closure panics, however, then
    /// it will *poison* this `Once` instance, causing all future invocations of
    /// `call_once` to also panic.
    #[inline]
    pub fn call_once<F>(&self, f: F)
        where F: FnOnce()
    {
        if self.0.load(Ordering::Acquire) == DONE_BIT {
            return;
        }

        let mut f = Some(f);
        self.call_once_slow(false, &mut |_| unsafe { f.take().unchecked_unwrap()() });
    }

    /// Performs the same function as `call_once` except ignores poisoning.
    ///
    /// If this `Once` has been poisoned (some initialization panicked) then
    /// this function will continue to attempt to call initialization functions
    /// until one of them doesn't panic.
    ///
    /// The closure `f` is yielded a structure which can be used to query the
    /// state of this `Once` (whether initialization has previously panicked or
    /// not).
    #[inline]
    pub fn call_once_force<F>(&self, f: F)
        where F: FnOnce(OnceState)
    {
        if self.0.load(Ordering::Acquire) == DONE_BIT {
            return;
        }

        let mut f = Some(f);
        self.call_once_slow(true,
                            &mut |state| unsafe {
                                f.take().unchecked_unwrap()(state)
                            });
    }

    // This is a non-generic function to reduce the monomorphization cost of
    // using `call_once` (this isn't exactly a trivial or small implementation).
    //
    // Additionally, this is tagged with `#[cold]` as it should indeed be cold
    // and it helps let LLVM know that calls to this function should be off the
    // fast path. Essentially, this should help generate more straight line code
    // in LLVM.
    //
    // Finally, this takes an `FnMut` instead of a `FnOnce` because there's
    // currently no way to take an `FnOnce` and call it via virtual dispatch
    // without some allocation overhead.
    #[cold]
    #[inline(never)]
    fn call_once_slow(&self, ignore_poison: bool, f: &mut FnMut(OnceState)) {
        let mut spinwait = SpinWait::new();
        let mut state = self.0.load(Ordering::Relaxed);
        loop {
            // If another thread called the closure, we're done
            if state & DONE_BIT != 0 {
                // An acquire fence is needed here since we didn't load the
                // state with Ordering::Acquire.
                fence(Ordering::Acquire);
                return;
            }

            // If the state has been poisoned and we aren't forcing, then panic
            if state & POISON_BIT != 0 && !ignore_poison {
                // Need the fence here as well for the same reason
                fence(Ordering::Acquire);
                panic!("Once instance has previously been poisoned");
            }

            // Grab the lock if it isn't locked, even if there is a queue on it.
            // We also clear the poison bit since we are going to try running
            // the closure again.
            if state & LOCKED_BIT == 0 {
                match self.0
                    .compare_exchange_weak(state,
                                           (state | LOCKED_BIT) & !POISON_BIT,
                                           Ordering::Acquire,
                                           Ordering::Relaxed) {
                    Ok(_) => break,
                    Err(x) => state = x,
                }
                continue;
            }

            // If there is no queue, try spinning a few times
            if state & PARKED_BIT == 0 && spinwait.spin() {
                state = self.0.load(Ordering::Relaxed);
                continue;
            }

            // Set the parked bit
            if state & PARKED_BIT == 0 {
                if let Err(x) = self.0.compare_exchange_weak(state,
                                                             state | PARKED_BIT,
                                                             Ordering::Relaxed,
                                                             Ordering::Relaxed) {
                    state = x;
                    continue;
                }
            }

            // Park our thread until we are woken up by the thread that owns the
            // lock.
            unsafe {
                let addr = self as *const _ as usize;
                let validate = || self.0.load(Ordering::Relaxed) == LOCKED_BIT | PARKED_BIT;
                let before_sleep = || {};
                let timed_out = |_, _| unreachable!();
                parking_lot_core::park(addr,
                                       validate,
                                       before_sleep,
                                       timed_out,
                                       DEFAULT_PARK_TOKEN,
                                       None);
            }

            // Loop back and check if the done bit was set
            spinwait.reset();
            state = self.0.load(Ordering::Relaxed);
        }

        struct PanicGuard<'a>(&'a Once);
        impl<'a> Drop for PanicGuard<'a> {
            fn drop(&mut self) {
                // Mark the state as poisoned, unlock it and unpark all threads.
                let once = self.0;
                let state = once.0.swap(POISON_BIT, Ordering::Release);
                if state & PARKED_BIT != 0 {
                    unsafe {
                        let addr = once as *const _ as usize;
                        parking_lot_core::unpark_all(addr, DEFAULT_UNPARK_TOKEN);
                    }
                }
            }
        }

        // At this point we have the lock, so run the closure. Make sure we
        // properly clean up if the closure panicks.
        let guard = PanicGuard(self);
        f(OnceState(state & POISON_BIT != 0));
        mem::forget(guard);

        // Now unlock the state, set the done bit and unpark all threads
        let state = self.0.swap(DONE_BIT, Ordering::Release);
        if state & PARKED_BIT != 0 {
            unsafe {
                let addr = self as *const _ as usize;
                parking_lot_core::unpark_all(addr, DEFAULT_UNPARK_TOKEN);
            }
        }
    }
}

impl Default for Once {
    #[inline]
    fn default() -> Once {
        Once::new()
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "nightly")]
    use std::panic;
    use std::sync::mpsc::channel;
    use std::thread;
    use {Once, ONCE_INIT};

    #[test]
    fn smoke_once() {
        static O: Once = ONCE_INIT;
        let mut a = 0;
        O.call_once(|| a += 1);
        assert_eq!(a, 1);
        O.call_once(|| a += 1);
        assert_eq!(a, 1);
    }

    #[test]
    fn stampede_once() {
        static O: Once = ONCE_INIT;
        static mut run: bool = false;

        let (tx, rx) = channel();
        for _ in 0..10 {
            let tx = tx.clone();
            thread::spawn(move || {
                for _ in 0..4 {
                    thread::yield_now()
                }
                unsafe {
                    O.call_once(|| {
                        assert!(!run);
                        run = true;
                    });
                    assert!(run);
                }
                tx.send(()).unwrap();
            });
        }

        unsafe {
            O.call_once(|| {
                assert!(!run);
                run = true;
            });
            assert!(run);
        }

        for _ in 0..10 {
            rx.recv().unwrap();
        }
    }

    #[cfg(feature = "nightly")]
    #[test]
    fn poison_bad() {
        static O: Once = ONCE_INIT;

        // poison the once
        let t = panic::catch_unwind(|| {
            O.call_once(|| panic!());
        });
        assert!(t.is_err());

        // poisoning propagates
        let t = panic::catch_unwind(|| {
            O.call_once(|| {});
        });
        assert!(t.is_err());

        // we can subvert poisoning, however
        let mut called = false;
        O.call_once_force(|p| {
            called = true;
            assert!(p.poisoned())
        });
        assert!(called);

        // once any success happens, we stop propagating the poison
        O.call_once(|| {});
    }

    #[cfg(feature = "nightly")]
    #[test]
    fn wait_for_force_to_finish() {
        static O: Once = ONCE_INIT;

        // poison the once
        let t = panic::catch_unwind(|| {
            O.call_once(|| panic!());
        });
        assert!(t.is_err());

        // make sure someone's waiting inside the once via a force
        let (tx1, rx1) = channel();
        let (tx2, rx2) = channel();
        let t1 = thread::spawn(move || {
            O.call_once_force(|p| {
                assert!(p.poisoned());
                tx1.send(()).unwrap();
                rx2.recv().unwrap();
            });
        });

        rx1.recv().unwrap();

        // put another waiter on the once
        let t2 = thread::spawn(|| {
            let mut called = false;
            O.call_once(|| {
                called = true;
            });
            assert!(!called);
        });

        tx2.send(()).unwrap();

        assert!(t1.join().is_ok());
        assert!(t2.join().is_ok());

    }
}
