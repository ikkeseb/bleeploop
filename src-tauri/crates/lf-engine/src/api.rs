//! OWNS: what crosses the RT boundary: the commands the UI (and, from Stage 4, native MIDI) sends, the
//! events the engine answers with, the per-callback context and I/O, and the plugin seam
//! ([`SlotProcessor`]). Commands and events travel only over rtrb rings; a full event ring refuses the
//! event and counts it, nothing blocks: a one-off event is lost, a lane's state and mix are offered
//! again at the next publish (`Looper::publish`).

use std::any::Any;

use crate::dsp::fx::{default_fx_states, FxKind, FxParam, FxState, MAX_PARAMS};
use crate::grid::Frame;

pub const TRACK_COUNT: usize = 5;
/// The HOLD controls the engine tells apart ([`Action::Hold`]): pedals held down at once.
pub const HOLD_CONTROLS: usize = 16;
/// The plugin slots (`src/ui/state/instrument-slots.ts`: two instrument slots).
pub const SLOT_COUNT: usize = 2;

/// A command, applied at `frame` (a device frame: a test's or a probe's; the app's senders, native MIDI
/// included, stamp none) or, with `None`, at the start of the next block the engine renders.
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
    /// One-level UNDO/REDO of the lane's last overdub or TRIM.
    Undo(u8),
    Reverse(u8),
    /// Copy the lane into the first EMPTY lane (answered by [`Event::Copied`]).
    Copy(u8),
    /// TRIM (F16): the lane keeps its first `bars` bars as heard and repeats them across the loop, whose
    /// length stays; the loop before it becomes the lane's undo target. A committed lane only, over a
    /// loop of whole bars, `1 <= bars <` its bars; anything else is refused with a reason
    /// ([`Refusal::Capturing`], [`Refusal::Stopping`], [`Refusal::NoTrim`]).
    Trim(u8, u32),
    /// Clear the lane (the on-screen control confirms first).
    Clear(u8),
    PlayAll,
    StopAll,
    ClearAll,
    /// A hands-free press on the selected lane: gated, and refused with a reason (keys, pedals).
    Action(Action),
    /// A hands-free press bound to its lane: a press aimed at a named lane, and what a press held for a
    /// block job re-enters as, so a selection change meanwhile cannot move it.
    ActionOn(u8, Action),
    /// A hands-free press the engine does not run as an [`Action`] (TAP, GO LIVE), sent just before the
    /// setting it changes: a looper press all the same, so it disarms a pending pedal CLEAR. The setting
    /// alone does not (a slider, a settings replay).
    Press,
    SelectTrack(u8),
    SetBpm(f64),
    SetMetronome(bool),
    SetClickVolume(f32),
    /// The master output level (0..1) and its mute (`master.ts`).
    SetMasterVolume(f32),
    SetMasterMute(bool),
    SetLoopEndStop(bool),
    /// FADE's length in bars: 1, 2, 4 or 8 (another count snaps down to one of them, at least 1).
    SetFadeBars(u32),
    SetFixedLength(bool),
    SetFixedBars(f64),
    SetRetake(bool),
    SetAutoRecord(bool),
    SetAutoSensitivity(f64),
    SetVolume(u8, f32),
    SetMute(u8, bool),
    /// A lane's DUB FEEDBACK (0..1, default 1): what an overdub keeps of the loop it writes over, pass by
    /// pass (`input + feedback * old`); 0 replaces it.
    SetDubFeedback(u8, f32),
    /// A lane's pan, -1 (hard left) to 1 (hard right), default 0 (centre); clamped, and a value that is no
    /// number centres it. It glides there (`effects`); only this command glides a lane's pan.
    SetPan(u8, f32),
    /// A lane's FX parameter, in its def's units (`src/ui/state/fx-metadata.ts`).
    SetFxParam(u8, FxParam, f64),
    SetFxBypass(u8, FxKind, bool),
    /// Where the notes go: a built-in instrument, a plugin slot, or nowhere ([`NoteTarget::Off`]). Sent on
    /// a slot switch, not per note: it releases the held notes (of the instrument or slot it leaves) and
    /// hands a built-in instrument the wheels, even when the target stays (two slots may hold the same
    /// synth, as two web synths).
    SelectInstrument(NoteTarget),
    /// A note (0..127) on the selected target; velocity 0..1. Sustain and the owner of a held note stay
    /// with the sender (`src/ui/state/input-router.ts`). The instrument commands never wait behind a
    /// looper command that waits for a block job. On a built-in instrument a note on or off sounds
    /// `instruments::LEAD` frames after it is applied; a plugin slot gets it at the frame it is applied.
    NoteOn(u8, f32),
    NoteOff(u8),
    /// The pitch wheel, in semitones (built-in instruments only; a plugin slot gets no wheels yet, D12).
    PitchBend(f64),
    /// The mod wheel, 0..1 (built-in instruments only).
    Modulation(f64),
    AllNotesOff,
    /// GO LIVE: the slot's own input (its capture channel, picked on the device side) feeds the slot (an
    /// empty slot, or one holding an effect, passes it on as the wet signal; an instrument plugin takes
    /// no input). Off, the slot gets silence. A toggle ramps the slot's input over 5 ms from its frame
    /// (STATUS D23); the effect's own output is never gated. Both slots may be live at once.
    SetSlotLive(u8, bool),
    /// The slot's output level (linear, 0..): the per-plugin gain staging (`plugin-bridge.ts`), on both
    /// what is heard and what is recorded; an empty live slot's is its input's level.
    SetSlotGain(u8, f32),
    /// A built-in instrument's output level (linear, 0.., default 1), smoothed as a slot's gain: on what
    /// is heard and what is recorded, whether or not it is the note target, so a tail rings out at it.
    SetInstrumentGain(Instrument, f32),
    /// An input send on or off (`input_fx`): the ECHO, the REVERB or the RING MOD on the wet signal,
    /// heard and recorded. Off closes its input and lets its tail ring out. A rig setting, not a lane's.
    SetInputSend(InputSend, bool),
    /// An input send's parameter, clamped to its range ([`InputSendParam::range`]).
    SetInputSendParam(InputSendParam, f64),
}

