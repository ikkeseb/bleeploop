//! OWNS: the export's wet master, rendered offline by a fresh [`Engine`] the size of the session: the
//! live path by construction (the lanes, their FX chains, the reverb bus, the master volume and the
//! limiter), not a second mixer. The host renders it beside an export's snapshot
//! (`src-tauri/src/engine_io/session.rs`). Ported from the Tone render it replaced (`src/session/render.ts`
//! and `render-plan.ts` on Tone's OfflineContext, removed with Tone; git history keeps them at d53cd3d7).
//!
//! [`wet_master`] loads the session through the engine's own session path while no device runs
//! ([`Engine::service_session_idle`]) with every lane PLAYING (a lane saved STOPPED is in the master; a
//! muted one is out through its mute), then applies at the load's frame each lane's [`LaneMix`] (its
//! volume, mute and FX, as the snapshot took them at its pin) and, from the host's settings, the master's
//! volume and mute. Every other setting is skipped (a lane's mix there included, the click, the
//! instruments, the input sends, the plugin slots, the looper's modes), and the click is switched off, so
//! the render has no click, no instrument, no input sends, no plugin unit and a silent input. It renders
//! contiguous blocks on one clock from frame 0, so the output is the same at any block size.
//!
//! Warm-up: whole loop passes, at least one, that cover the longest lane's tail: its enabled delay's
//! echoes until they fall below [`TAIL_THRESHOLD`], each a division plus one render quantum after the
//! last, and when its reverb send is on too, the reverb's decay plus pre-delay after the last echo
//! (`warmup_passes`), read from the FX state the engine applied. The pass kept after them starts the
//! limiter's pre-delay late, so the master's frame 0 is loop position 0, lined up with the stems (the
//! Tone export kept whole passes, the pre-delay late).
//!
//! Known limits:
//! - A rhythmic FX whose pattern spans several loops (a dotted stutter over one-bar loops) keeps one
//!   phase of it, from a grid origin at loop position 0 (as the Tone render's), which can differ from live.
//! - A transient playback state (a FADE in progress, a pending END STOP) is not rendered: every lane
//!   plays at its stored mix.
//!
//! Not the audio thread: this allocates (an engine, a load's buffers, the output) and may take seconds.
//! Every refusal is an `Err` with a sentence; nothing here panics on a caller's input.

use crate::api::{Command, LaneMix, ProcessContext};
use crate::dsp::fx::{division_beats, FxKind, FxParam, FxState, DIVISIONS, REVERB_DECAY, REVERB_PRE_DELAY};
use crate::dsp::param::QUANTUM;
use crate::engine::{Engine, EngineConfig};
use crate::grid::{frames_per_bar, Frame, MAX_BPM, MIN_BPM};
use crate::session::{Load, SessionJob};

/// A delay's echoes are warmed until their level falls below this (the Tone render's `TAIL_THRESHOLD`).
pub const TAIL_THRESHOLD: f64 = 1e-4;
/// The block [`wet_master`] renders in.
pub const DEFAULT_BLOCK: usize = 1024;
/// The largest block [`wet_master_with`] accepts.
pub const MAX_BLOCK: usize = 1 << 16;
/// The sample rates a render accepts.
pub const MIN_RATE: u32 = 8_000;
pub const MAX_RATE: u32 = 384_000;
/// The longest loop a render accepts (twice the live engine's 60 s): the engine it builds holds eleven
/// buffers this long.
pub const MAX_LOOP_SECONDS: f64 = 120.0;

/// The rendered stereo master: `left` and `right` exactly the load's `master` frames each, frame 0 at
/// loop position 0.
#[derive(Clone, Debug, PartialEq)]
pub struct WetMaster {
    pub left: Vec<f32>,
    pub right: Vec<f32>,
    /// The warm-up passes rendered before the kept one.
    pub warmup: u32,
}

/// How [`wet_master_with`] renders. The output does not depend on `block`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderOptions {
    /// Frames per `process` call, 1 to [`MAX_BLOCK`].
    pub block: usize,
    /// Warm-up passes on top of the derived ones (a steady-state check).
    pub extra_warmup: u32,
}

impl Default for RenderOptions {
    fn default() -> Self {
        RenderOptions { block: DEFAULT_BLOCK, extra_warmup: 0 }
    }
}

/// Render `load`'s wet stereo master at `rate` with each lane's mix in `mixes` and the master's in
/// `settings` ([`wet_master_with`] at the default options).
pub fn wet_master(rate: u32, load: Load, mixes: &[LaneMix], settings: &[Command]) -> Result<WetMaster, String> {
    wet_master_with(rate, load, mixes, settings, RenderOptions::default())
}

