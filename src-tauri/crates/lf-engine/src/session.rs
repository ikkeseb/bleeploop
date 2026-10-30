//! OWNS: saving and loading a session through the engine (the bytes and the host side:
//! `src-tauri/src/engine_io/session.rs`): the host's [`SessionPort`], a [`Snapshot`] copied out of the committed lanes a budget per
//! rendered frame, and a [`Load`] swapped into an all-EMPTY looper. Neither allocates, frees or waits on
//! the audio thread: the host hands over every buffer (a snapshot's destination; a load's lane buffers
//! and their peaks, built off the audio thread) and takes each back, a load's with the engine's old lane
//! buffers in them, to free it.
//!
//! A session job comes after every command sent before it (one stamped for a later frame lands on its
//! frame, by the stamp). The host hands it over with the count of
//! commands it has pushed so far, and [`SessionPort::send`] and those pushes share one lock, so the
//! ring's own count orders both, for every sender (`engine_io`'s `Ends::pushed`). The job leaves the
//! port at a block start, after the commands due on that frame, once the engine has taken that many. A
//! load also waits for those a block job holds, and no command sent after it is taken until it has run;
//! a snapshot holds nothing back (a later change may reach its pin: [`SnapshotTrack::mix`]). With no
//! device running a snapshot runs at once on what is applied, and a load waits for the next block: only
//! a block may apply the commands ahead of it, with the device's alignment and a table that can drain
//! ([`crate::Engine::service_session_idle`]).
//!
//! A snapshot is the committed loops as they stood on the frame it began (it waits for the looper's
//! block jobs first), each with its lane's mix as the engine applied it on that frame: an OVERDUBBING
//! lane gives its loop as committed before the layer in flight (its undo buffer), and every loop comes
//! out in play order (a reversed lane's buffer read backwards). It copies `SNAPSHOT_RATE` positions per
//! rendered frame, about 0.3 s of copying for five 60-second lanes at 48 kHz, so a write to a buffer it
//! copies from (a new overdub, a clear and a new take) can land meanwhile: it then answers
//! [`SessionError::Changed`] and the host tries again. While no device runs the host services the port
//! itself, all at once ([`crate::Engine::service_session_idle`]).
//!
//! A load applies at a block start into an all-EMPTY looper with no take in flight: each lane's buffer
//! is swapped for the host's and its mix set from the load (overriding a mix sent to the EMPTY lane
//! before), the tempo set and locked, the master grid anchored on that frame, and the PLAYING lanes play
//! loop position 0 there together (the web `loadSession`), each at its loaded level from its first
//! sample, through an FX chain that holds nothing from before it (`effects::LaneFx::clear_history`);
//! the lanes and the transport reach the feed at the next publish, as a commit's do.
//!
//! A job the host gives up on before the engine took it can be withdrawn ([`SessionPort::withdraw`]):
//! the engine then hands it back unrun ([`SessionError::Withdrawn`]), so a load answered as failed never
//! applies later. One atomic decides which side won: the host's withdrawal or the engine's claim as it
//! takes the job off the ring.

use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed};
use std::sync::Arc;

use rtrb::{Consumer, Producer, RingBuffer};

use crate::api::{LaneMix, LaneState, TRACK_COUNT};
use crate::grid::{Frame, Meter};

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
    /// The host withdrew the job before the engine took it ([`SessionPort::withdraw`]): it never ran.
    Withdrawn,
}

impl SessionError {
    pub fn text(self) -> String {
        match self {
            SessionError::NotEmpty => "the looper is not empty: clear every track first".to_string(),
            SessionError::TooSmall(need) => format!("the snapshot needs {need} samples"),
            SessionError::Changed => "the loops changed while the snapshot copied them".to_string(),
            SessionError::Invalid(why) => why.to_string(),
            SessionError::Withdrawn => "the session job was withdrawn before it ran".to_string(),
        }
    }
}

/// A committed lane in a snapshot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SnapshotTrack {
    pub index: u8,
    pub state: LaneState,
    /// The lane plays its loop backwards: its PCM is in play order all the same.
    pub reversed: bool,
    /// The lane's mix as the engine applied it on the frame the snapshot pinned its loop, a block start
    /// once no block job runs: not necessarily the mix when the snapshot was asked for. A change applied
    /// up to that frame is in it (a command due on it included, one that reached the engine with the
    /// request too); one applied while it copies is not.
    pub mix: LaneMix,
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
    /// The meter the master's bars count in.
    pub meter: Meter,
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
        Snapshot { pcm, rate: 0, master: 0, bpm: 0, meter: Meter::FOUR_FOUR, tracks: [None; TRACK_COUNT], count: 0, result: None, started: false, pins: [None; TRACK_COUNT], done: 0 }
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
    /// The lane's volume, mute, DUB FEEDBACK, pan and FX from the load's frame (its pan and level with no
    /// glide in).
    pub mix: LaneMix,
}

pub struct Load {
    pub bpm: u32,
    /// The meter `bars` count in: `bars` bars of it at `bpm` are `master` frames.
    pub meter: Meter,
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

    fn set_result(&mut self, result: Result<(), SessionError>) {
        match self {
            SessionJob::Snapshot(s) => s.result = Some(result),
            SessionJob::Load(l) => l.result = Some(result),
        }
    }
}

