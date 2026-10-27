//! The plugin unit fakes the slot tests hand the engine (`tests/slots.rs`, `tests/slot_swap*.rs`). A fake
//! records what it saw in preallocated buffers and atomics, so every `process` still runs under the
//! rig's `assert_no_alloc`, and counts its drops, so a test sees the engine never dropped it.

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};

use lf_engine::grid::Frame;
use lf_engine::{SlotEvent, SlotEventKind, SlotKind, SlotProcessor};

/// What a fake logs before it stops logging (a long run at block size 1 would outgrow it).
pub const LOG: usize = 1 << 16;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Seen {
    Call { frame: Frame, len: usize },
    Event(SlotEvent),
    Stop,
}

/// The test's view of a fake while the engine holds it.
#[derive(Clone)]
pub struct Probe {
    pub log: Arc<Mutex<Vec<Seen>>>,
    pub drops: Arc<AtomicUsize>,
    /// The largest |input| the fake was given, as f32 bits.
    pub input_peak: Arc<AtomicU32>,
}

impl Probe {
    pub fn new() -> Probe {
        Probe { log: Arc::new(Mutex::new(Vec::with_capacity(LOG))), drops: Arc::new(AtomicUsize::new(0)), input_peak: Arc::new(AtomicU32::new(0)) }
    }

    pub fn seen(&self) -> Vec<Seen> {
        let log = self.log.lock().unwrap();
        assert!(log.len() < LOG, "the fake's log filled up");
        log.clone()
    }

    pub fn calls(&self) -> Vec<(Frame, usize)> {
        self.seen().into_iter().filter_map(|s| if let Seen::Call { frame, len } = s { Some((frame, len)) } else { None }).collect()
    }

    /// Every event: the frame it lands on, its offset into its call, and what it is.
    pub fn events(&self) -> Vec<(Frame, u32, SlotEventKind)> {
        let mut at: (Frame, usize) = (0, 0);
        let mut out = Vec::new();
        for s in self.seen() {
            match s {
                Seen::Call { frame, len } => at = (frame, len),
                Seen::Event(e) => {
                    assert!((e.offset as usize) < at.1, "an event's offset lies inside its call");
                    out.push((at.0 + e.offset as Frame, e.offset, e.kind));
                }
                Seen::Stop => {}
            }
        }
        out
    }

    pub fn keys(&self) -> Vec<(bool, u8)> {
        self.events()
            .into_iter()
            .map(|(_, _, k)| match k {
                SlotEventKind::NoteOn { key, .. } => (true, key),
                SlotEventKind::NoteOff { key } => (false, key),
            })
            .collect()
    }

    pub fn stops(&self) -> usize {
        self.seen().iter().filter(|s| **s == Seen::Stop).count()
    }

    pub fn input_peak(&self) -> f32 {
        f32::from_bits(self.input_peak.load(SeqCst))
    }
}

/// A method a [`Fake`] panics in, as a plugin can.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PanicIn {
    Kind,
    Latency,
}

/// A unit stand-in. Its output is `level` plus `through` × its input; an instrument also plays a 1.0
/// impulse `latency` frames after each NoteOn.
pub struct Fake {
    pub id: u32,
    pub kind: SlotKind,
    pub latency: Frame,
    pub level: f32,
    pub through: f32,
    impulse: Option<Frame>,
    /// Added to the last frame of every call (a value gone bad late in an otherwise sound block).
    last: f32,
    /// It refuses to start processing in its first call ([`SlotProcessor::start_refused`]).
    refuses: bool,
    panics: Option<PanicIn>,
    called: bool,
    probe: Probe,
}

impl Fake {
    pub fn effect(level: f32, through: f32, probe: &Probe) -> Box<Fake> {
        Box::new(Fake { id: 0, kind: SlotKind::Effect, latency: 0, level, through, impulse: None, last: 0.0, refuses: false, panics: None, called: false, probe: probe.clone() })
    }

    pub fn instrument(level: f32, latency: Frame, probe: &Probe) -> Box<Fake> {
        Box::new(Fake { id: 0, kind: SlotKind::Instrument, latency, level, through: 0.0, impulse: None, last: 0.0, refuses: false, panics: None, called: false, probe: probe.clone() })
    }

    pub fn with_last(mut self: Box<Self>, last: f32) -> Box<Fake> {
        self.last = last;
        self
    }

    pub fn with_id(mut self: Box<Self>, id: u32) -> Box<Fake> {
        self.id = id;
        self
    }

    pub fn with_latency(mut self: Box<Self>, latency: Frame) -> Box<Fake> {
        self.latency = latency;
        self
    }

    pub fn refusing(mut self: Box<Self>) -> Box<Fake> {
        self.refuses = true;
        self
    }

    pub fn panicking_in(mut self: Box<Self>, method: PanicIn) -> Box<Fake> {
        self.panics = Some(method);
        self
    }
}

impl SlotProcessor for Fake {
    fn kind(&self) -> SlotKind {
        assert_ne!(self.panics, Some(PanicIn::Kind), "the unit panics in kind()");
        self.kind
    }

    fn latency(&self) -> Frame {
        assert_ne!(self.panics, Some(PanicIn::Latency), "the unit panics in latency()");
        self.latency
    }

    fn process(&mut self, frame: Frame, input: &[f32], events: &[SlotEvent], out: &mut [f32]) {
        self.called = true;
        let mut log = self.probe.log.lock().unwrap();
        if log.len() + 1 + events.len() < LOG {
            log.push(Seen::Call { frame, len: out.len() });
            log.extend(events.iter().map(|&e| Seen::Event(e)));
        }
        let peak = input.iter().fold(self.probe.input_peak(), |m, x| m.max(x.abs()));
        self.probe.input_peak.store(peak.to_bits(), SeqCst);
        let mut next = events.iter().peekable();
        for (k, (y, &x)) in out.iter_mut().zip(input).enumerate() {
            while let Some(e) = next.next_if(|e| e.offset as usize == k) {
                if matches!(e.kind, SlotEventKind::NoteOn { .. }) {
                    self.impulse = Some(frame + k as Frame + self.latency);
                }
            }
            let hit = if self.impulse == Some(frame + k as Frame) { 1.0 } else { 0.0 };
            *y = self.level + self.through * x + hit;
        }
        if let Some(y) = out.last_mut() {
            *y += self.last;
        }
    }

    fn stop(&mut self) {
        self.probe.log.lock().unwrap().push(Seen::Stop);
    }

    fn start_refused(&self) -> bool {
        self.refuses && self.called
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.probe.drops.fetch_add(1, SeqCst);
    }
}

/// A unit the engine handed back, as the fake it is.
pub fn fake(unit: Box<dyn SlotProcessor>) -> Box<Fake> {
    unit.into_any().downcast::<Fake>().ok().expect("the unit is the fake")
}
