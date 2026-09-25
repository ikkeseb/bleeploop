//! OWNS: what crosses the RT boundary: the commands the UI (and, from Stage 4, native MIDI) sends, the
//! events the engine answers with, and the per-callback context and I/O. Commands and events travel
//! only over rtrb rings; a full event ring drops the event and counts it, nothing blocks.

use crate::dsp::fx::{FxKind, FxParam};
use crate::grid::Frame;

pub const TRACK_COUNT: usize = 5;

/// A command, applied at `frame` (a device frame, e.g. a MIDI pedal's press) or, with `None`, at the
/// start of the next block the engine renders.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimedCommand {
    pub frame: Option<Frame>,
    pub command: Command,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Command {
    /// The lane's REC/DUB core: record, commit, overdub, commit the layer.
    RecDub(u8),
    /// The lane's PLAY/STOP: commit and stop a capture, stop (or stop at the loop end), resume.
    PlayStop(u8),
    /// Abort: a count-in, an armed take, AUTO listening or an uncommitted take go back to EMPTY, an
    /// overdub layer is discarded, a playing lane stops now.
    Stop(u8),
    /// One-level UNDO/REDO of the lane's last overdub.
    Undo(u8),
    Reverse(u8),
    /// Copy the lane into the first EMPTY lane (answered by [`Event::Copied`]).
    Copy(u8),
    /// Clear the lane (the on-screen control confirms first).
    Clear(u8),
    PlayAll,
    StopAll,
    ClearAll,
    /// A hands-free press on the selected lane: gated, and refused with a reason (keys, pedals).
    Action(Action),
    /// A hands-free press bound to its lane: what a press held for a block job re-enters as, so a
    /// selection change meanwhile cannot move it.
    ActionOn(u8, Action),
    SelectTrack(u8),
    SetBpm(f64),
    SetMetronome(bool),
    SetClickVolume(f32),
    /// The master output level (0..1) and its mute (`src/audio/master.ts`).
    SetMasterVolume(f32),
    SetMasterMute(bool),
    SetLoopEndStop(bool),
    SetFixedLength(bool),
    SetFixedBars(f64),
    SetRetake(bool),
    SetAutoRecord(bool),
    SetAutoSensitivity(f64),
    SetVolume(u8, f32),
    SetMute(u8, bool),
    /// A lane's FX parameter, in its def's units (`src/audio/fx/metadata.ts`).
    SetFxParam(u8, FxParam, f64),
    SetFxBypass(u8, FxKind, bool),
    /// The built-in instrument notes go to; `None` while a plugin slot takes them (Stage 4). Sent on a
    /// slot switch, not per note: it releases the held notes and hands the instrument the wheels, even
    /// when the instrument stays (two slots may hold the same one, as two web synths).
    SelectInstrument(Option<Instrument>),
    /// A note (0..127) on the selected instrument; velocity 0..1. Sustain and the owner of a held note
    /// stay with the sender (`src/audio/input-router.ts`). The instrument commands never wait behind a
    /// looper command that waits for a block job; a note on or off sounds `instruments::LEAD` frames
    /// after it is applied.
    NoteOn(u8, f32),
    NoteOff(u8),
    /// The pitch wheel, in semitones.
    PitchBend(f64),
    /// The mod wheel, 0..1.
    Modulation(f64),
    AllNotesOff,
}

impl Command {
    /// A command for the built-in instruments (notes, wheels, the pick).
    pub fn is_instrument(&self) -> bool {
        matches!(
            self,
            Command::SelectInstrument(_)
                | Command::NoteOn(..)
                | Command::NoteOff(_)
                | Command::PitchBend(_)
                | Command::Modulation(_)
                | Command::AllNotesOff
        )
    }
}

/// The six built-in instruments (`src/audio/synths/index.ts`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Instrument {
    Lead,
    Pad,
    Piano,
    Organ,
    Bass,
    Drums,
}

impl Instrument {
    pub const ALL: [Instrument; 6] = [Instrument::Lead, Instrument::Pad, Instrument::Piano, Instrument::Organ, Instrument::Bass, Instrument::Drums];

    /// The id the UI and the session file use.
    pub fn from_id(id: &str) -> Option<Instrument> {
        Instrument::ALL.into_iter().find(|i| i.id() == id)
    }