impl Command {
    /// A command for the built-in instruments (notes, wheels, the pick, a level).
    pub fn is_instrument(&self) -> bool {
        matches!(
            self,
            Command::SelectInstrument(_)
                | Command::NoteOn(..)
                | Command::NoteOff(_)
                | Command::PitchBend(_)
                | Command::Modulation(_)
                | Command::AllNotesOff
                | Command::SetInstrumentGain(..)
        )
    }

    /// A command a plugin slot hears: the note target, the notes, and the slot's live flag and gain.
    /// Like the instrument commands it never waits behind the looper, and the engine renders the slots
    /// up to its frame before applying it.
    pub fn reaches_slots(&self) -> bool {
        matches!(
            self,
            Command::SelectInstrument(_)
                | Command::NoteOn(..)
                | Command::NoteOff(_)
                | Command::AllNotesOff
                | Command::SetSlotLive(..)
                | Command::SetSlotGain(..)
        )
    }

    /// A command for the input sends, an input send's toggle included. It never waits behind the looper
    /// either: it touches only the sends, which no block job moves, and the player hears the echo come on
    /// as they switch it. (A toggle that passes a held command disarms a pending CLEAR in its own turn:
    /// `engine.rs` `apply_due`.)
    pub fn is_input_send(&self) -> bool {
        matches!(
            self,
            Command::SetInputSend(..)
                | Command::SetInputSendParam(..)
                | Command::Action(Action::Toggle(Toggle::Send(_)))
                | Command::ActionOn(_, Action::Toggle(Toggle::Send(_)))
        )
    }
}

/// The three input sends (`input_fx`), on the live input before the record tap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputSend {
    /// A tempo-synced feedback delay.
    Echo,
    Reverb,
    /// A ring modulator: the input times a sine carrier.
    Ring,
}

impl InputSend {
    pub const ALL: [InputSend; 3] = [InputSend::Echo, InputSend::Reverb, InputSend::Ring];

    /// The key the UI sends (`src/platform/engine-wire.ts`).
    pub fn key(self) -> &'static str {
        ["echo", "reverb", "ring"][self as usize]
    }

    pub fn from_key(key: &str) -> Option<InputSend> {
        InputSend::ALL.into_iter().find(|s| s.key() == key)
    }
}

