//! OWNS: the built-in chain units implementing [`UnitProcessor`] (`docs/unit-contract.md` §5).
//!
//! This first slice (E3a) holds ECHO ([`EchoUnit`]) and RING MOD ([`RingUnit`]), each reusing
//! [`crate::input_fx`]'s per-channel DSP without rewriting it so that alone, with equal channels,
//! each renders today's input send bit for bit.
//!
//! Each is a zero-latency stereo insert whose output is `input + wet` per channel:
//! - [`EchoUnit`] runs one independent feedback delay line per channel (two ECHOs are two lines),
//!   tempo-synced from [`UnitCx::bpm`] and [`UnitCx::meter`], with 20 ms ramps from the next
//!   128-frame DSP quantum boundary.
//! - [`RingUnit`] runs one carrier-modulated wet path per channel with the same 20 ms ramps.
//! - Both bypass themselves (`set_bypass` returns `true`): the input gate closes over 20 ms from the
//!   command's frame, the dry passes untouched, ECHO's tail rings out, and once settled (or at level 0)
//!   the output is the input bit for bit. Before a unit renders its first frame (an install that
//!   arrives bypassed), `set_bypass` settles the starting gate immediately with no 20 ms leak.
//!
//! Everything is allocated in `new`; `process`, `set_param`, `set_bypass` and `stop` never allocate,
//! lock or block (invariant 5).

use std::any::Any;

use crate::api::{InputSendParam, SlotEvent, UnitCx, UnitInfo, UnitKind, UnitParam, UnitProcessor};
use crate::dsp::fx::Ctl;
use crate::dsp::param::QUANTUM;
use crate::grid::frames_per_bar;
use crate::input_fx::{Echo, Ring, START_BEAT_PERIOD};

/// Stereo ECHO insert unit (`UnitKind::Echo`): one [`Echo`] feedback delay line per channel.
pub struct EchoUnit {
    sample_rate: u32,
    /// Quarter-note period (as f64 bits) last applied to both channels.
    beat_period: Option<u64>,
    channels: [Echo; 2],
    /// The DSP quantum whose control values are computed.
    prepared: Option<u64>,
    /// At least one frame has rendered: from here `set_bypass` ramps over 20 ms (before it, sets the
    /// starting state settled).
    rendered: bool,
    wet: [f32; QUANTUM],
}

impl EchoUnit {
    /// Allocates both channels' 2 s delay lines: build off the audio thread. Starts active (not
    /// bypassed) at the default ECHO parameters (`1/8`, feedback `0.4`, level `0.5`).
    pub fn new(sample_rate: u32) -> Self {
        let sr = sample_rate as f32;
        let start = Ctl::at(0, sr);
        EchoUnit {
            sample_rate,
            beat_period: Some(START_BEAT_PERIOD.to_bits()),
            channels: [Echo::new_on(sr, start), Echo::new_on(sr, start)],
            prepared: None,
            rendered: false,
            wet: [0.0; QUANTUM],
        }
    }

    fn ctl(&self, cx: &UnitCx) -> Ctl {
        Ctl::at(cx.dsp_frame.max(0) as u64, self.sample_rate as f32)
    }

    fn follow_tempo(&mut self, cx: &UnitCx) {
        if !(cx.bpm > 0.0 && cx.bpm.is_finite()) {
            return;
        }
        let sr = self.sample_rate as f64;
        let beat_period = frames_per_bar(cx.bpm, self.sample_rate, cx.meter) as f64 / sr / cx.meter.quarters_per_bar();
        if self.beat_period == Some(beat_period.to_bits()) {
            return;
        }
        self.beat_period = Some(beat_period.to_bits());
        let ctl = self.ctl(cx);
        for ch in &mut self.channels {
            ch.set_beat_period(beat_period, ctl);
        }
    }
}

impl UnitProcessor for EchoUnit {
    fn info(&self) -> UnitInfo {
        UnitInfo { kind: UnitKind::Echo, source: false, latency: 0 }
    }

    fn process(&mut self, cx: &UnitCx, input: [&[f32]; 2], _events: &[SlotEvent], out: [&mut [f32]; 2]) {
        self.follow_tempo(cx);
        let len = out[0].len();
        if len == 0 {
            return;
        }
        self.rendered = true;
        let [ol, or] = out;
        let q = QUANTUM as u64;
        let start = cx.dsp_frame.max(0) as u64;
        let mut done = 0;
        while done < len {
            let f = start + done as u64;
            let quantum_start = f - f % q;
            if self.prepared != Some(quantum_start) {
                for ch in &mut self.channels {
                    ch.begin_quantum(quantum_start);
                }
                self.prepared = Some(quantum_start);
            }
            let at = (f - quantum_start) as usize;
            let n = (QUANTUM - at).min(len - done);
            let span = done..done + n;
            let wet = &mut self.wet[..n];
            for (ch, (x, y)) in self.channels.iter_mut().zip([(&input[0][span.clone()], &mut ol[span.clone()]), (&input[1][span.clone()], &mut or[span])]) {
                if ch.process(at, x, wet) && ch.has_level() {
                    for (o, (&dry, &w)) in y.iter_mut().zip(x.iter().zip(wet.iter())) {
                        *o = dry + w;
                    }
                } else {
                    y.copy_from_slice(x);
                }
            }
            done += n;
        }
    }

