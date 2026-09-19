//! DEV-only global-allocator shim (invariant #5: "no allocation in the RT path"). It is a thin
//! wrapper over `System` that, when a thread-local `rt_guard` is set, bumps `RT_ALLOCS`. The
//! producer sets the guard around its per-block body, so any steady-state heap allocation on the
//! RT thread is *measured* (not asserted) and surfaced in the gate line. Const-initialised
//! thread-local → no lazy alloc/registration inside `alloc()` (safe to read there). Debug builds
//! only; `tauri dev` is a debug build, so the gate sees real numbers.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

pub static RT_ALLOCS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static RT_GUARD: Cell<bool> = const { Cell::new(false) };
}

/// RAII: sets the per-thread "count allocations" flag for its lifetime.
pub struct Guard(());

pub fn guard() -> Guard {
    RT_GUARD.with(|g| g.set(true));
    Guard(())
}

impl Drop for Guard {
    fn drop(&mut self) {
        RT_GUARD.with(|g| g.set(false));
    }
}

pub struct Shim;

// SAFETY: every method forwards to the System allocator; the only extra work is a const-init
// thread-local read + a relaxed atomic add, neither of which allocates or can re-enter alloc.
unsafe impl GlobalAlloc for Shim {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if RT_GUARD.with(|g| g.get()) {
            RT_ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if RT_GUARD.with(|g| g.get()) {
            RT_ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        System.alloc_zeroed(layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if RT_GUARD.with(|g| g.get()) {
            RT_ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        System.realloc(ptr, layout, new_size)
    }
}