/// Render `load`'s wet stereo master at `rate`. `load` is the session as the host's load builds it: each
/// lane's loop in the first `master` samples of its buffer, in buffer order (a reversed lane's play-order
/// PCM reversed back); a buffer only has to hold `master` samples, and `playing` is ignored (every lane
/// plays). `mixes` holds one mix per load track, in its order (a snapshot's tracks'). `settings` are the
/// commands the host keeps (its settings replay): only the master's volume and mute apply (the module
/// doc).
pub fn wet_master_with(rate: u32, mut load: Load, mixes: &[LaneMix], settings: &[Command], options: RenderOptions) -> Result<WetMaster, String> {
    if !(MIN_RATE..=MAX_RATE).contains(&rate) {
        return Err(format!("export render: the sample rate {rate} Hz is outside {MIN_RATE} to {MAX_RATE} Hz"));
    }
    if options.block == 0 || options.block > MAX_BLOCK {
        return Err(format!("export render: a block of {} frames is outside 1 to {MAX_BLOCK}", options.block));
    }
    if !(MIN_BPM..=MAX_BPM).contains(&load.bpm) || load.bars < 1 {
        return Err(format!("export render: {} BPM or {} bars is out of range", load.bpm, load.bars));
    }
    let master = load.master;
    if master <= 0 || master as f64 > MAX_LOOP_SECONDS * rate as f64 {
        return Err(format!("export render: a loop of {master} frames is not between 1 frame and {MAX_LOOP_SECONDS} seconds"));
    }
    if load.bars.checked_mul(frames_per_bar(load.bpm as f64, rate)) != Some(master) {
        return Err(format!("export render: {} bars at {} BPM are not {master} frames at {rate} Hz", load.bars, load.bpm));
    }
    if mixes.len() != load.tracks.len() {
        return Err(format!("export render: {} mixes for {} tracks", mixes.len(), load.tracks.len()));
    }
    let samples = master as usize;
    for track in &load.tracks {
        if track.buf.len() < samples {
            return Err(format!("export render: track {} holds {} samples, fewer than the loop's {samples}", track.index, track.buf.len()));
        }
        if !track.buf[..samples].iter().all(|x| x.is_finite()) {
            return Err(format!("export render: track {} holds a sample that is not a finite number", track.index));
        }
    }

    // The engine the size of the session: its lane buffers hold the loop, no more.
    let config = EngineConfig { max_loop_seconds: master as f64 / rate as f64, max_block: options.block, ..EngineConfig::new(rate) };
    let (mut engine, mut handle) = Engine::new(config);
    let capacity = engine.looper().capacity();
    if capacity < master {
        return Err(format!("export render: the engine holds {capacity} frames, fewer than the loop's {master}"));
    }
    let lanes: Vec<usize> = load.tracks.iter().map(|t| t.index as usize).collect();
    for track in &mut load.tracks {
        track.buf.resize(capacity as usize, 0.0);
        track.playing = true;
    }
    load.result = None;
    if handle.session.send(Box::new(SessionJob::Load(load))).is_err() {
        return Err("export render: the engine refused the load".to_string());
    }
    engine.service_session_idle();
    match handle.session.returned().map(|job| *job) {
        Some(SessionJob::Load(Load { result: Some(Ok(())), .. })) => {}
        Some(SessionJob::Load(Load { result: Some(Err(e)), .. })) => return Err(format!("export render: the session does not load: {}", e.text())),
        _ => return Err("export render: the load came back unfinished".to_string()),
    }

    // The mix, on the load's frame after it (which set each lane's from its track): each lane's from
    // `mixes`, which wins, the master's from the settings.
    let lane_mixes = lanes.iter().zip(mixes).flat_map(|(&i, mix)| lane_commands(i as u8, mix));
    for command in lane_mixes.chain(settings.iter().copied().filter(is_master_mix)) {
        if !engine.apply_idle(command) {
            return Err(format!("export render: the setting {command:?} did not apply"));
        }
    }
    if !engine.apply_idle(Command::SetMetronome(false)) {
        return Err("export render: the click did not switch off".to_string());
    }

    let states: Vec<[FxState; 5]> = lanes.iter().filter(|&&i| i < crate::api::TRACK_COUNT).map(|&i| engine.fx().chain(i).get_state()).collect();
    let bpm = engine.clock().bpm();
    let beat_seconds = frames_per_bar(bpm as f64, rate) as f64 / 4.0 / rate as f64;
    let warmup = warmup_passes(&states, beat_seconds, master as f64 / rate as f64, rate)
        .checked_add(options.extra_warmup)
        .ok_or("export render: too many warm-up passes")?;

    let latency = engine.limiter_latency();
    let keep_from = (warmup as Frame).checked_mul(master).and_then(|f| f.checked_add(latency)).ok_or("export render: the warm-up is too long")?;
    let total = keep_from.checked_add(master).ok_or("export render: the warm-up is too long")?;

    let mut left = vec![0.0f32; samples];
    let mut right = vec![0.0f32; samples];
    let silent = vec![0.0f32; options.block];
    let mut block = [vec![0.0f32; options.block], vec![0.0f32; options.block]];
    let mut f: Frame = 0;
    while f < total {
        let n = (options.block as Frame).min(total - f) as usize;
        let ctx = ProcessContext { frame: f, xrun: false, damaged: false, align_frames: 0, input_frames: 0 };
        let [bl, br] = &mut block;
        engine.process(&ctx, &silent[..n], &mut bl[..n], &mut br[..n]);
        // Keep what falls in [keep_from, total).
        let from = keep_from.max(f);
        let to = f + n as Frame;
        if from < to {
            let (a, b) = ((from - f) as usize, (to - f) as usize);
            let at = (from - keep_from) as usize;
            left[at..at + b - a].copy_from_slice(&bl[a..b]);
            right[at..at + b - a].copy_from_slice(&br[a..b]);
        }
        // Nothing reads the feed: empty it so the ring never fills.
        while handle.events.pop().is_ok() {}
        f = to;
    }
    Ok(WetMaster { left, right, warmup })
}