    fn set_param(&mut self, cx: &UnitCx, param: UnitParam, value: f64) -> Option<f64> {
        let UnitParam::Send(p @ (InputSendParam::EchoTime | InputSendParam::EchoFeedback | InputSendParam::EchoLevel)) = param else {
            return None;
        };
        if !value.is_finite() {
            return None;
        }
        self.follow_tempo(cx);
        let ctl = self.ctl(cx);
        let applied = self.channels[0].set_param(p, value, ctl);
        self.channels[1].set_param(p, value, ctl);
        Some(applied)
    }

    fn set_bypass(&mut self, cx: &UnitCx, bypassed: bool) -> bool {
        let on = !bypassed;
        if !self.rendered {
            for ch in &mut self.channels {
                ch.snap_on(on);
            }
            return true;
        }
        if self.channels[0].is_on() == on {
            return true;
        }
        let ctl = self.ctl(cx);
        for ch in &mut self.channels {
            ch.set_on(on, ctl);
        }
        true
    }

    fn stop(&mut self) {}

    fn into_any(self: Box<Self>) -> Box<dyn Any + Send> {
        self
    }
}

/// Stereo RING MOD insert unit (`UnitKind::Ring`): one [`Ring`] modulator per channel.
pub struct RingUnit {
    sample_rate: u32,
    channels: [Ring; 2],
    /// The DSP quantum whose control values are computed.
    prepared: Option<u64>,
    /// At least one frame has rendered: from here `set_bypass` ramps over 20 ms (before it, sets the
    /// starting state settled).
    rendered: bool,
    wet: [f32; QUANTUM],
}

impl RingUnit {
    /// Allocates both channels' parameter timelines: build off the audio thread. Starts active (not
    /// bypassed) at the default RING MOD parameters (`440 Hz`, level `0.5`).
    pub fn new(sample_rate: u32) -> Self {
        let sr = sample_rate as f32;
        let start = Ctl::at(0, sr);
        RingUnit {
            sample_rate,
            channels: [Ring::new_on(sr, start), Ring::new_on(sr, start)],
            prepared: None,
            rendered: false,
            wet: [0.0; QUANTUM],
        }
    }

    /// Each channel's carrier phase at the next frame it renders, in radians in [0, 2π).
    pub fn phase(&self) -> [f64; 2] {
        [self.channels[0].phase(), self.channels[1].phase()]
    }

    fn ctl(&self, cx: &UnitCx) -> Ctl {
        Ctl::at(cx.dsp_frame.max(0) as u64, self.sample_rate as f32)
    }
}

impl UnitProcessor for RingUnit {
    fn info(&self) -> UnitInfo {
        UnitInfo { kind: UnitKind::Ring, source: false, latency: 0 }
    }

    fn process(&mut self, cx: &UnitCx, input: [&[f32]; 2], _events: &[SlotEvent], out: [&mut [f32]; 2]) {
        let len = out[0].len();
        if len == 0 {
            return;
        }
        self.rendered = true;
        let [ol, or] = out;
        let q = QUANTUM as u64;
        let start = cx.dsp_frame.max(0) as u64;
        let mut done = 0;
        while done < len {
            let f = start + done as u64;
            let quantum_start = f - f % q;
            if self.prepared != Some(quantum_start) {
                for ch in &mut self.channels {
                    ch.begin_quantum(quantum_start);
                }
                self.prepared = Some(quantum_start);
            }
            let at = (f - quantum_start) as usize;
            let n = (QUANTUM - at).min(len - done);
            let span = done..done + n;
            let wet = &mut self.wet[..n];
            for (ch, (x, y)) in self.channels.iter_mut().zip([(&input[0][span.clone()], &mut ol[span.clone()]), (&input[1][span.clone()], &mut or[span])]) {
                if ch.process(at, x, wet) && ch.has_level() {
                    for (o, (&dry, &w)) in y.iter_mut().zip(x.iter().zip(wet.iter())) {
                        *o = dry + w;
                    }
                } else {
                    y.copy_from_slice(x);
                }
            }
            done += n;
        }
    }

    fn set_param(&mut self, cx: &UnitCx, param: UnitParam, value: f64) -> Option<f64> {
        let UnitParam::Send(p @ (InputSendParam::RingFreq | InputSendParam::RingLevel)) = param else {
            return None;
        };
        if !value.is_finite() {
            return None;
        }
        let ctl = self.ctl(cx);
        let applied = self.channels[0].set_param(p, value, ctl);
        self.channels[1].set_param(p, value, ctl);
        Some(applied)
    }

    fn set_bypass(&mut self, cx: &UnitCx, bypassed: bool) -> bool {
        let on = !bypassed;
        if !self.rendered {
            for ch in &mut self.channels {
                ch.snap_on(on);
            }
            return true;
        }
        if self.channels[0].is_on() == on {
            return true;
        }
        let ctl = self.ctl(cx);
        for ch in &mut self.channels {
            ch.set_on(on, ctl);
        }
        true
    }

    fn stop(&mut self) {}

    fn into_any(self: Box<Self>) -> Box<dyn Any + Send> {
        self
    }
}