    pub fn id(self) -> &'static str {
        ["lead", "pad", "piano", "organ", "bass", "drum"][self as usize]
    }
}

/// The named hands-free actions (`src/app/actions.ts`; GO LIVE stays with the plugin host).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    RecDub,
    PlayStop,
    Undo,
    /// CLEAR on a pedal: the first press arms, the very next looper press on the same lane inside the
    /// confirm window clears.
    Clear,
    NextTrack,
    PrevTrack,
    PlayAll,
    StopAll,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneState {
    Empty,
    Recording,
    Overdubbing,
    Playing,
    Stopped,
}

/// What the UI shows for a lane (the TS `TrackPublic`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaneInfo {
    pub state: LaneState,
    /// The committed loop length, 0 before a commit.
    pub length: Frame,
    /// RECORDING but waiting for the counted downbeat or the master boundary.
    pub armed: bool,
    /// First-track AUTO REC listening for an onset.
    pub auto_armed: bool,
    pub can_undo: bool,
    pub can_reverse: bool,
    pub reversed: bool,
    /// A pending loop-end stop.
    pub stop_at: Option<Frame>,
    /// RETAKE: the 1-based pass in flight, 0 when the lane is not rolling.
    pub retake_pass: u32,
}

/// Why a hands-free press did nothing (`src/ui/looper/gates.ts`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Stopping,
    PlayFirst,
    Reversed,
    OtherRecording,
    Empty,
    NoUndo,
    NoClear,
    /// The first CLEAR press: press again to clear.
    ConfirmClear,
}

impl Refusal {
    pub fn text(self) -> &'static str {
        match self {
            Refusal::Stopping => "stopping at loop end, wait or stop now",
            Refusal::PlayFirst => "play first to overdub",
            Refusal::Reversed => "overdub unavailable while reversed, switch to forward first",
            Refusal::OtherRecording => "another track is recording, stop it first",
            Refusal::Empty => "nothing to play, record first",
            Refusal::NoUndo => "nothing to undo, overdub first",
            Refusal::NoClear => "nothing to clear",
            Refusal::ConfirmClear => "press again to clear",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Event {
    Lane { frame: Frame, lane: u8, info: LaneInfo },
    /// The master loop length (0 = none), the tempo and its lock.
    Transport { frame: Frame, master: Frame, bpm: u32, locked: bool },
    /// A beat of the pulse: the beat LED, the count-in numeral, and whether it clicked.
    Beat { frame: Frame, beat_in_bar: u8, count_left: u8, clicked: bool },
    Selected { frame: Frame, lane: u8 },
    Refused { frame: Frame, lane: u8, reason: Refusal },
    /// A take or overdub layer whose window saw an input gap was discarded.
    TakeRejected { frame: Frame, lane: u8, overdub: bool },
    /// A RETAKE pass saw an input gap: it is dropped, and the kept pass before it with it.
    PassDropped { frame: Frame, lane: u8, pass: u32 },
    Copied { frame: Frame, from: u8, to: u8 },
}

/// The callback's view of the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessContext {
    /// Device frame of the block's first sample.
    pub frame: Frame,
    /// The device lost audio just before this block (an xrun): its first frame follows a gap.
    pub xrun: bool,
    /// Input plus output latency the driver reports, in frames. A take starts this much (plus the
    /// plugin's latency and the master limiter's pre-delay) after its downbeat, so what the player heard
    /// and played lines up on the grid. A constant, not a user trim.
    pub align_frames: Frame,
    /// Of `align_frames`, the input side. A built-in instrument's note is played the output latency
    /// after the click the player heard, so its record path lags it by this (plus the plugin's latency,
    /// less the note's lead) to reach the grid where the guitar does.
    pub input_frames: Frame,
}

/// The plugin seam (Stage 4): the instrument slot processes the mono device input into the wet signal
/// the engine records and monitors.
pub trait Inserts {
    fn process(&mut self, frame: Frame, input: &[f32], wet: &mut [f32]);
    /// Frames the wet signal lags its input; added to the alignment of every take armed after it.
    fn latency(&self) -> Frame {
        0
    }
}

/// No plugin: the dry input is the wet signal.
pub struct Dry;

impl Inserts for Dry {
    fn process(&mut self, _frame: Frame, input: &[f32], wet: &mut [f32]) {
        wet.copy_from_slice(input);
    }
}