/// A setting a hands-free press switches ([`Action::Toggle`]): the click, END STOP, FIXED, RETAKE, AUTO REC
/// and each input send. The engine owns each one's value; a press says "switch it" and the engine reads
/// the value it switches when the press applies, so two producers (a pedal, a click on screen) never undo
/// each other by sending what each last saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Toggle {
    Click,
    EndStop,
    Fixed,
    Retake,
    AutoRec,
    Send(InputSend),
}

impl Toggle {
    pub const COUNT: usize = 8;
    pub const ALL: [Toggle; Toggle::COUNT] = [
        Toggle::Click,
        Toggle::EndStop,
        Toggle::Fixed,
        Toggle::Retake,
        Toggle::AutoRec,
        Toggle::Send(InputSend::Echo),
        Toggle::Send(InputSend::Reverb),
        Toggle::Send(InputSend::Ring),
    ];

    /// Its position in [`Toggle::ALL`].
    pub fn index(self) -> usize {
        match self {
            Toggle::Click => 0,
            Toggle::EndStop => 1,
            Toggle::Fixed => 2,
            Toggle::Retake => 3,
            Toggle::AutoRec => 4,
            Toggle::Send(send) => 5 + send as usize,
        }
    }

    /// The setter that sets it to `on` outright: what initialization and a settings replay send, and
    /// what the host's settings memory keeps for it.
    pub fn setter(self, on: bool) -> Command {
        match self {
            Toggle::Click => Command::SetMetronome(on),
            Toggle::EndStop => Command::SetLoopEndStop(on),
            Toggle::Fixed => Command::SetFixedLength(on),
            Toggle::Retake => Command::SetRetake(on),
            Toggle::AutoRec => Command::SetAutoRecord(on),
            Toggle::Send(send) => Command::SetInputSend(send, on),
        }
    }
}

/// An input send's parameter. The echo's time is an index into the lane delay's divisions
/// (`dsp::fx::DIVISIONS`: 1/4, 1/8, 1/8., 1/16); the ring's frequency is its carrier's, in Hz;
/// feedback and the levels are linear.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputSendParam {
    EchoTime,
    EchoFeedback,
    EchoLevel,
    ReverbLevel,
    RingFreq,
    RingLevel,
}

impl InputSendParam {
    pub const ALL: [InputSendParam; 6] = [
        InputSendParam::EchoTime,
        InputSendParam::EchoFeedback,
        InputSendParam::EchoLevel,
        InputSendParam::ReverbLevel,
        InputSendParam::RingFreq,
        InputSendParam::RingLevel,
    ];

    /// The key the UI sends (`src/platform/engine-wire.ts`).
    pub fn key(self) -> &'static str {
        ["echoTime", "echoFeedback", "echoLevel", "reverbLevel", "ringFreq", "ringLevel"][self as usize]
    }

    pub fn from_key(key: &str) -> Option<InputSendParam> {
        InputSendParam::ALL.into_iter().find(|p| p.key() == key)
    }

    pub fn send(self) -> InputSend {
        match self {
            InputSendParam::EchoTime | InputSendParam::EchoFeedback | InputSendParam::EchoLevel => InputSend::Echo,
            InputSendParam::ReverbLevel => InputSend::Reverb,
            InputSendParam::RingFreq | InputSendParam::RingLevel => InputSend::Ring,
        }
    }

    /// Minimum, maximum and default (the UI's too: `src/ui/state/engine-store.ts`).
    pub fn range(self) -> (f64, f64, f64) {
        match self {
            InputSendParam::EchoTime => (0.0, (crate::dsp::fx::DIVISIONS.len() - 1) as f64, 1.0),
            InputSendParam::EchoFeedback => (0.0, crate::dsp::fx::MAX_FEEDBACK, 0.4),
            InputSendParam::EchoLevel => (0.0, 1.0, 0.5),
            InputSendParam::ReverbLevel => (0.0, 1.0, 0.5),
            InputSendParam::RingFreq => (20.0, 1500.0, 440.0),
            InputSendParam::RingLevel => (0.0, 1.0, 0.5),
        }
    }
}

/// Where the notes go (`src/ui/state/instrument.ts`: the active slot's synth, or its plugin).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoteTarget {
    Builtin(Instrument),
    /// The plugin in this slot (0..SLOT_COUNT).
    Slot(u8),
    /// Nowhere: a note sounds nothing (a slot whose source is off).
    Off,
}

/// The six built-in instruments (`src/ui/state/instruments.ts`).
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

