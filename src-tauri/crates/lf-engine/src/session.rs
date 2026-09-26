//! OWNS: saving and loading a session through the engine (`docs/plans/native-engine.md` § Stage 5,
//! Session): the host's [`SessionPort`], a [`Snapshot`] copied out of the committed lanes a budget per
//! rendered frame, and a [`Load`] swapped into an all-EMPTY looper. Neither allocates, frees or waits on
//! the audio thread: the host hands over every buffer (a snapshot's destination; a load's lane buffers
//! and their peaks, built off the audio thread) and takes each back, a load's with the engine's old lane
//! buffers in them, to free it.
//!
//! A snapshot is the committed loops as they stood on the frame it began (it waits for the looper's
//! block jobs first): an OVERDUBBING lane gives its loop as committed before the layer in flight (its
//! undo buffer), and every loop comes out in play order (a reversed lane's buffer read backwards). It
//! copies `SNAPSHOT_RATE` positions per rendered frame, about 1.4 s of copying for five 60-second lanes
//! at 48 kHz, so a write to a buffer it copies from (a new overdub, a clear and a new take) can land
//! meanwhile: it then answers [`SessionError::Changed`] and the host tries again. While no device runs
//! the host services the port itself, all at once ([`crate::Engine::service_session_idle`]).
//!
//! A load applies at a block start into an all-EMPTY looper with no take in flight: each lane's buffer
//! is swapped for the host's, the tempo set and locked, the master grid anchored on that frame, and the
//! PLAYING lanes play loop position 0 there together (the web `loadSession`); the lanes and the
//! transport reach the feed at the next publish, as a commit's do.

use rtrb::{Consumer, Producer, RingBuffer};

use crate::api::{LaneState, TRACK_COUNT};
use crate::grid::Frame;

/// Loop positions a snapshot copies per rendered frame (as a block job moves).
pub const SNAPSHOT_RATE: Frame = crate::looper::JOB_RATE;

/// Why a session job did not complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionError {
    /// A load needs every lane EMPTY and no take in flight.
    NotEmpty,
    /// A snapshot's destination holds fewer than this many samples: grow it and try again.
    TooSmall(usize),
    /// A buffer the snapshot copied from was written while it copied: try again.
    Changed,
    /// A load that does not fit the engine (its text says why).
    Invalid(&'static str),
}

impl SessionError {
    pub fn text(self) -> String {
        match self {
            SessionError::NotEmpty => "the looper is not empty: clear every track first".to_string(),
            SessionError::TooSmall(need) => format!("the snapshot needs {need} samples"),
            SessionError::Changed => "the loops changed while the snapshot copied them".to_string(),
            SessionError::Invalid(why) => why.to_string(),
        }
    }
}

/// A committed lane in a snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotTrack {
    pub index: u8,
    pub state: LaneState,
    /// The lane plays its loop backwards: its PCM is in play order all the same.
    pub reversed: bool,
}

/// What a snapshot copies for one lane: which buffer, read which way, and the buffer's write count
/// when it began.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Pin {
    pub(crate) buf: usize,
    pub(crate) backwards: bool,
    pub(crate) writes: u64,
}

pub struct Snapshot {
    /// The destination, allocated and written through by the host (its pages touched): the tracks'
    /// loops back to back, `master` samples each, in `tracks` order.
    pub pcm: Vec<f32>,
    pub rate: u32,
    pub master: Frame,
    pub bpm: u32,
    /// The committed lanes, ascending; `count` of them.
    pub tracks: [Option<SnapshotTrack>; TRACK_COUNT],
    pub count: usize,
    /// `None` while in flight.
    pub result: Option<Result<(), SessionError>>,
    pub(crate) started: bool,
    pub(crate) pins: [Option<Pin>; TRACK_COUNT],
    /// Samples copied so far, over all tracks.
    pub(crate) done: usize,
}

impl Snapshot {
    /// A snapshot into `pcm` (the host's buffer, its pages touched).
    pub fn new(pcm: Vec<f32>) -> Snapshot {
        Snapshot { pcm, rate: 0, master: 0, bpm: 0, tracks: [None; TRACK_COUNT], count: 0, result: None, started: false, pins: [None; TRACK_COUNT], done: 0 }
    }
}

