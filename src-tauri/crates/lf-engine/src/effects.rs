//! OWNS: each lane's FX chain and the shared reverb bus they send to, the grid the chains' rhythmic
//! effects follow, and what CLEAR and COPY do to a lane's FX. Ported from `src/audio/fx/fx.ts` and the
//! FX side of `src/audio/looper/{playback,machine}.ts`.
//!
//! A lane plays through its chain (lane volume first, as the web's gain feeds the chain); the chain's
//! output goes to the master bus and its reverb send into the one [`ReverbBus`], whose stereo output
//! goes to the master bus too. Every chain and the bus are built in [`LaneFx::new`] and render every
//! block, so a delay or reverb tail rings on after its lane stops, as the web's chains do once built.
//!
//! The DSP runs on its own clock: the device frame less every frame the device skipped (an xrun that
//! jumps the frame counter), so its blocks always follow each other, as Blink's do. Methods take device
//! frames; [`LaneFx::set_offset`] keeps the difference.
//!
//! The chains take the looper's grid (its anchor and the tempo's beat) whenever it moves, where the
//! web handed it to a chain as its lane started: the grid moves only while no lane plays, so every
//! playing lane has the grid it would have had. (A device gap moves the anchor on the DSP clock while
//! lanes play; the re-timed grid keeps the device-frame phase.) CLEAR resets the lane's FX to the
//! defaults, COPY gives the copy the source's FX (`machine.ts` `clear`, `copy`). A parameter is clamped
//! to its def's range; a change sounds from the next quantum boundary, as a live Web Audio param does.

use std::sync::Arc;

use crate::api::TRACK_COUNT;
use crate::dsp::buffer_source::AudioBuffer;
use crate::dsp::fx::{default_fx_states, Ctl, FxChain, FxKind, FxParam, FxTiming, ReverbBus, REVERB_DECAY, REVERB_PRE_DELAY};
use crate::dsp::param::QUANTUM;
use crate::dsp::reverb_ir;
use crate::grid::{frames_per_bar, Frame};

/// The reverb IR's two draws (`makeReverbBus`'s `Math.random` calls). Fixed, so a render repeats.
const IR_DRAWS: [f64; 2] = [0.25, 0.75];

pub struct LaneFx {
    sample_rate: u32,
    /// One per lane, on the heap (a chain is ~80 kB of render buffers).
    chains: Vec<FxChain>,
    bus: ReverbBus,
    /// Device frames the DSP clock is behind.
    offset: Frame,
    /// The grid the chains hold: the anchor on the DSP clock and the tempo.
    timing: Option<(Frame, u32)>,
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

    pub fn set_param(&mut self, lane: usize, param: FxParam, value: f64, frame: Frame) {
        if value.is_finite() {
            let (def, ctl) = (param.def(), self.ctl(frame));
            self.chains[lane].set_param(param, value.clamp(def.min, def.max), ctl);
        }
    }

    pub fn set_bypass(&mut self, lane: usize, kind: FxKind, bypassed: bool, frame: Frame) {
        let ctl = self.ctl(frame);
        self.chains[lane].set_bypass(kind, bypassed, ctl);
    }

    /// CLEAR: the lane's FX back to the defaults.
    pub fn reset(&mut self, lane: usize, frame: Frame) {
        let ctl = self.ctl(frame);
        self.chains[lane].set_state(&default_fx_states(), ctl);
    }

    /// COPY: lane `to` takes lane `from`'s FX.
    pub fn copy(&mut self, from: usize, to: usize, frame: Frame) {
        let (state, ctl) = (self.chains[from].get_state(), self.ctl(frame));
        self.chains[to].set_state(&state, ctl);
    }

    /// Hand every chain the looper's grid if it moved: loop position 0 at `anchor`, a beat a quarter of
    /// a bar at `bpm`. No grid (`master` 0) leaves the last one.
    pub fn follow_grid(&mut self, anchor: Frame, master: Frame, bpm: u32, frame: Frame) {
        let anchor = anchor - self.offset;
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
    /// outputs and the bus's are added to `left`/`right`.
    pub fn render(&mut self, frame: Frame, lanes: &[[f32; QUANTUM]; TRACK_COUNT], left: &mut [f32], right: &mut [f32]) {
        let n = left.len();
        let f = self.dsp(frame);
        debug_assert!(n <= QUANTUM && (f % QUANTUM as u64) as usize + n <= QUANTUM, "a render crosses a quantum");
        let sends = &mut self.sends[..n];
        sends.fill(0.0);
        let mut any_send = false;
        for (chain, lane) in self.chains.iter_mut().zip(lanes) {
            let (out, send) = (&mut self.out[..n], &mut self.send[..n]);
            chain.process(f, &lane[..n], out, send);
            any_send |= !chain.send_silent();
            for (s, &x) in sends.iter_mut().zip(send.iter()) {
                *s += x;
            }
            for ((l, r), &x) in left.iter_mut().zip(right.iter_mut()).zip(out.iter()) {
                *l += x;
                *r += x;
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
