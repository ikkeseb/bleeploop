//! OWNS: each lane's FX chain and the shared reverb bus they send to, the grid the chains' rhythmic
//! effects follow, and what CLEAR and COPY do to a lane's FX. Ported from `fx/fx.ts` and the
//! FX side of `looper/{playback,machine}.ts`.
//!
//! A lane plays through its chain (lane volume first, as the web's gain feeds the chain); the chain's
//! output goes to the master bus and its reverb send into the one [`ReverbBus`], whose stereo output
//! goes to the master bus too. Every chain and the bus are built in [`LaneFx::new`] and render every
//! block, so a delay or reverb tail rings on after its lane stops, as the web's chains do once built. A
//! lane that FADEs (the engine's own, no web counterpart) takes its returns down with it: its delay feeds
//! its echoes back under the fade's ramp ([`LaneFx::render`]).
//!
//! The DSP runs on its own clock: the device frame less every frame the device skipped (an xrun that
//! jumps the frame counter), so its blocks always follow each other, as Blink's do. Methods take device
//! frames; [`LaneFx::set_offset`] keeps the difference.
//!
//! The chains take the looper's beat grid (its origin and the tempo's beat) whenever it moves, where the
//! web handed the loop's anchor to a chain as its lane started: the grid moves only while no lane plays
//! (a first take, an import, an idle restart), so every playing lane has the grid it would have had. A
//! multiply re-anchors the loop, not the beat grid (`Looper::grid_origin`): a stutter's dotted gate keeps
//! its phase across it. (A device gap moves the origin on the DSP clock while lanes play; the re-timed
//! grid keeps the device-frame phase.) CLEAR resets the lane's FX to the
//! defaults, COPY gives the copy the source's FX (`machine.ts` `clear`, `copy`), and a load each loaded
//! lane its session's ([`LaneFx::set_state`]). CLEAR and a load also silence the lane's chain history
//! at their frame (the delay line and its feedback, the filter's memory, the PitchShift's lines;
//! [`LaneFx::clear_history`], no web counterpart): a CLEAR cuts its loop's echoes there as it cuts the
//! loop, and nothing from before either comes back through a delay turned on later. A parameter is clamped
//! to its def's range; a change sounds from the next quantum boundary, as a live Web Audio param does.
//!
//! Each lane's pan ([`pan_gains`], the engine's own) places the chain's direct output between the two
//! sides; the reverb send is taken before it and stays unpanned. A `SetPan` glides the position frame by
//! frame (`crate::glide`, over [`PAN_TAU_SECONDS`]) and each frame plays at the gains of the position it
//! has reached, so a move lands on the same frame at any block size; a settled position plays at gains
//! computed once, and the centre adds the direct output as it did before pan, bit for bit. Construction,
//! CLEAR (centre), COPY (the source's target) and a load (its track's) set the position where they put
//! the target: no glide from the old one.

use std::f64::consts::{FRAC_PI_4, SQRT_2};
use std::sync::Arc;

use crate::api::TRACK_COUNT;
use crate::dsp::buffer_source::AudioBuffer;
use crate::dsp::fx::{default_fx_states, Ctl, FxChain, FxKind, FxParam, FxParamDef, FxState, FxTiming, ReverbBus, REVERB_DECAY, REVERB_PRE_DELAY};
use crate::dsp::param::QUANTUM;
use crate::dsp::reverb_ir;
use crate::grid::{frames_per_bar, Frame};

/// `value` within `def`'s range, an integer param rounded half up first.
fn clamped(def: &FxParamDef, value: f64) -> f64 {
    let value = if def.integer { (value + 0.5).floor() } else { value };
    value.clamp(def.min, def.max)
}

/// The reverb IR's two draws (`makeReverbBus`'s `Math.random` calls). Fixed, so a render repeats.
const IR_DRAWS: [f64; 2] = [0.25, 0.75];