/// One lane of a load.
pub struct LoadTrack {
    pub index: u8,
    /// The lane's new buffer: exactly the looper's capacity long, the loop in its first `master`
    /// samples in buffer order (a reversed lane's play-order PCM reversed back). After the load it holds
    /// the engine's old buffer, for the host to free.
    pub buf: Vec<f32>,
    /// The buffer's waveform bins (`overview::PEAK_FRAMES` frames each, buffer order): (min, max).
    pub peaks: Vec<(f32, f32)>,
    pub reversed: bool,
    /// PLAYING from the grid anchor; else STOPPED.
    pub playing: bool,
}

pub struct Load {
    pub bpm: u32,
    pub bars: Frame,
    pub master: Frame,
    pub tracks: Vec<LoadTrack>,
    /// `None` while in flight.
    pub result: Option<Result<(), SessionError>>,
}

pub enum SessionJob {
    Snapshot(Snapshot),
    Load(Load),
}

impl SessionJob {
    fn result(&self) -> Option<Result<(), SessionError>> {
        match self {
            SessionJob::Snapshot(s) => s.result,
            SessionJob::Load(l) => l.result,
        }
    }
}

/// The host's end (`EngineHandle::session`): one job at a time, handed over and taken back.
pub struct SessionPort {
    tx: Producer<Box<SessionJob>>,
    rx: Consumer<Box<SessionJob>>,
}

impl SessionPort {
    /// Hand a job to the engine; it comes back on [`SessionPort::returned`] with its result. Gives it
    /// back when one is already on its way in.
    pub fn send(&mut self, job: Box<SessionJob>) -> Result<(), Box<SessionJob>> {
        self.tx.push(job).map_err(|rtrb::PushError::Full(job)| job)
    }

    pub fn returned(&mut self) -> Option<Box<SessionJob>> {
        self.rx.pop().ok()
    }
}

/// The engine's end: the job it holds, and the rings.
pub(crate) struct SessionEnd {
    rx: Consumer<Box<SessionJob>>,
    tx: Producer<Box<SessionJob>>,
    job: Option<Box<SessionJob>>,
}

pub(crate) fn channel() -> (SessionPort, SessionEnd) {
    let (in_tx, in_rx) = RingBuffer::new(1);
    let (back_tx, back_rx) = RingBuffer::new(1);
    (SessionPort { tx: in_tx, rx: back_rx }, SessionEnd { rx: in_rx, tx: back_tx, job: None })
}

impl SessionEnd {
    /// At a block start: take a job the host sent (only while the return ring has room, so it can
    /// always go back), apply a load, begin a snapshot once no block job runs. `idle`: no device runs,
    /// so a block job cannot finish either and a snapshot waiting for one answers `Changed`.
    pub(crate) fn begin(&mut self, looper: &mut crate::looper::Looper, cx: &mut crate::looper::Cx, rate: u32, idle: bool) {
        if self.job.is_none() && self.tx.slots() > 0 {
            self.job = self.rx.pop().ok();
        }
        match self.job.as_deref_mut() {
            Some(SessionJob::Load(load)) if load.result.is_none() => load.result = Some(looper.load(cx, load)),
            Some(SessionJob::Snapshot(s)) if !s.started && !looper.busy() => looper.snapshot_begin(s, cx.clock.bpm(), rate),
            Some(SessionJob::Snapshot(s)) if !s.started && idle => s.result = Some(Err(SessionError::Changed)),
            _ => {}
        }
        self.finish();
    }

    /// After `frames` rendered: copy the snapshot's share of them.
    pub(crate) fn advance(&mut self, looper: &crate::looper::Looper, frames: Frame) {
        if let Some(SessionJob::Snapshot(s)) = self.job.as_deref_mut() {
            if s.started && s.result.is_none() {
                looper.snapshot_copy(s, (frames.max(1) * SNAPSHOT_RATE) as usize);
            }
        }
        self.finish();
    }

    /// A finished job goes back to the host (the ring had room when it was taken).
    fn finish(&mut self) {
        if self.job.as_ref().is_some_and(|j| j.result().is_some()) {
            if let Some(job) = self.job.take() {
                if let Err(rtrb::PushError::Full(job)) = self.tx.push(job) {
                    self.job = Some(job);
                }
            }
        }
    }
}