/// The named hands-free actions (`src/app/actions.ts`; GO LIVE stays with the plugin host). A lane action
/// acts on the lane the engine has selected when it applies the press (`Command::Action`), or on a named
/// one (`Command::ActionOn`), so a press right after NEXT TRACK acts on the new lane whatever the UI has
/// seen of the selection yet.
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
    /// MUTE on or off (answered by [`Event::Muted`]; the UI follows the lane's [`Event::Mix`]).
    Mute,
    Reverse,
    /// COPY into the first EMPTY lane.
    Copy,
    /// TRIM to the first half of the loop's whole bars, rounded down, as the loop stands when the press
    /// applies; refused as [`Command::Trim`] is.
    Halve,
    /// HOLD's press on the selected lane (`Command::Action`) or a named one (`Command::ActionOn`), by the
    /// control (pedal) the UI numbers it with: REC/DUB, and once the press is accepted, that lane is where
    /// the same control's [`Action::Release`] without a lane acts. A refused press remembers nothing for
    /// its control and never touches another control's lane. A control number past [`HOLD_CONTROLS`] is
    /// remembered nowhere: its release does nothing.
    Hold(u8),
    /// HOLD's release, by the control whose press it answers: ends the capture on its lane (a count-in, a
    /// boundary arm or AUTO listening is cancelled, as REC/DUB's stop cancels it), and does nothing once
    /// the lane no longer captures (a take that FIXED closed stays closed). As `Command::Action`, its lane
    /// is the one that control's last accepted [`Action::Hold`] acted on; as `Command::ActionOn`, the
    /// named one.
    Release(u8),
    /// FADE (all): every playing lane fades to silence over the fade's bars and stops on the bar line;
    /// a second press while they fade stops them at once.
    FadeAll,
    /// Switch a setting from the value it has when the press applies, answered by [`Event::Toggled`], or
    /// refused with a reason (FIXED, RETAKE and AUTO REC while a take records; FIXED while RETAKE rolls over
    /// a loop; AUTO REC once a loop has locked the tempo: `Looper::toggle_gate`), the refusal on the
    /// selected lane and nothing switched. A looper press: it disarms a pending pedal CLEAR, refused or
    /// not. No lane: as `Command::ActionOn` it acts as `Command::Action` does, its lane ignored. The
    /// setters (`Command::SetMetronome` and the rest) stay for initialization and a settings replay.
    Toggle(Toggle),
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
    /// A pending stop: at the loop end (END STOP), or where a fade ends.
    pub stop_at: Option<Frame>,
    /// FADE: the lane fades to silence and stops at `stop_at`.
    pub fading: bool,
    /// RETAKE: the 1-based pass in flight, 0 when the lane is not rolling.
    pub retake_pass: u32,
}

/// A lane's mix as the engine applied it: its volume, mute, DUB FEEDBACK and pan (its target, not where a
/// glide toward it has got to), and its FX chain's targets in [`FxKind::ALL`] order (filter, pitch,
/// stutter, delay, reverb). A snapshot carries one per lane ([`crate::SnapshotTrack`]), and the export's
/// wet master renders with it ([`crate::render`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LaneMix {
    pub volume: f32,
    pub muted: bool,
    pub dub_feedback: f32,
    /// -1 (hard left) to 1 (hard right); 0 is the centre, where the lane plays as it did before pan.
    pub pan: f32,
    pub fx: [FxState; 5],
}

impl Default for LaneMix {
    /// A fresh or cleared lane's: unity, unmuted, a plain sum, centred, every effect bypassed at its
    /// defaults.
    fn default() -> Self {
        LaneMix { volume: 1.0, muted: false, dub_feedback: 1.0, pan: 0.0, fx: default_fx_states() }
    }
}

/// A [`LaneMix`] as [`Event::Mix`] carries it: the FX params as f32, so an event stays under twice the
/// size it has without one (a whole `LaneMix` would make every slot of the event ring three times
/// larger). Equal compact mixes are the same mix to the feed: a change within an f32's precision is
/// not sent.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompactMix {
    pub volume: f32,
    pub muted: bool,
    pub dub_feedback: f32,
    pub pan: f32,
    pub fx: [CompactFx; 5],
}

/// One effect of a [`CompactMix`]: [`FxState`] with its params as f32.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompactFx {
    pub bypassed: bool,
    pub params: [f32; MAX_PARAMS],
}