/// The last job sent: on the ring, not taken yet (the host may still withdraw it).
const QUEUED: u8 = 0;
/// The engine took it off the ring: it runs, whatever the host does.
const TAKEN: u8 = 1;
/// The host withdrew it before the engine took it: it comes back unrun.
const WITHDRAWN: u8 = 2;

/// A job on its way in, with the commands it comes after ([`SessionPort::send`]).
type Sent = (u64, Box<SessionJob>);

/// The host's end (`EngineHandle::session`): one job at a time, handed over and taken back.
pub struct SessionPort {
    tx: Producer<Sent>,
    rx: Consumer<Box<SessionJob>>,
    /// The last job sent's `QUEUED` | `TAKEN` | `WITHDRAWN`, shared with the engine's end.
    claim: Arc<AtomicU8>,
}

impl SessionPort {
    /// Hand a job to the engine, after the first `pushed` commands of the ring (all a sender has pushed
    /// so far, read under the lock those pushes take); it comes back on [`SessionPort::returned`] with
    /// its result. Gives it back when one is already on its way in.
    pub fn send(&mut self, job: Box<SessionJob>, pushed: u64) -> Result<(), Box<SessionJob>> {
        // Only this end pushes, so a free slot stays free: the claim is reset only for a job that goes
        // in, never under one already on the ring (a withdrawn one must stay withdrawn).
        if self.tx.slots() == 0 {
            return Err(job);
        }
        self.claim.store(QUEUED, Relaxed);
        // The push publishes the store above to the engine's pop (rtrb: release, acquire).
        self.tx.push((pushed, job)).map_err(|rtrb::PushError::Full((_, job))| job)
    }

    pub fn returned(&mut self) -> Option<Box<SessionJob>> {
        self.rx.pop().ok()
    }

    /// Withdraw the job sent last, if the engine has not taken it yet: true, it never runs and comes
    /// back on [`SessionPort::returned`] unrun ([`SessionError::Withdrawn`]), waiting for its commands
    /// or not; false, the engine took it
    /// (it runs, and comes back with its result) or it was withdrawn already.
    pub fn withdraw(&mut self) -> bool {
        self.claim.compare_exchange(QUEUED, WITHDRAWN, AcqRel, Acquire).is_ok()
    }
}

/// The engine's end: the job it holds, and the rings.
pub(crate) struct SessionEnd {
    rx: Consumer<Sent>,
    tx: Producer<Box<SessionJob>>,
    job: Option<Box<SessionJob>>,
    claim: Arc<AtomicU8>,
}

pub(crate) fn channel() -> (SessionPort, SessionEnd) {
    let (in_tx, in_rx) = RingBuffer::new(1);
    let (back_tx, back_rx) = RingBuffer::new(1);
    let claim = Arc::new(AtomicU8::new(TAKEN));
    (SessionPort { tx: in_tx, rx: back_rx, claim: claim.clone() }, SessionEnd { rx: in_rx, tx: back_tx, job: None, claim })
}

impl SessionEnd {
    /// The commands of the ring a load waiting on the port comes after: none past them may be taken
    /// before it runs.
    pub(crate) fn load_fence(&self) -> Option<u64> {
        self.rx.peek().ok().filter(|(_, job)| matches!(**job, SessionJob::Load(_))).map(|&(fence, _)| fence)
    }

    /// The job on the port may leave it: its commands are `taken`, a load's `settled` too (a snapshot
    /// waits out the block jobs anyway, holding nothing back). With no device a snapshot does not wait:
    /// it reads what is applied. One the host withdrew leaves at once.
    fn ready(&self, idle: bool, taken: u64, settled: u64) -> bool {
        self.rx.peek().is_ok_and(|(fence, job)| {
            self.claim.load(Acquire) == WITHDRAWN
                || match **job {
                    SessionJob::Snapshot(_) => idle || *fence <= taken,
                    SessionJob::Load(_) => *fence <= settled,
                }
        })
    }

    /// At a block start, after the commands due on its frame: take a job the host sent once it is
    /// [`ready`](Self::ready) (only while the return ring has room, so it can always go back), apply a
    /// load, begin a snapshot once no block job runs. `idle`: no device runs, so a block job cannot
    /// finish either and a snapshot waiting for one answers `Changed`. `taken`: the commands taken off
    /// the ring so far; `settled`: how many of them lead up to the first one a block job holds.
    /// A job the host withdrew before this took it goes straight back unrun.
    pub(crate) fn begin(&mut self, looper: &mut crate::looper::Looper, cx: &mut crate::looper::Cx, rate: u32, idle: bool, taken: u64, settled: u64) {
        if self.job.is_none() && self.tx.slots() > 0 && self.ready(idle, taken, settled) {
            self.job = self.rx.pop().ok().map(|(_, job)| job);
            if let Some(job) = self.job.as_deref_mut() {
                if self.claim.compare_exchange(QUEUED, TAKEN, AcqRel, Acquire).is_err() {
                    job.set_result(Err(SessionError::Withdrawn));
                    self.finish();
                    return;
                }
            }
        }
        match self.job.as_deref_mut() {
            Some(SessionJob::Load(load)) if load.result.is_none() => load.result = Some(looper.load(cx, load)),
            Some(SessionJob::Snapshot(s)) if !s.started && !looper.busy() => looper.snapshot_begin(s, cx.fx, cx.clock.bpm(), cx.clock.meter(), rate),
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
