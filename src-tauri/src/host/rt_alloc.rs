//! DEV-only global-allocator shim (invariant #5: "no allocation in the RT path"). It is a thin
//! wrapper over `System` that, when a thread-local `rt_guard` is set, bumps that thread's count
//! ([`allocations`]: guarded code read from its own thread, so tests running side by side never count
//! each other's) and the process's (`RT_ALLOCS`: guarded threads read from another). Each audio
//! callback, input, output and Share output's, sets the guard around its whole body
//! (`engine_io::callback::RtSection`), so any steady-state heap allocation on the RT thread is
//! *measured* (not asserted) and counted in its `rt_allocs`; the unit and pipe tests assert zero. Const-initialised
//! thread-local → no lazy alloc/registration inside `alloc()` (safe to read there). Debug builds
//! only; `tauri dev` is a debug build, so the gate sees real numbers.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
/// Every thread's allocations under its guard so far.
pub static RT_ALLOCS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static RT_GUARD: Cell<bool> = const { Cell::new(false) };
    static THREAD_ALLOCS: Cell<u64> = const { Cell::new(0) };
}

/// The allocations this thread made under its guard so far.
pub fn allocations() -> u64 {
    THREAD_ALLOCS.with(Cell::get)
}

fn count() {
    if RT_GUARD.with(Cell::get) {
        THREAD_ALLOCS.with(|n| n.set(n.get() + 1));
        RT_ALLOCS.fetch_add(1, Ordering::Relaxed);
    }
}

/// RAII: sets the per-thread "count allocations" flag for its lifetime, and puts back what it found,
/// so a guard inside another leaves the outer one counting.
pub struct Guard {
    was: bool,
}

pub fn guard() -> Guard {
    Guard { was: RT_GUARD.with(|g| g.replace(true)) }
}

impl Drop for Guard {
    fn drop(&mut self) {
        RT_GUARD.with(|g| g.set(self.was));
    }
}

pub struct Shim;

// SAFETY: every method forwards to the System allocator; the only extra work is const-init
// thread-locals without destructors (readable at any point of a thread's life) and a relaxed atomic
// add, none of which allocates or re-enters alloc.
unsafe impl GlobalAlloc for Shim {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count();
        System.alloc_zeroed(layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        System.realloc(ptr, layout, new_size)
    }
}