/// A pan move's time constant: a lane's volume's (`looper`).
pub const PAN_TAU_SECONDS: f64 = 0.01;

/// The pan law: the left and right gains at pan `p` (-1 hard left, 0 centre, 1 hard right), constant
/// power normalised to the centre: `L = sqrt(2) cos(t)`, `R = sqrt(2) sin(t)`, `t = (p + 1) pi / 4`. The
/// centre is exactly `[1, 1]` (a lane plays there as it did before pan) and a hard pan exactly
/// `[sqrt(2), 0]` or `[0, sqrt(2)]`: +3 dB on one side, silence on the other. `L^2 + R^2 = 2` throughout.
/// A pan outside -1..1 is taken at the nearer end.
pub fn pan_gains(p: f64) -> [f64; 2] {
    if p == 0.0 {
        [1.0, 1.0]
    } else if p >= 1.0 {
        [0.0, SQRT_2]
    } else if p <= -1.0 {
        [SQRT_2, 0.0]
    } else {
        let t = (p + 1.0) * FRAC_PI_4;
        [SQRT_2 * t.cos(), SQRT_2 * t.sin()]
    }
}

/// A lane's pan as a `SetPan` value lands: clamped to -1..1 (-0 as 0); a value that is no number, the
/// centre.
fn pan_target(p: f32) -> f32 {
    if p.is_finite() { p.clamp(-1.0, 1.0) + 0.0 } else { 0.0 }
}

/// One lane's pan: its target, the position a glide has reached, and the gains there once it rests.
#[derive(Clone, Copy, Debug)]
struct Pan {
    target: f32,
    at: f64,
    /// `pan_gains(at)` while `at` rests on `target`; computed when it lands there.
    gains: [f64; 2],
}

impl Pan {
    /// Resting on `target`, with no glide from where it was.
    fn seeded(target: f32) -> Pan {
        let target = pan_target(target);
        Pan { target, at: target as f64, gains: pan_gains(target as f64) }
    }
}

pub struct LaneFx {
    sample_rate: u32,
    /// One per lane, on the heap (a chain is ~80 kB of render buffers).
    chains: Vec<FxChain>,
    bus: ReverbBus,
    /// Device frames the DSP clock is behind.
    offset: Frame,
    /// The grid the chains hold: the origin on the DSP clock and the tempo.
    timing: Option<(Frame, u32)>,
    pans: [Pan; TRACK_COUNT],
    pan_coef: f64,
    /// Per lane, its chain's output over the chunk [`LaneFx::render`] last rendered: `[min, max]`,
    /// spanning zero; what the engine folds into the scope's columns ([`crate::scope`]). Written only
    /// while the taps are on ([`LaneFx::set_env`]), so a closed stage view renders exactly what it
    /// rendered before the taps existed.
    env: [[f32; 2]; TRACK_COUNT],
    env_on: bool,
    out: [f32; QUANTUM],
    send: [f32; QUANTUM],
    sends: [f32; QUANTUM],
    wet: [[f32; QUANTUM]; 2],
}

/// The bus reverb's IR, from `white`, Tone's white noise table (the one the drum kit plays too:
/// `Instruments::new`). Allocates: `Engine::new` builds it once, for this bus and the input sends'
/// reverb (`input_fx`).
pub fn reverb_ir(sample_rate: u32, white: &Arc<AudioBuffer>) -> [Vec<f32>; 2] {
    reverb_ir::generate(white, sample_rate as f32, REVERB_DECAY, REVERB_PRE_DELAY, IR_DRAWS)
}