/// A setting the render applies: the master's volume and mute (a lane's mix is the snapshot's).
fn is_master_mix(command: &Command) -> bool {
    matches!(command, Command::SetMasterVolume(_) | Command::SetMasterMute(_))
}

/// The commands that give lane `lane` the mix `mix`: its volume and mute, then each effect's params and
/// its bypass, in chain order (as `FxChain::set_state` sets them). DUB FEEDBACK writes nothing here.
fn lane_commands(lane: u8, mix: &LaneMix) -> Vec<Command> {
    let mut out = vec![Command::SetVolume(lane, mix.volume), Command::SetMute(lane, mix.muted)];
    for kind in FxKind::ALL {
        let state = mix.fx[kind.index()];
        for (def, &value) in kind.params().iter().zip(&state.params) {
            out.extend(FxParam::from_key(kind, def.key).map(|param| Command::SetFxParam(lane, param, value)));
        }
        out.push(Command::SetFxBypass(lane, kind, state.bypassed));
    }
    out
}

/// Whole loop passes (at least one) that cover the session's longest tail: on each lane, its enabled
/// delay's echoes until they fall below [`TAIL_THRESHOLD`] and, when its reverb send is on too, the
/// reverb's tail (its decay plus pre-delay) after the last of them: the send follows the delay in the
/// lane's chain (`FxChain::process`), so the reverb rings on the echoes. Each echo recurs a division
/// plus one render quantum after the last: the feedback reaches the delay a quantum late
/// (`dsp/fx/delay.rs`). An echo's level is the feedback to the power of the recurrences before it.
fn warmup_passes(lanes: &[[FxState; 5]], beat_seconds: f64, loop_seconds: f64, rate: u32) -> u32 {
    let quantum = QUANTUM as f64 / rate as f64;
    let mut tail: f64 = 0.0;
    for fx in lanes {
        let delay = fx[FxKind::Delay.index()];
        let [time, feedback, mix] = delay.params;
        let echoes = if !delay.bypassed && mix > 0.0 {
            let recurrence = division_beats((time.max(0.0) as usize).min(DIVISIONS.len() - 1)) * beat_seconds + quantum;
            let repeats = if feedback > 0.0 { (TAIL_THRESHOLD.ln() / feedback.ln()).ceil().max(1.0) } else { 1.0 };
            recurrence * repeats
        } else {
            0.0
        };
        let reverb = fx[FxKind::Reverb.index()];
        let decay = if !reverb.bypassed && reverb.params[0] > 0.0 { REVERB_DECAY + REVERB_PRE_DELAY } else { 0.0 };
        tail = tail.max(echoes + decay);
    }
    (tail / loop_seconds).ceil().max(1.0) as u32
}
