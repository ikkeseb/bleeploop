//! The export's memory, measured (an ignored bar: run it on its own, in release, `cargo test -p lf-engine
//! --release --no-default-features --test export_memory -- --ignored --nocapture`). A five-lane,
//! 60-second session at 48 kHz renders its wet master (`render.rs`) under a counting allocator: the peak
//! the render adds over the load it takes. The live engine's own pool and the snapshot's stems are
//! beside it in the app; `engine_io` renders one export's master at a time (`Core::master_render`), so
//! this is the most an export adds while another waits.
//!
//! Measured 2026-11-20 (the engine at opt-level 3, 48 kHz, five 60 s lanes): the render adds a peak of
//! 394.3 MB over its 109.9 MB load; the render engine's eleven stereo lane buffers are 253.4 MB of it
//! (`docs/unit-contract.md` §7), the rest is not broken down. The bar sits just above it.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use lf_engine::grid::Meter;
use lf_engine::{wet_master, LaneMix, Load, LoadTrack};

struct Counting;

static NOW: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(size: usize) {
    let now = NOW.fetch_add(size, Relaxed) + size;
    PEAK.fetch_max(now, Relaxed);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        unsafe { System.dealloc(p, layout) };
        NOW.fetch_sub(layout.size(), Relaxed);
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const MB: f64 = 1024.0 * 1024.0;

#[test]
#[ignore]
fn a_five_lane_minute_long_exports_master_render_adds_under_420_mb() {
    const RATE: u32 = 48_000;
    // 60 s: 30 bars of 4/4 at 120 BPM.
    let master = 60 * RATE as i64;
    let tracks: Vec<LoadTrack> = (0..5u8)
        .map(|index| {
            let buf = (0..master).map(|f| {
                let x = 0.1 * ((f * (index as i64 + 3)) % 997) as f32 / 997.0;
                [x, -x]
            });
            LoadTrack { index, buf: buf.collect(), peaks: Vec::new(), reversed: false, playing: true, mix: LaneMix::default() }
        })
        .collect();
    let load = Load { bpm: 120, meter: Meter::FOUR_FOUR, bars: 30, master, tracks, result: None };
    let before = NOW.load(Relaxed);
    PEAK.store(before, Relaxed);
    let wet = wet_master(RATE, load, &[]).expect("the render");
    let added = (PEAK.load(Relaxed) - before) as f64 / MB;
    println!("export render: a five-lane 60 s master at 48 kHz adds a peak of {added:.1} MB over its {:.1} MB load", before as f64 / MB);
    assert_eq!(wet.left.len(), master as usize);
    assert!(added < 420.0, "{added:.1} MB");
}
