//! Test support: run code as the audio thread would, and see whether it
//! touched the heap.
//!
//! [`realtime`] runs a closure inside RealtimeSanitizer's real-time context.
//! In a build with `RTSAN_ENABLE=1` set, an allocation, a free, a lock or a
//! blocking call in there aborts the test binary with a stack trace. Without
//! it, the counting allocator below still catches allocations and frees, so
//! the same tests mean something on every machine.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static WATCHING: Cell<bool> = const { Cell::new(false) };
    static CALLS: Cell<usize> = const { Cell::new(0) };
}

/// The system allocator, counting calls on threads that are being watched.
struct Counting;

fn note() {
    let _ = WATCHING.try_with(|w| {
        if w.get() {
            let _ = CALLS.try_with(|c| c.set(c.get() + 1));
        }
    });
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note();
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note();
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Runs `f` as the audio thread runs: in RTSan's real-time context, and
/// counting this thread's heap calls. Returns `f`'s result and the count.
pub fn realtime<R>(f: impl FnOnce() -> R) -> (R, usize) {
    CALLS.with(|c| c.set(0));
    WATCHING.with(|w| w.set(true));
    rtsan_standalone::realtime_enter();
    let r = f();
    rtsan_standalone::realtime_exit();
    WATCHING.with(|w| w.set(false));
    (r, CALLS.with(Cell::get))
}

#[test]
fn the_counter_sees_an_allocation() {
    // Not inside `realtime`'s real-time context, so RTSan stays quiet.
    CALLS.with(|c| c.set(0));
    WATCHING.with(|w| w.set(true));
    let v = std::hint::black_box(vec![1u8; 64]);
    drop(v);
    WATCHING.with(|w| w.set(false));
    assert_eq!(CALLS.with(Cell::get), 2, "one allocation and one free");
}