impl From<&LaneMix> for CompactMix {
    fn from(m: &LaneMix) -> Self {
        CompactMix {
            volume: m.volume,
            muted: m.muted,
            dub_feedback: m.dub_feedback,
            pan: m.pan,
            fx: m.fx.map(|s| CompactFx { bypassed: s.bypassed, params: s.params.map(|p| p as f32) }),
        }
    }
}

impl CompactMix {
    /// The mix at full width, each FX param as its f32 prints (the shortest decimal that reads back as
    /// it), so a value sent with up to seven significant digits comes back as it was sent. Not for the
    /// audio thread: it formats.
    pub fn widen(&self) -> LaneMix {
        let wide = |p: f32| p.to_string().parse::<f64>().unwrap_or(p as f64);
        LaneMix {
            volume: self.volume,
            muted: self.muted,
            dub_feedback: self.dub_feedback,
            pan: self.pan,
            fx: self.fx.map(|s| FxState { bypassed: s.bypassed, params: s.params.map(wide) }),
        }
    }
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
    /// TRIM on a lane that records or overdubs.
    Capturing,
    /// TRIM with nothing to keep: no committed loop of two whole bars or more, or a bar count outside it.
    NoTrim,
    /// MUTE on an EMPTY lane.
    NoMute,
    /// REVERSE on a lane with no committed loop.
    NoReverse,
    /// COPY from a lane with no committed loop.
    NoCopy,
    /// COPY with no EMPTY lane to copy to.
    NoFreeLane,
    /// A press a fading lane cannot take (it stops where the fade ends).
    Fading,
    /// FADE with no lane playing.
    NoFade,
    /// FIXED switched while a take records or overdubs.
    FixedCapturing,
    /// FIXED switched while RETAKE, whose passes roll at the loop's length, overrides it over a loop.
    FixedRetake,
    /// RETAKE switched while a take records or overdubs (it is read at arm).
    RetakeCapturing,
    /// AUTO REC switched while a take records or overdubs.
    AutoRecCapturing,
    /// AUTO REC switched once a loop has locked the tempo (it only starts a first take).
    AutoRecLocked,
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
            Refusal::Capturing => "this track is recording, stop it first",
            Refusal::NoTrim => "nothing to trim, the loop needs two bars or more",
            Refusal::NoMute => "nothing to mute, record first",
            Refusal::NoReverse => "nothing to reverse, record first",
            Refusal::NoCopy => "nothing to copy, record first",
            Refusal::NoFreeLane => "no empty track to copy to",
            Refusal::Fading => "fading out, wait or stop now",
            Refusal::NoFade => "nothing is playing to fade",
            Refusal::FixedCapturing => "a take is recording, FIXED changes after it",
            Refusal::FixedRetake => "RETAKE is on, so FIXED is ignored",
            Refusal::RetakeCapturing => "a take is recording, RETAKE changes after it",
            Refusal::AutoRecCapturing => "a take is recording, AUTO REC changes after it",
            Refusal::AutoRecLocked => "AUTO REC starts a first take, clear all to use it",
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
    /// COPY into lane `to` is done. `feedback` is the DUB FEEDBACK it copied, the source's when COPY
    /// applied (the source's may have moved since). The destination's whole mix follows as its
    /// `Mix`, which the UI and the host's settings memory read.
    Copied { frame: Frame, from: u8, to: u8, feedback: f32 },
    /// The lane was cleared: its loop gone, its volume, mute, pan and FX back to their defaults (CLEAR, a
    /// pedal's confirmed CLEAR, and every lane at CLEAR ALL, an empty one included). A lane that goes
    /// EMPTY any other way (a cancelled count-in, a stopped or rejected first take) keeps its mix.
    Cleared { frame: Frame, lane: u8 },
    /// A MUTE action ([`Action::Mute`]) switched the lane's mute. The UI shows the mute from the lane's
    /// [`Event::Mix`], not from this event.
    Muted { frame: Frame, lane: u8, on: bool },
    /// The lane's mix as the engine applies it ([`crate::Looper::mix`]), sent when it differs from the
    /// last one the ring took for the lane (a new engine sends every lane's once): a setting, a COPY, a
    /// CLEAR, a pedal's MUTE and a load all reach the feed this way; a pan's glide sends one, its target.
    /// The host's settings memory keeps the last one per lane as the mix it replays into a new engine.
    Mix { frame: Frame, lane: u8, mix: CompactMix },
    /// A toggled setting's value as the engine applies it: sent by an accepted [`Action::Toggle`] with the
    /// value it left, and whenever the value differs from the last one the event ring took for it (a
    /// setter changed it, or the full ring refused the toggle's event: offered again at the next publish).
    /// The UI shows the setting from it, and the host's settings memory keeps it as the value it replays.
    Toggled { frame: Frame, toggle: Toggle, on: bool },
}