impl LaneFx {
    /// Allocates (the chains and the bus's convolver over `ir`, [`reverb_ir`]): build it off the audio
    /// thread.
    pub fn new(sample_rate: u32, ir: [&[f32]; 2]) -> Self {
        let sr = sample_rate as f32;
        let start = Ctl::at(0, sr);
        LaneFx {
            sample_rate,
            chains: (0..TRACK_COUNT).map(|_| FxChain::new(sr, None, start)).collect(),
            bus: ReverbBus::new(sr, ir, 0),
            offset: 0,
            timing: None,
            pans: [Pan::seeded(0.0); TRACK_COUNT],
            pan_coef: (-1.0 / (PAN_TAU_SECONDS * sample_rate as f64)).exp(),
            env: [[0.0; 2]; TRACK_COUNT],
            env_on: false,
            out: [0.0; QUANTUM],
            send: [0.0; QUANTUM],
            sends: [0.0; QUANTUM],
            wet: [[0.0; QUANTUM]; 2],
        }
    }

    pub fn set_offset(&mut self, offset: Frame) {
        self.offset = offset;
    }

    fn dsp(&self, frame: Frame) -> u64 {
        (frame - self.offset) as u64
    }

    fn ctl(&self, frame: Frame) -> Ctl {
        Ctl::at(self.dsp(frame), self.sample_rate as f32)
    }

    pub fn chain(&self, lane: usize) -> &FxChain {
        &self.chains[lane]
    }

    /// Clamped to the param's range; an integer param (semitones, a division index) rounds half up, as
    /// the UI's `Math.round` does, so a mix read back (`Looper::mix`) is one session.json's schema accepts.
    pub fn set_param(&mut self, lane: usize, param: FxParam, value: f64, frame: Frame) {
        if value.is_finite() {
            let ctl = self.ctl(frame);
            self.chains[lane].set_param(param, clamped(param.def(), value), ctl);
        }
    }

    /// A load: lane `lane` takes `states` whole, each param as [`LaneFx::set_param`] takes it (one that
    /// is no number keeps the chain's). Ramped as a CLEAR's and a COPY's are (`FxChain::set_state`).
    pub fn set_state(&mut self, lane: usize, states: &[FxState; 5], frame: Frame) {
        let mut states = *states;
        let current = self.chains[lane].get_state();
        for kind in FxKind::ALL {
            let (state, now) = (&mut states[kind.index()], &current[kind.index()]);
            for ((v, def), &old) in state.params.iter_mut().zip(kind.params()).zip(&now.params) {
                *v = if v.is_finite() { clamped(def, *v) } else { old };
            }
        }
        let ctl = self.ctl(frame);
        self.chains[lane].set_state(&states, ctl);
    }

    pub fn set_bypass(&mut self, lane: usize, kind: FxKind, bypassed: bool, frame: Frame) {
        let ctl = self.ctl(frame);
        self.chains[lane].set_bypass(kind, bypassed, ctl);
    }

    /// CLEAR: the lane's FX back to the defaults, its chain's history silenced ([`LaneFx::clear_history`]),
    /// its pan centred at once.
    pub fn reset(&mut self, lane: usize, frame: Frame) {
        let ctl = self.ctl(frame);
        self.chains[lane].set_state(&default_fx_states(), ctl);
        self.clear_history(lane, frame);
        self.pans[lane] = Pan::seeded(0.0);
    }

    /// Lane `lane`'s pan target (`Command::SetPan`): what its mix reports, not where a glide has got to.
    pub fn pan(&self, lane: usize) -> f32 {
        self.pans[lane].target
    }

    /// Each lane's min and max over the chunk [`LaneFx::render`] last rendered, after its FX and its
    /// fade and before its pan: what the scope folds into its columns ([`crate::scope`]). Stale while
    /// the taps are off.
    pub fn envelope(&self) -> &[[f32; 2]; TRACK_COUNT] {
        &self.env
    }

    /// `Command::SetScope`: take each lane's envelope every render, or stop taking it.
    pub(crate) fn set_env(&mut self, on: bool) {
        self.env_on = on;
    }

    /// Where lane `lane`'s pan has got to on its glide toward [`LaneFx::pan`].
    pub fn pan_position(&self, lane: usize) -> f64 {
        self.pans[lane].at
    }

