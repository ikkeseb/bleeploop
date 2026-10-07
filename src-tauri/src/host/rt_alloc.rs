//! DEV-only global-allocator shim (invariant #5: "no allocation in the RT path"). It is a thin
//! wrapper over `System` that, when a thread-local `rt_guard` is set, bumps that thread's count
//! ([`allocations`]: guarded code read from its own thread, so tests running side by side never count
//! each other's) and the process's (`RT_ALLOCS`: guarded threads read from another). The
//! engine's device callback sets the guard around its body (`engine_io::callback::guarded`), so any
//! steady-state heap allocation on the RT thread is *measured* (not asserted) and counted in its
//! `rt_allocs`; the unit and pipe tests assert zero. Const-initialised
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