/// The callback's view of the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessContext {
    /// Device frame of the block's first sample.
    pub frame: Frame,
    /// The device lost audio just before this block (an xrun): its first frame follows a gap, which
    /// spans the frames a jump in `frame` skipped. A point gap on a capture window's edge damages nothing.
    pub xrun: bool,
    /// This block's input never wholly reached the engine (the device side rendered silence or a splice
    /// in its place): the gap spans every frame of the block, so it damages every capture window the
    /// block overlaps, one that matches the block exactly included.
    pub damaged: bool,
    /// Input plus output latency the driver reports, in frames. A take starts this much (plus the
    /// plugin's latency and the master limiter's pre-delay) after its downbeat, so what the player heard
    /// and played lines up on the grid. A constant, not a user trim.
    pub align_frames: Frame,
    /// Of `align_frames`, the input side. A built-in instrument's note is played the output latency
    /// after the click the player heard, so its record path lags it by this (plus the plugin's latency,
    /// less the note's lead) to reach the grid where the guitar does.
    pub input_frames: Frame,
}

/// What a plugin slot holds, fixed when it is installed (the web UI's synth/effect kind:
/// `plugin-bridge.ts`, `inChannels > 0`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotKind {
    /// Has an audio input: takes the slot's input while the slot is live; its output is the wet signal
    /// (heard after the limiter, recorded at the take's alignment). Bypassed, it passes its input dry.
    Effect,
    /// No audio input: plays the notes while it is the note target; its output joins the master bus
    /// and is recorded where a built-in instrument's is. Bypassed, it is silent.
    Instrument,
}

/// A note for a plugin slot, `offset` frames into the `process` call that carries it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SlotEvent {
    pub offset: u32,
    pub kind: SlotEventKind,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SlotEventKind {
    /// Velocity 0..1.
    NoteOn { key: u8, velocity: f32 },
    NoteOff { key: u8 },
}

/// The plugin seam (Stage 4): one slot's processor, run inside the engine callback. The host builds and
/// activates it off the audio thread and installs it through the slot's [`crate::SlotPort`]; the
/// engine crossfades it in, and on removal crossfades it out, calls [`SlotProcessor::stop`] on the
/// audio thread and hands it back through the same port. The engine never drops a unit: a drop frees
/// memory and, for a plugin, calls into its DLL.
pub trait SlotProcessor: Send {
    /// Read once, at install.
    fn kind(&self) -> SlotKind;
    /// Frames the output lags its input (an effect) or a note (an instrument), as the plugin reported it
    /// when it was activated. Read once, at install: a plugin whose latency changes is restarted, which
    /// removes and reinstalls it.
    fn latency(&self) -> Frame;
    /// Render `out.len()` frames (= `input.len()`, at most the engine's `max_block`) from device frame
    /// `frame`. `input` is the slot's mono input while the slot is live and silence otherwise (always
    /// silence for an instrument); `events` are sorted by offset, every offset `< out.len()`. `out` is
    /// overwritten with the slot's mono output, every sample finite (the rack's render silences and counts
    /// a call that is not before routing it; the idle removal's one frame is not checked). Never
    /// allocates, locks or waits. Called on the audio
    /// thread, except one silent frame carrying a removal's released notes while no device runs (on the
    /// thread that holds the engine, just before [`SlotProcessor::stop`]).
    fn process(&mut self, frame: Frame, input: &[f32], events: &[SlotEvent], out: &mut [f32]);
    /// Called once, after the last `process` and before the unit leaves the engine (CLAP
    /// `stop_processing`, VST3 `setProcessing(0)`); on the audio thread while a device runs, or on the
    /// thread that holds the engine while none does. May come without any `process` before it.
    fn stop(&mut self);
    /// The host's way back to its concrete unit.
    fn into_any(self: Box<Self>) -> Box<dyn Any + Send>;
}