    /// `SetPan`: lane `lane`'s pan glides from where it is to `pan` (clamped; no number centres it).
    pub fn set_pan(&mut self, lane: usize, pan: f32) {
        let p = &mut self.pans[lane];
        p.target = pan_target(pan);
        if p.at == p.target as f64 {
            p.gains = pan_gains(p.at);
        }
    }

    /// A load: lane `lane`'s pan is `pan` from this frame, with no glide.
    pub fn seed_pan(&mut self, lane: usize, pan: f32) {
        self.pans[lane] = Pan::seeded(pan);
    }

    /// From `frame` on, lane `lane`'s chain holds nothing of what it heard before (`FxChain::clear_history`):
    /// the erased loop's echoes never come back through a delay turned on or a feedback raised later.
    pub fn clear_history(&mut self, lane: usize, frame: Frame) {
        let f = self.dsp(frame);
        self.chains[lane].clear_history(f);
    }

    /// COPY: lane `to` takes lane `from`'s FX, and its pan target at once (no glide).
    pub fn copy(&mut self, from: usize, to: usize, frame: Frame) {
        let (state, ctl) = (self.chains[from].get_state(), self.ctl(frame));
        self.chains[to].set_state(&state, ctl);
        self.pans[to] = Pan::seeded(self.pans[from].target);
    }

    /// Hand every chain the looper's beat grid if it moved: its origin at `origin` (a device frame), a
    /// beat a quarter of a bar at `bpm`. No grid (`master` 0) leaves the last one.
    pub fn follow_grid(&mut self, origin: Frame, master: Frame, bpm: u32, frame: Frame) {
        let anchor = origin - self.offset;
        if master == 0 || self.timing == Some((anchor, bpm)) {
            return;
        }
        self.timing = Some((anchor, bpm));
        let sr = self.sample_rate as f64;
        let timing = FxTiming { anchor: anchor as f64 / sr, beat_period: frames_per_bar(bpm as f64, self.sample_rate) as f64 / sr / 4.0 };
        let ctl = self.ctl(frame);
        for chain in self.chains.iter_mut() {
            chain.set_timing(timing, ctl).expect("the looper's grid is finite with a positive beat");
        }
    }

