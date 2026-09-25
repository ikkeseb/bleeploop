//! The FX scenarios' replay: a `fx-*` scenario from the manifest through a fresh `FxChain`, at a chosen
//! block size, with its events scheduled ahead (the reference) or landing live mid-block. Every render
//! and every control runs under assert_no_alloc.

use assert_no_alloc::assert_no_alloc;
use lf_engine::dsp::fx::{Ctl, FxChain, FxKind, FxParam, FxState, FxTiming};
use lf_engine::dsp::param::QUANTUM;

use super::refs::{self, ScriptEvent};
use super::violation_count;

pub const BLOCKS: [usize; 5] = [1, 64, 127, 128, 480];

pub fn states(s: &refs::Scenario) -> [FxState; 5] {
    let json = s.setup["states"].as_array().expect("fx states");
    FxKind::ALL.map(|kind| {
        let entry = &json[kind.index()];
        let mut state = FxState::default_for(kind);
        state.bypassed = entry["bypassed"].as_bool().expect("bypassed");
        for (i, def) in kind.params().iter().enumerate() {
            state.params[i] = entry["params"][def.key].as_f64().expect("a param value");
        }
        state
    })
}

pub fn timing(s: &refs::Scenario) -> FxTiming {
    let t = &s.setup["timing"];
    FxTiming { anchor: t["anchor"].as_f64().expect("anchor"), beat_period: t["beatPeriod"].as_f64().expect("beatPeriod") }
}

/// Tone's offline clock at the tick on `frame` (a multiple of 128): 128 / rate added once per tick,
/// as `_renderClock` sums it.
pub fn tick_time(frame: u64, rate: u32) -> f64 {
    let mut t = 0.0f64;
    for _ in 0..frame / QUANTUM as u64 {
        t += 128.0 / rate as f64;
    }
    t
}

pub fn apply(chain: &mut FxChain, e: &ScriptEvent, ctl: Ctl) {
    let kind = FxKind::ALL[e.fx.expect("fx index")];
    let value = e.value.as_ref().expect("value");
    match e.kind.as_str() {
        "param" => {
            let param = FxParam::from_key(kind, e.key.as_deref().expect("key")).expect("a known param");
            chain.set_param(param, value.as_f64().expect("a number"), ctl);
        }
        "bypass" => chain.set_bypass(kind, value.as_bool().expect("a bool"), ctl),
        other => panic!("unexpected fx event {other}"),
    }
}

#[derive(Clone, Copy)]
pub enum Schedule {
    /// Every event's calls run before rendering, at its tick's time: the reference.
    Ahead,
    /// Each event lands `shift` frames after its tick, mid-block, as the engine would deliver it.
    Live(u64),
}

/// The scenario through a fresh chain, `block` frames per call: the chain's output. With the reverb
/// bypassed, also checks what its send carries.
pub fn render(id: &str, block: usize, schedule: Schedule) -> Vec<f32> {
    let s = refs::scenario(id);
    let x = refs::fx_input(s);
    let rate = s.rate as f32;
    let start = Ctl { now: 0.0, frame: 0 };
    let states = states(s);
    let mut chain = FxChain::new(rate, Some(&states), start);
    chain.set_timing(timing(s), start).expect("a valid timing");
    let mut events: Vec<ScriptEvent> = s.events.clone();
    events.sort_by_key(|e| e.frame);
    let mut live: Vec<(u64, ScriptEvent)> = Vec::new();
    match schedule {
        Schedule::Ahead => {
            for e in &events {
                apply(&mut chain, e, Ctl { now: tick_time(e.frame, s.rate), frame: 0 });
            }
        }
        Schedule::Live(shift) => live = events.into_iter().map(|e| (e.frame + shift, e)).collect(),
    }
    let (mut out, mut send) = (vec![0.0f32; s.frames], vec![0.0f32; s.frames]);
    let violations = violation_count();
    let mut at = 0;
    let mut next = 0;
    while at < s.frames {
        // Split the block at the next live event, as the engine splits at a command's frame.
        let mut end = (at + block).min(s.frames);
        if let Some((f, _)) = live.get(next) {
            end = end.min(*f as usize).max(at);
        }
        if end > at {
            let (o, sd) = (&mut out[at..end], &mut send[at..end]);
            assert_no_alloc(|| chain.process(at as u64, &x[at..end], o, sd));
        }
        at = end;
        while let Some((f, e)) = live.get(next) {
            if *f as usize != at {
                break;
            }
            let ctl = Ctl::at(*f, rate);
            assert_no_alloc(|| apply(&mut chain, e, ctl));
            next += 1;
        }
    }
    assert_eq!(violation_count(), violations, "{id}: the chain allocated while rendering or taking a control");
    if states[FxKind::Reverb.index()].bypassed {
        // A bypassed reverb's send carries only the start of Tone's bypass ramp, which begins at 1e-7
        // (setRampPoint's floor for a zero), then nothing.
        let ramp = (0.02 * s.rate as f64).ceil() as usize;
        assert!(send[..ramp].iter().all(|v| v.abs() < 1e-6) && send[ramp..].iter().all(|&v| v == 0.0), "{id}: the bypassed send");
    }
    out
}

pub fn bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|x| x.to_bits()).collect()
}