    /// Render frames `frame..frame + n`, inside one quantum: lane `i`'s signal is `lanes[i]`; the chains'
    /// outputs, each at its pan's gains, and the bus's are added to `left`/`right`. A lane `fading` (FADE)
    /// has its ramp in `fades[i]`: its signal is already ramped, and its delay feeds its echoes back under
    /// the ramp too, so the echoes it held before the fade die with it instead of ringing on past the bar
    /// line at their old level (`FxChain::process_fading`). The shared reverb bus is fed the fading send,
    /// unpanned; a lane that does not fade is untouched.
    pub fn render(&mut self, frame: Frame, lanes: &[[f32; QUANTUM]; TRACK_COUNT], fading: [bool; TRACK_COUNT], fades: &[[f32; QUANTUM]; TRACK_COUNT], left: &mut [f32], right: &mut [f32]) {
        let n = left.len();
        let f = self.dsp(frame);
        debug_assert!(n <= QUANTUM && (f % QUANTUM as u64) as usize + n <= QUANTUM, "a render crosses a quantum");
        let sends = &mut self.sends[..n];
        sends.fill(0.0);
        let mut any_send = false;
        for (i, (chain, lane)) in self.chains.iter_mut().zip(lanes).enumerate() {
            let (out, send) = (&mut self.out[..n], &mut self.send[..n]);
            chain.process_fading(f, &lane[..n], fading[i].then(|| &fades[i][..n]), out, send);
            if self.env_on {
                // The lane as the player hears it before the master: after its FX and its fade, before
                // its pan (`env`, for the scope's columns). `out` is read once more here, so the fold
                // rides the cache line the chain just wrote.
                let mut env = [0.0f32; 2];
                for &x in out.iter() {
                    env[0] = env[0].min(x);
                    env[1] = env[1].max(x);
                }
                self.env[i] = env;
            }
            any_send |= !chain.send_silent();
            for (s, &x) in sends.iter_mut().zip(send.iter()) {
                *s += x;
            }
            let pan = &mut self.pans[i];
            let target = pan.target as f64;
            if pan.at == target && pan.gains == [1.0, 1.0] {
                for ((l, r), &x) in left.iter_mut().zip(right.iter_mut()).zip(out.iter()) {
                    *l += x;
                    *r += x;
                }
            } else if pan.at == target {
                let [gl, gr] = pan.gains;
                for ((l, r), &x) in left.iter_mut().zip(right.iter_mut()).zip(out.iter()) {
                    *l += (gl * x as f64) as f32;
                    *r += (gr * x as f64) as f32;
                }
            } else {
                for ((l, r), &x) in left.iter_mut().zip(right.iter_mut()).zip(out.iter()) {
                    let [gl, gr] = pan_gains(pan.at);
                    *l += (gl * x as f64) as f32;
                    *r += (gr * x as f64) as f32;
                    pan.at = crate::glide(pan.at, target, self.pan_coef);
                }
                if pan.at == target {
                    pan.gains = pan_gains(target);
                }
            }
        }
        let [wl, wr] = &mut self.wet;
        let (wl, wr) = (&mut wl[..n], &mut wr[..n]);
        self.bus.process(f, any_send.then_some(&*sends), wl, wr);
        for ((l, r), (&a, &b)) in left.iter_mut().zip(right.iter_mut()).zip(wl.iter().zip(wr.iter())) {
            *l += a;
            *r += b;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`LaneFx::render`] as it was before pan (`ff03884a`), every lane unfaded: each chain's output added
    /// to both sides, then the bus's.
    fn render_before_pan(fx: &mut LaneFx, frame: Frame, lanes: &[[f32; QUANTUM]; TRACK_COUNT], left: &mut [f32], right: &mut [f32]) {
        let n = left.len();
        let f = fx.dsp(frame);
        let LaneFx { chains, bus, out, send, sends, wet, .. } = fx;
        let sends = &mut sends[..n];
        sends.fill(0.0);
        let mut any_send = false;
        for (chain, lane) in chains.iter_mut().zip(lanes) {
            let (out, send) = (&mut out[..n], &mut send[..n]);
            chain.process_fading(f, &lane[..n], None, out, send);
            any_send |= !chain.send_silent();
            for (s, &x) in sends.iter_mut().zip(send.iter()) {
                *s += x;
            }
            for ((l, r), &x) in left.iter_mut().zip(right.iter_mut()).zip(out.iter()) {
                *l += x;
                *r += x;
            }
        }
        let [wl, wr] = wet;
        let (wl, wr) = (&mut wl[..n], &mut wr[..n]);
        bus.process(f, any_send.then_some(&*sends), wl, wr);
        for ((l, r), (&a, &b)) in left.iter_mut().zip(right.iter_mut()).zip(wl.iter().zip(wr.iter())) {
            *l += a;
            *r += b;
        }
    }

    /// Two copies of the same five busy lanes (every effect but the stutter on, the reverb bus fed): one
    /// rendered by [`LaneFx::render`], the other by the render before pan, `edit(fx, frame)` applied to
    /// both before each quantum (the reference ignores what it does to pans). Per quantum: whether the bits
    /// matched, and where the rendered copy's pans had got to after it.
    fn against_before_pan(mut edit: impl FnMut(&mut LaneFx, Frame)) -> Vec<(bool, [f64; TRACK_COUNT])> {
        const SR: u32 = 48_000;
        let ir: Vec<f32> = (0..9_600).map(|k| ((k * 7_919 % 1_000) as f32 / 1_000.0 - 0.5) * (-(k as f32) / 2_000.0).exp()).collect();
        let build = || {
            let mut fx = LaneFx::new(SR, [&ir, &ir]);
            for lane in 0..TRACK_COUNT {
                for kind in [FxKind::Filter, FxKind::Pitch, FxKind::Delay, FxKind::Reverb] {
                    fx.set_bypass(lane, kind, false, 0);
                }
                fx.set_param(lane, FxParam::Semitones, lane as f64 - 2.0, 0);
                fx.set_param(lane, FxParam::Feedback, 0.6, 0);
                fx.set_param(lane, FxParam::Amount, 0.5, 0);
            }
            fx
        };
        let (mut panned, mut before) = (build(), build());
        let mut lanes = [[0.0f32; QUANTUM]; TRACK_COUNT];
        let fading = [false; TRACK_COUNT];
        let fades = [[0.0f32; QUANTUM]; TRACK_COUNT];
        let (mut a, mut b) = ([[0.0f32; QUANTUM]; 2], [[0.0f32; QUANTUM]; 2]);
        (0..375)
            .map(|q| {
                let frame = (q * QUANTUM) as Frame;
                for (i, lane) in lanes.iter_mut().enumerate() {
                    for (k, x) in lane.iter_mut().enumerate() {
                        *x = ((((frame as usize + k) * (31 + 6 * i)) % 197) as f32 / 98.5 - 1.0) * 0.4;
                    }
                }
                edit(&mut panned, frame);
                edit(&mut before, frame);
                for side in a.iter_mut().chain(b.iter_mut()) {
                    side.fill(0.0);
                }
                let [al, ar] = &mut a;
                panned.render(frame, &lanes, fading, &fades, al, ar);
                let [bl, br] = &mut b;
                render_before_pan(&mut before, frame, &lanes, bl, br);
                let bits = |x: &[[f32; QUANTUM]; 2]| x.iter().flatten().map(|v| v.to_bits()).collect::<Vec<_>>();
                (bits(&a) == bits(&b), std::array::from_fn(|i| panned.pan_position(i)))
            })
            .collect()
    }

    /// The quanta in `from..` whose bits did not match.
    fn differing(quanta: &[(bool, [f64; TRACK_COUNT])], from: usize) -> Vec<usize> {
        (from..quanta.len()).filter(|&q| !quanta[q].0).collect()
    }

    #[test]
    fn a_centred_lane_plays_the_bits_it_played_before_pan() {
        let fresh = against_before_pan(|_, _| {});
        assert_eq!(differing(&fresh, 0), [0usize; 0], "a fresh engine's centred lanes");
        // A pan that moves away and comes back: once it rests on the centre again, the same bits.
        let moved = against_before_pan(|fx, frame| match frame {
            0 => fx.set_pan(1, 0.7),
            12_800 => fx.set_pan(1, 0.0),
            _ => {}
        });
        let panned = (1..100).filter(|&q| !moved[q].0).count();
        assert!(panned > 90, "while panned, the bits differ ({panned} of 99 quanta)");
        let back = moved.iter().position(|(_, at)| at[1] == 0.0).expect("the pan came back to the centre");
        assert!(back > 100, "it glided back ({back})");
        assert_eq!(differing(&moved, back + 1), [0usize; 0], "back on the centre: the bits of before pan");
        // A centre reached with no glide (a COPY of a centred lane over a panned one, a CLEAR): the same.
        let seeded = against_before_pan(|fx, frame| match frame {
            0 => fx.seed_pan(2, -1.0),
            6_400 => fx.copy(0, 2, frame),
            12_800 => fx.seed_pan(3, 0.4),
            19_200 => fx.reset(3, frame),
            _ => {}
        });
        // Some early quanta match (8 to 17 when this was written): lane 2's direct output is silent there.
        assert!((0..50).filter(|&q| !seeded[q].0).count() > 30, "lane 2 panned hard left");
        assert_eq!(differing(&seeded, 50).into_iter().filter(|&q| !(100..150).contains(&q)).collect::<Vec<_>>(), [0usize; 0], "a COPY's and a CLEAR's centre");
    }
}
