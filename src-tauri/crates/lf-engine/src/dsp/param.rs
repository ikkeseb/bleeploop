//! Parameter automation: Blink's AudioParam timeline ([`AudioParam`]) and Tone's Param layer on top
//! of it ([`ToneParam`]), the core every Stage 3 port schedules through.
//!
//! [`AudioParam`] ports Blink's `modules/webaudio/audio_param_handler.{h,cc}` (which holds the
//! timeline), the value setter and ramp arguments of `modules/webaudio/audio_param.cc` and
//! `DiscreteTimeConstantForSampleRate` from `platform/audio/audio_utilities.cc`, at Chromium
//! 153.0.8010.12, taking the x86 paths (the product ships on x64 Windows; their float rounding differs
//! from the scalar paths ARM takes). Blink computes a param once per 128-frame render quantum and keeps
//! state between quanta (the running setTarget value, pruned and rewritten events), so the render API
//! is per quantum and every quantum starts on an absolute frame that is a multiple of [`QUANTUM`]:
//! rendering is bit-identical at any block size. A control call made while frame `f` is rendering
//! sees Blink's context time as the next quantum not yet computed (`f` rounded up to a quantum).
//!
//! [`ToneParam`] ports Tone.js 15.1.22's `core/context/Param.js` (its own event timeline mirror from
//! `core/util/Timeline.js`, units, `rampTo` and friends, `cancelAndHoldAtTime`, `setRampPoint`, the
//! `1e-7` floor for exponential targets) and `connectSignal` from `signal/Signal.js`. JS arithmetic
//! uses fdlibm, as V8 does.
//!
//! The Tone API that reaches a param from `src/audio/{synths,fx,engine.ts}` and the Tone classes they
//! build (Envelope, Monophonic, MembraneSynth, OneShotSource, Signal, Reverb, Gain, Filter): the value
//! setter, `setValueAtTime`, `linearRampToValueAtTime`, `exponentialRampToValueAtTime`,
//! `setTargetAtTime`, `cancelScheduledValues`, `cancelAndHoldAtTime`, `setRampPoint`, `rampTo`,
//! `linearRampTo`, `exponentialRampTo`, `targetRampTo`, `exponentialApproachValueAtTime` and a signal
//! connected into a param. Deliberately not ported: Blink's `setValueCurveAtTime` (no production path
//! calls the native one; Tone's own `setValueCurveAtTime` only runs for array envelope curves, which no
//! synth sets), Tone's `setValueCurveAtTime`/`apply`/`setParam` (swappable params) and string time or
//! frequency notation (every production value is a number in seconds or hertz). Tone's range asserts
//! throw in JS; here they are debug assertions.
//!
//! Scheduling never allocates: both timelines are vectors with fixed capacity. Blink's timeline holds
//! [`EVENT_CAPACITY`] live events (it prunes past events on every insert; an insert past capacity is
//! dropped, which no production schedule reaches) and Tone's mirror keeps Tone's memory of
//! [`TONE_MEMORY`] events, dropping the oldest as Tone does.
//!
//! Ported from Chromium (Blink), Copyright The Chromium Authors, BSD-3-Clause.

use super::fdlibm;

/// Frames per render quantum: Blink's unit of per-quantum work.
pub const QUANTUM: usize = 128;
const QUANTUM_F64: f64 = QUANTUM as f64;

/// Live events one Blink timeline holds.
pub const EVENT_CAPACITY: usize = 64;

/// Tone's `Param` keeps this many past events (`new Timeline(1000)`).
pub const TONE_MEMORY: usize = 1000;

/// setTarget has converged after this many time constants (Blink's `kTimeConstantsToConverge`).
const TIME_CONSTANTS_TO_CONVERGE: f32 = 10.0;
/// exp(-10), the relative distance at which setTarget has converged (Blink's literal).
#[allow(clippy::excessive_precision)]
const SET_TARGET_THRESHOLD: f32 = 4.539992976248485e-05;

/// The Blink context frame a control call at `frame` sees: the next quantum not yet computed.
pub fn context_frame(frame: u64) -> u64 {
    frame.next_multiple_of(QUANTUM as u64)
}

/// Blink's `DiscreteTimeConstantForSampleRate`: 1 - exp(-1 / (rate * tau)).
fn discrete_time_constant(time_constant: f64, sample_rate: f64) -> f64 {
    1.0 - fdlibm::exp(-1.0 / (sample_rate * time_constant))
}

fn has_set_target_converged(value: f32, target: f32, current_time: f64, start_time: f64, time_constant: f64) -> bool {
    if current_time > start_time + TIME_CONSTANTS_TO_CONVERGE as f64 * time_constant {
        return true;
    }
    if target == 0.0 && value.abs() < SET_TARGET_THRESHOLD {
        return true;
    }
    target != 0.0 && (target - value).abs() < SET_TARGET_THRESHOLD * value.abs()
}

fn linear_ramp_at_time(t: f64, value1: f32, time1: f64, value2: f32, time2: f64) -> f32 {
    (value1 as f64 + (value2 - value1) as f64 * (t - time1) / (time2 - time1)) as f32
}

fn exponential_ramp_at_time(t: f64, value1: f32, time1: f64, value2: f32, time2: f64) -> f32 {
    if value1 == 0.0 || value1.is_sign_negative() != value2.is_sign_negative() {
        value1
    } else {
        (value1 as f64 * fdlibm::pow((value2 / value1) as f64, (t - time1) / (time2 - time1))) as f32
    }
}

/// a-rate: one value per frame; k-rate: one value per quantum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rate {
    A,
    K,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    SetValue,
    LinearRamp,
    ExponentialRamp,
    SetTarget,
    /// cancelAndHoldAtTime's marker.
    CancelValues,
}

/// The ramp a CancelValues event cut short.
#[derive(Clone, Copy, Debug)]
struct Saved {
    kind: Kind,
    value: f32,
    time: f64,
}

#[derive(Clone, Copy, Debug)]
struct Event {
    kind: Kind,
    value: f32,
    time: f64,
    /// A ramp's start value when nothing precedes it (the param's intrinsic value at the call).
    initial_value: f32,
    time_constant: f64,
    saved: Option<Saved>,
    has_default_cancelled_value: bool,
    /// Blink's `new_events_`: inserted since the last render, so still to be clamped to its time.
    is_new: bool,
}

impl Event {
    fn new(kind: Kind, value: f32, time: f64) -> Self {
        Event { kind, value, time, initial_value: 0.0, time_constant: 0.0, saved: None, has_default_cancelled_value: false, is_new: false }
    }
}

/// Blink's AudioParam (`AudioParamHandler`): an intrinsic value, a nominal range and a timeline of
/// automation events. Times are seconds on the context clock.
pub struct AudioParam {
    rate: Rate,
    sample_rate: f64,
    intrinsic: f32,
    default: f32,
    min: f32,
    max: f32,
    events: Vec<Event>,
    /// An audio-rate connection sums into the param (the owner passes its values in).
    connected: bool,
    /// The quantum [`AudioParam::fill`] last computed.
    cached: Option<u64>,
    cache: [f32; QUANTUM],
}

impl AudioParam {
    pub fn new(sample_rate: f64, default: f32, min: f32, max: f32, rate: Rate) -> Self {
        AudioParam {
            rate,
            sample_rate,
            intrinsic: default,
            default,
            min,
            max,
            events: Vec::with_capacity(EVENT_CAPACITY),
            connected: false,
            cached: None,
            cache: [0.0; QUANTUM],
        }
    }

    /// Back to a freshly built param (no events, the default value, nothing connected).
    pub fn reset(&mut self) {
        self.events.clear();
        self.intrinsic = self.default;
        self.connected = false;
        self.cached = None;
    }

    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    pub fn default_value(&self) -> f32 {
        self.default
    }

    pub fn min_value(&self) -> f32 {
        self.min
    }

    pub fn max_value(&self) -> f32 {
        self.max
    }

    pub fn rate(&self) -> Rate {
        self.rate
    }

    pub fn intrinsic_value(&self) -> f32 {
        self.intrinsic
    }

    /// A signal is (or is no longer) connected into this param.
    pub fn set_connected(&mut self, connected: bool) {
        self.connected = connected;
    }

    /// `AudioParamHandler::SetValue`: store the intrinsic value, clamped to the nominal range.
    pub fn set_intrinsic_value(&mut self, value: f32) {
        self.intrinsic = value.clamp(self.min, self.max);
    }

    // ── Control side (Blink's main thread) ────────────────────────────────────────────────────

    fn current_frame(&self, frame: u64) -> u64 {
        context_frame(frame)
    }

    fn current_time(&self, frame: u64) -> f64 {
        self.current_frame(frame) as f64 / self.sample_rate
    }

    /// The `value` attribute's setter: the intrinsic value now, and a setValueAtTime at the current
    /// time so the timeline agrees.
    pub fn set_value(&mut self, value: f32, frame: u64) {
        self.set_intrinsic_value(value);
        self.set_value_at_time(self.intrinsic, self.current_time(frame), frame);
    }

    pub fn set_value_at_time(&mut self, value: f32, time: f64, frame: u64) {
        if !valid_time(time) {
            return;
        }
        let time = time.max(self.current_time(frame));
        self.insert_event(Event::new(Kind::SetValue, value, time), frame);
    }

    pub fn linear_ramp_to_value_at_time(&mut self, value: f32, time: f64, frame: u64) {
        if !valid_time(time) {
            return;
        }
        let time = time.max(self.current_time(frame));
        let event = Event { initial_value: self.intrinsic, ..Event::new(Kind::LinearRamp, value, time) };
        self.insert_event(event, frame);
    }

    /// A zero target is a RangeError in Blink: nothing is scheduled.
    pub fn exponential_ramp_to_value_at_time(&mut self, value: f32, time: f64, frame: u64) {
        if !valid_time(time) || value == 0.0 {
            debug_assert!(value != 0.0, "exponentialRampToValueAtTime to 0");
            return;
        }
        let time = time.max(self.current_time(frame));
        let event = Event { initial_value: self.intrinsic, ..Event::new(Kind::ExponentialRamp, value, time) };
        self.insert_event(event, frame);
    }

    pub fn set_target_at_time(&mut self, target: f32, time: f64, time_constant: f64, frame: u64) {
        if !valid_time(time) || !valid_time(time_constant) {
            return;
        }
        let time = time.max(self.current_time(frame));
        if time_constant == 0.0 {
            self.insert_event(Event::new(Kind::SetValue, target, time), frame);
        } else {
            self.insert_event(Event { time_constant, ..Event::new(Kind::SetTarget, target, time) }, frame);
        }
    }

    pub fn cancel_scheduled_values(&mut self, time: f64, frame: u64) {
        if !valid_time(time) {
            return;
        }
        let cancel_time = time.max(self.current_time(frame));
        if let Some(i) = self.events.iter().position(|e| e.time >= cancel_time) {
            self.events.truncate(i);
        }
    }

    pub fn cancel_and_hold_at_time(&mut self, time: f64, frame: u64) {
        if !valid_time(time) {
            return;
        }
        let cancel_time = time.max(self.current_time(frame));
        // The first event past the cancel time.
        let i = self.events.iter().position(|e| e.time > cancel_time).unwrap_or(self.events.len());
        let mut cancelled = i;
        if i > 0 && self.events[i - 1].kind == Kind::SetTarget {
            cancelled = i - 1;
        } else if i >= self.events.len() {
            return;
        }
        let event = self.events[cancelled];
        let mut new_event = None;
        match event.kind {
            Kind::LinearRamp | Kind::ExponentialRamp => {
                let saved = Saved { kind: event.kind, value: event.value, time: event.time };
                new_event = Some(Event { saved: Some(saved), ..Event::new(Kind::CancelValues, 0.0, cancel_time) });
            }
            Kind::SetTarget => {
                if event.time < cancel_time {
                    cancelled += 1;
                    new_event = Some(Event::new(Kind::CancelValues, 0.0, cancel_time));
                }
            }
            Kind::SetValue | Kind::CancelValues => {}
        }
        if cancelled < self.events.len() {
            self.events.truncate(cancelled);
        }
        if let Some(e) = new_event {
            self.insert_event(e, frame);
        }
    }

    fn insert_event(&mut self, mut event: Event, frame: u64) {
        // Prune past events so a timeline whose node is not rendering cannot grow without bound.
        if !self.events.is_empty() {
            let current_frame = self.current_frame(frame);
            let n = self.events.len();
            let mut skipped = 0;
            for i in 0..n {
                let next = if i + 1 < n { Some(i + 1) } else { None };
                if self.is_event_current(i, next, current_frame, self.sample_rate) {
                    break;
                }
                skipped += 1;
            }
            if skipped > 0 {
                self.remove_old_events(skipped);
            }
        }

        event.is_new = true;
        if self.events.is_empty() && matches!(event.kind, Kind::LinearRamp | Kind::ExponentialRamp) {
            // Nothing precedes the ramp: start it from the intrinsic value at the call.
            let start = Event { is_new: true, ..Event::new(Kind::SetValue, event.initial_value, self.current_time(frame)) };
            self.push_event(0, start);
        }
        // After every event at or before the new one's time.
        let at = self.events.partition_point(|e| e.time <= event.time);
        self.push_event(at, event);
    }

    fn push_event(&mut self, at: usize, event: Event) {
        debug_assert!(self.events.len() < EVENT_CAPACITY, "param timeline full");
        if self.events.len() < EVENT_CAPACITY {
            self.events.insert(at, event);
        }
    }

    fn remove_old_events(&mut self, count: usize) {
        let n = self.events.len();
        if n > 1 {
            self.events.drain(0..count.min(n - 1));
        }
    }

    // ── Render side (Blink's audio thread) ────────────────────────────────────────────────────

    /// Whether the timeline must run sample-accurately in the quantum at `quantum_start`.
    pub fn has_sample_accurate_values(&self, quantum_start: u64) -> bool {
        if self.connected {
            return true;
        }
        let Some(first) = self.events.first() else { return false };
        let end_time = (quantum_start + QUANTUM as u64) as f64 / self.sample_rate;
        if first.time > end_time && matches!(first.kind, Kind::SetTarget | Kind::SetValue) {
            return false;
        }
        if self.events.len() >= 2 {
            return true;
        }
        match first.kind {
            Kind::SetTarget => first.time <= end_time,
            _ => first.time >= quantum_start as f64 / self.sample_rate,
        }
    }

    /// `CalculateSampleAccurateValues`: this quantum's values (a-rate), or the k-rate value in every
    /// slot, plus the connected signal's `input`.
    pub fn calculate_sample_accurate_values(&mut self, quantum_start: u64, values: &mut [f32; QUANTUM], input: Option<&[f32; QUANTUM]>) {
        debug_assert_eq!(quantum_start % QUANTUM as u64, 0);
        if self.rate == Rate::A {
            let (min, max) = (self.min, self.max);
            let last = self.values_for_frame_range(quantum_start, quantum_start + QUANTUM as u64, self.intrinsic, values, self.sample_rate, min, max);
            self.set_intrinsic_value(last);
        } else {
            let value = self.timeline_value(quantum_start);
            values.fill(value);
            self.set_intrinsic_value(value);
        }
        if self.connected {
            if let Some(input) = input {
                if self.rate == Rate::A {
                    for (v, x) in values.iter_mut().zip(input) {
                        *v += x;
                    }
                } else {
                    let v = values[0] + input[0];
                    values.fill(v);
                }
            }
            for v in values.iter_mut() {
                if v.is_nan() {
                    *v = self.default;
                }
                *v = v.clamp(self.min, self.max);
            }
        }
    }

    /// `FinalValue`: the k-rate value for this quantum, plus the first frame of a connected `input`.
    pub fn final_value(&mut self, quantum_start: u64, input: Option<f32>) -> f32 {
        let mut value = self.timeline_value(quantum_start);
        self.set_intrinsic_value(value);
        if self.connected {
            if let Some(x) = input {
                value += x;
            }
            if value.is_nan() {
                value = self.default;
            }
            value = value.clamp(self.min, self.max);
        }
        value
    }

    /// `Value()` on the audio thread: the k-rate timeline value, stored as the intrinsic value.
    pub fn value(&mut self, quantum_start: u64) -> f32 {
        let value = self.timeline_value(quantum_start);
        self.set_intrinsic_value(value);
        self.intrinsic
    }

    /// This param as a GainNode reads it each quantum (sample-accurate a-rate values, else the k-rate
    /// value), for any block size: `out[k]` is the value at frame `frame + k`. Quanta must be visited
    /// in order, each once.
    pub fn fill(&mut self, frame: u64, out: &mut [f32]) {
        let mut done = 0;
        while done < out.len() {
            let f = frame + done as u64;
            let q = f - f % QUANTUM as u64;
            if self.cached != Some(q) {
                self.cached = Some(q);
                let mut cache = self.cache;
                if self.rate == Rate::A && self.has_sample_accurate_values(q) {
                    self.calculate_sample_accurate_values(q, &mut cache, None);
                } else {
                    cache.fill(self.value(q));
                }
                self.cache = cache;
            }
            let at = (f - q) as usize;
            let n = (QUANTUM - at).min(out.len() - done);
            out[done..done + n].copy_from_slice(&self.cache[at..at + n]);
            done += n;
        }
    }

    /// `ValueForContextTime`: the timeline at the quantum start, at control rate; the intrinsic value
    /// when no event has started.
    fn timeline_value(&mut self, quantum_start: u64) -> f32 {
        let default = self.intrinsic;
        if self.events.is_empty() || (quantum_start as f64 / self.sample_rate) < self.events[0].time {
            return default;
        }
        let mut value = [0.0f32];
        let control_rate = self.sample_rate / QUANTUM_F64;
        let (min, max) = (self.min, self.max);
        self.values_for_frame_range(quantum_start, quantum_start + 1, default, &mut value, control_rate, min, max)
    }

    /// `ValuesForFrameRange`: fill `values`, clip them to the nominal range, return the last
    /// (unclipped) value.
    #[allow(clippy::too_many_arguments)]
    fn values_for_frame_range(&mut self, start_frame: u64, end_frame: u64, default: f32, values: &mut [f32], control_rate: f64, min: f32, max: f32) -> f32 {
        let last = self.values_for_frame_range_impl(start_frame, end_frame, default, values, self.sample_rate, control_rate);
        for v in values.iter_mut() {
            *v = v.clamp(min, max);
        }
        last
    }

    fn values_for_frame_range_impl(&mut self, start_frame: u64, end_frame: u64, mut default: f32, values: &mut [f32], sample_rate: f64, control_rate: f64) -> f32 {
        if self.events.is_empty() || end_frame as f64 / sample_rate <= self.events[0].time {
            values.fill(default);
            return default;
        }
        let number_of_events = self.events.len();
        if self.events.iter().any(|e| e.is_new) {
            self.clamp_new_events_to_current_time(start_frame as f64 / sample_rate);
        }
        if self.handle_all_events_in_the_past(start_frame as f64 / sample_rate, sample_rate, &mut default, values) {
            return default;
        }

        let (mut current_frame, mut write_index) = self.handle_first_event(values, default, start_frame, end_frame, sample_rate);
        let mut value = default;
        let mut skipped = 0;
        let mut i = 0;
        while i < number_of_events && write_index < values.len() {
            let next = if i < number_of_events - 1 { Some(i + 1) } else { None };
            if !self.is_event_current(i, next, current_frame, sample_rate) {
                skipped += 1;
                i += 1;
                continue;
            }
            let next_kind = next.map(|n| self.events[n].kind);
            self.process_set_target_followed_by_ramp(i, next_kind, current_frame, sample_rate, control_rate, &mut value);

            let event = self.events[i];
            let value1 = event.value;
            let time1 = event.time;
            let (value2, time2) = match next {
                Some(n) => (self.events[n].value, self.events[n].time),
                None => (value1, end_frame as f64 / sample_rate + 1.0),
            };
            let (value2, time2, next_kind) = self.handle_cancel_values(i, next, value2, time2);
            debug_assert!(time2 >= time1);

            let fill_to_end_frame = if end_frame as f64 > time2 * sample_rate { (time2 * sample_rate).ceil() as u64 } else { end_frame };
            let fill_to_frame = (fill_to_end_frame.wrapping_sub(start_frame)).min(values.len() as u64) as usize;
            debug_assert!(fill_to_frame >= write_index);
            let fill_to_frame = fill_to_frame.max(write_index);

            (current_frame, value, write_index) = match (next_kind, event.kind) {
                (Some(Kind::LinearRamp), _) => process_linear_ramp(fill_to_frame, time1, time2, value1, value2, sample_rate, values, current_frame, value, write_index),
                (Some(Kind::ExponentialRamp), _) => {
                    process_exponential_ramp(fill_to_frame, time1, time2, value1, value2, sample_rate, values, current_frame, write_index)
                }
                (_, Kind::SetValue | Kind::LinearRamp | Kind::ExponentialRamp) => {
                    values[write_index..fill_to_frame].fill(event.value);
                    (fill_to_end_frame, event.value, fill_to_frame)
                }
                (_, Kind::CancelValues) => {
                    self.process_cancel_values(fill_to_frame, time1, sample_rate, control_rate, fill_to_end_frame, i, values, current_frame, value, write_index)
                }
                (_, Kind::SetTarget) => {
                    process_set_target(fill_to_frame, time1, value1, sample_rate, control_rate, fill_to_end_frame, &event, values, current_frame, value, write_index)
                }
            };
            i += 1;
        }

        if skipped > 0 {
            self.remove_old_events(skipped);
        }
        values[write_index..].fill(value);
        values[values.len() - 1]
    }

    fn handle_first_event(&self, values: &mut [f32], default: f32, start_frame: u64, end_frame: u64, sample_rate: f64) -> (u64, usize) {
        let first_event_time = self.events[0].time;
        let mut current_frame = start_frame;
        let mut write_index = 0;
        if first_event_time > start_frame as f64 / sample_rate {
            let mut fill_to_end_frame = end_frame;
            let first_event_frame = (first_event_time * sample_rate).ceil();
            if end_frame as f64 > first_event_frame {
                fill_to_end_frame = first_event_frame as u64;
            }
            let fill_to_frame = ((fill_to_end_frame - start_frame) as usize).min(values.len());
            values[..fill_to_frame].fill(default);
            write_index = fill_to_frame;
            current_frame += fill_to_frame as u64;
        }
        (current_frame, write_index)
    }

    /// Whether event `i` still produces values at `current_frame` (its successor has not started, or
    /// it is a SetValue landing inside the previous frame).
    fn is_event_current(&self, i: usize, next: Option<usize>, current_frame: u64, sample_rate: f64) -> bool {
        let Some(n) = next else { return true };
        if self.events[n].time * sample_rate >= current_frame as f64 {
            return true;
        }
        let event = &self.events[i];
        let event_frame = event.time * sample_rate;
        event.kind == Kind::SetValue && event_frame <= current_frame as f64 && (current_frame as f64) < event_frame + 1.0
    }

    fn clamp_new_events_to_current_time(&mut self, current_time: f64) {
        let mut clamped = false;
        for e in self.events.iter_mut().filter(|e| e.is_new) {
            if e.time < current_time {
                e.time = current_time;
                clamped = true;
            }
        }
        if clamped {
            // Blink's stable_sort by time, without allocating: insertion sort is stable.
            for k in 1..self.events.len() {
                let mut j = k;
                while j > 0 && self.events[j].time < self.events[j - 1].time {
                    self.events.swap(j, j - 1);
                    j -= 1;
                }
            }
        }
        for e in self.events.iter_mut() {
            e.is_new = false;
        }
    }

    fn handle_all_events_in_the_past(&mut self, current_time: f64, sample_rate: f64, default: &mut f32, values: &mut [f32]) -> bool {
        let last = self.events[self.events.len() - 1];
        if last.time + 1.5 * QUANTUM_F64 / sample_rate < current_time {
            if last.kind == Kind::SetTarget {
                if has_set_target_converged(*default, last.value, current_time, last.time, last.time_constant) {
                    *default = last.value;
                } else {
                    return false;
                }
            }
            values.fill(*default);
            self.remove_old_events(self.events.len());
            return true;
        }
        false
    }

    /// A ramp after a setTarget starts where the setTarget has got to: rewrite the setTarget as a
    /// SetValue at the current frame.
    fn process_set_target_followed_by_ramp(&mut self, i: usize, next_kind: Option<Kind>, current_frame: u64, sample_rate: f64, control_rate: f64, value: &mut f32) {
        let event = self.events[i];
        if event.kind != Kind::SetTarget || !matches!(next_kind, Some(Kind::LinearRamp | Kind::ExponentialRamp)) {
            return;
        }
        if (2.0 * sample_rate * event.time - 2.0 * current_frame as f64 + 1.0).abs() <= 1.0 {
            *value = (event.value as f64
                + (*value - event.value) as f64 * fdlibm::exp(-(current_frame as f64 / sample_rate - event.time) / event.time_constant))
                as f32;
        } else {
            let dtc = discrete_time_constant(event.time_constant, control_rate) as f32;
            *value += (event.value - *value) * dtc;
        }
        self.events[i] = Event::new(Kind::SetValue, *value, current_frame as f64 / sample_rate);
    }

    /// When the next event is a CancelValues that cut a ramp short, run that ramp up to the cancel
    /// time instead.
    fn handle_cancel_values(&mut self, i: usize, next: Option<usize>, mut value2: f32, mut time2: f64) -> (f32, f64, Option<Kind>) {
        let mut next_kind = next.map(|n| self.events[n].kind);
        let Some(n) = next else { return (value2, time2, next_kind) };
        let next_event = self.events[n];
        let Some(saved) = next_event.saved else { return (value2, time2, next_kind) };
        if next_event.kind != Kind::CancelValues {
            return (value2, time2, next_kind);
        }
        let current = self.events[i];
        if current.kind == Kind::SetTarget {
            return (value2, time2, next_kind);
        }
        time2 = next_event.time;
        next_kind = Some(saved.kind);
        if next_event.has_default_cancelled_value {
            value2 = next_event.value;
        } else {
            value2 = match saved.kind {
                Kind::LinearRamp => linear_ramp_at_time(next_event.time, current.value, current.time, saved.value, saved.time),
                Kind::ExponentialRamp => exponential_ramp_at_time(next_event.time, current.value, current.time, saved.value, saved.time),
                _ => unreachable!("only ramps are saved"),
            };
            self.events[n].value = value2;
            self.events[n].has_default_cancelled_value = true;
        }
        (value2, time2, next_kind)
    }

    #[allow(clippy::too_many_arguments)]
    fn process_cancel_values(
        &self,
        fill_to_frame: usize,
        time1: f64,
        sample_rate: f64,
        control_rate: f64,
        fill_to_end_frame: u64,
        i: usize,
        values: &mut [f32],
        current_frame: u64,
        mut value: f32,
        write_index: usize,
    ) -> (u64, f32, usize) {
        let event = &self.events[i];
        if event.has_default_cancelled_value {
            value = event.value;
        } else {
            let cancel_frame = time1 * sample_rate;
            if i >= 1 && cancel_frame <= current_frame as f64 && (current_frame as f64) < cancel_frame + 1.0 {
                let previous = &self.events[i - 1];
                if previous.kind == Kind::SetTarget {
                    // Blink narrows the time constant to float here.
                    let time_constant = previous.time_constant as f32;
                    let dtc = discrete_time_constant(time_constant as f64, control_rate) as f32;
                    value += (previous.value - value) * dtc;
                }
            }
        }
        values[write_index..fill_to_frame].fill(value);
        (fill_to_end_frame, value, fill_to_frame)
    }
}

fn valid_time(time: f64) -> bool {
    // Blink throws a RangeError for a negative time; nothing is scheduled.
    debug_assert!(time >= 0.0, "negative automation time {time}");
    time >= 0.0
}

#[allow(clippy::too_many_arguments)]
fn process_linear_ramp(
    fill_to_frame: usize,
    time1: f64,
    time2: f64,
    value1: f32,
    value2: f32,
    sample_rate: f64,
    values: &mut [f32],
    mut current_frame: u64,
    mut value: f32,
    mut write_index: usize,
) -> (u64, f32, usize) {
    let delta_time = time2 - time1;
    let k: f32 = if delta_time <= f32::MIN_POSITIVE as f64 { 0.0 } else { (1.0 / delta_time) as f32 };
    let value_delta = value2 - value1;
    if fill_to_frame > write_index {
        // Blink's SSE loop: four lanes stepped by a float increment.
        let step = (1.0 / sample_rate) as f32;
        let offset = (current_frame as f64 / sample_rate - time1) as f32;
        let slope = k * value_delta;
        let mut lanes = [0.0f32, 1.0, 2.0, 3.0].map(|l| (step * l + offset) * slope + value1);
        let inc = (4.0 / sample_rate * k as f64 * value_delta as f64) as f32;
        let trunc = write_index + (fill_to_frame - write_index) / 4 * 4;
        current_frame += (trunc - write_index) as u64;
        while write_index < trunc {
            values[write_index..write_index + 4].copy_from_slice(&lanes);
            for l in lanes.iter_mut() {
                *l += inc;
            }
            write_index += 4;
        }
    }
    if write_index >= 1 {
        value = values[write_index - 1];
    }
    for v in values[write_index..fill_to_frame].iter_mut() {
        let x = ((current_frame as f64 / sample_rate - time1) * k as f64) as f32;
        value = value1 + x * value_delta;
        current_frame += 1;
        *v = value;
    }
    (current_frame, value, fill_to_frame)
}

#[allow(clippy::too_many_arguments)]
fn process_exponential_ramp(
    fill_to_frame: usize,
    time1: f64,
    time2: f64,
    value1: f32,
    value2: f32,
    sample_rate: f64,
    values: &mut [f32],
    mut current_frame: u64,
    mut write_index: usize,
) -> (u64, f32, usize) {
    let mut value;
    if value1 * value2 <= 0.0 || time1 >= time2 {
        value = value1;
        values[write_index..fill_to_frame].fill(value);
        write_index = fill_to_frame;
    } else {
        let delta_time = time2 - time1;
        let num_sample_frames = delta_time * sample_rate;
        let multiplier = fdlibm::pow((value2 / value1) as f64, 1.0 / num_sample_frames);
        value = (value1 as f64 * fdlibm::pow(value2 as f64 / value1 as f64, (current_frame as f64 / sample_rate - time1) / delta_time)) as f32;
        let mut accumulator = value as f64;
        for v in values[write_index..fill_to_frame].iter_mut() {
            value = accumulator as f32;
            accumulator *= multiplier;
            current_frame += 1;
            *v = value;
        }
        write_index = fill_to_frame;
        if current_frame as f64 > time2 * sample_rate - 0.5 {
            value = value2;
        }
    }
    (current_frame, value, write_index)
}

#[allow(clippy::too_many_arguments)]
fn process_set_target(
    fill_to_frame: usize,
    time1: f64,
    value1: f32,
    sample_rate: f64,
    control_rate: f64,
    fill_to_end_frame: u64,
    event: &Event,
    values: &mut [f32],
    mut current_frame: u64,
    mut value: f32,
    mut write_index: usize,
) -> (u64, f32, usize) {
    let target = value1;
    // Blink narrows the time constant to float here.
    let time_constant = event.time_constant as f32;
    let dtc = discrete_time_constant(time_constant as f64, control_rate) as f32;

    let ramp_start_frame = time1 * sample_rate;
    if ramp_start_frame <= current_frame as f64 && (current_frame as f64) < ramp_start_frame + 1.0 {
        value = (target as f64 + (value - target) as f64 * fdlibm::exp(-(current_frame as f64 / sample_rate - time1) / time_constant as f64)) as f32;
    } else {
        value += (target - value) * dtc;
    }

    if has_set_target_converged(value, target, current_frame as f64 / sample_rate, time1, time_constant as f64) {
        current_frame += (fill_to_frame - write_index) as u64;
        values[write_index..fill_to_frame].fill(target);
        write_index = fill_to_frame;
    } else {
        if fill_to_frame > write_index {
            // Blink's SSE loop: four frames from one value, the fourth power of the step folded in.
            let c0 = dtc;
            let c1 = c0 * (2.0 - c0);
            let c2 = c0 * ((c0 - 3.0) * c0 + 3.0);
            let c3 = c0 * (c0 * ((4.0 - c0) * c0 - 6.0) + 4.0);
            let c = [0.0, c0, c1, c2];
            let trunc = write_index + (fill_to_frame - write_index) / 4 * 4;
            while write_index < trunc {
                let delta = target - value;
                for l in 0..4 {
                    values[write_index + l] = value + delta * c[l];
                }
                value += delta * c3;
                write_index += 4;
            }
        }
        for v in values[write_index..fill_to_frame].iter_mut() {
            *v = value;
            value += (target - value) * dtc;
        }
        write_index = fill_to_frame;
        if write_index >= 1 {
            value = values[write_index - 1];
        }
        current_frame = fill_to_end_frame;
    }
    (current_frame, value, write_index)
}

// ── Tone's layer ────────────────────────────────────────────────────────────────────────────────

/// Tone's EPSILON comparisons (`core/util/Math.js`).
const EPSILON: f64 = 1e-6;

fn gt(a: f64, b: f64) -> bool {
    a > b + EPSILON
}

fn lt(a: f64, b: f64) -> bool {
    a + EPSILON < b
}

fn eq(a: f64, b: f64) -> bool {
    (a - b).abs() < EPSILON
}

/// Tone's `Timeline` (`core/util/Timeline.js`): events kept sorted by time, equal times in insertion
/// order, compared with Tone's 1e-6 tolerance, the oldest dropped past `memory`.
pub struct Timeline<T: Copy> {
    events: Vec<T>,
    memory: usize,
    time: fn(&T) -> f64,
}

impl<T: Copy> Timeline<T> {
    pub fn new(memory: usize, time: fn(&T) -> f64) -> Self {
        Timeline { events: Vec::with_capacity(memory), memory, time }
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn events(&self) -> &[T] {
        &self.events
    }

    pub fn clear(&mut self) {
        self.events.clear();
    }

    fn t(&self, i: usize) -> f64 {
        (self.time)(&self.events[i])
    }

    /// Tone's `add`: after every event at the same time; the oldest drops past memory.
    pub fn add(&mut self, event: T) {
        let at = (self.search((self.time)(&event)) + 1) as usize;
        if self.events.len() == self.memory {
            // Tone inserts and then drops the first event; the new one is it when it sorts first.
            if at == 0 {
                return;
            }
            self.events.remove(0);
            self.events.insert(at - 1, event);
        } else {
            self.events.insert(at, event);
        }
    }

    /// Tone's `_search`: the index of the last event at or before `time` (equal within EPSILON), or -1.
    pub fn search(&self, time: f64) -> isize {
        let len = self.events.len();
        if len == 0 {
            return -1;
        }
        if self.t(len - 1) <= time {
            return len as isize - 1;
        }
        let (mut beginning, mut end) = (0usize, len);
        while beginning < end {
            let mid = beginning + (end - beginning) / 2;
            let at = self.t(mid);
            if eq(at, time) {
                // The last of the events at this time.
                let mut last = mid;
                while last + 1 < len && eq(self.t(last + 1), time) {
                    last += 1;
                }
                return last as isize;
            } else if lt(at, time) && gt(self.t(mid + 1), time) {
                return mid as isize;
            } else if gt(at, time) {
                end = mid;
            } else {
                beginning = mid + 1;
            }
        }
        -1
    }

    pub fn get(&self, time: f64) -> Option<T> {
        let i = self.search(time);
        (i >= 0).then(|| self.events[i as usize])
    }

    pub fn get_after(&self, time: f64) -> Option<T> {
        let i = self.search(time);
        self.events.get((i + 1) as usize).copied()
    }

    pub fn get_before(&self, time: f64) -> Option<T> {
        let len = self.events.len();
        if len > 0 && self.t(len - 1) < time {
            return Some(self.events[len - 1]);
        }
        let i = self.search(time);
        (i >= 1).then(|| self.events[i as usize - 1])
    }

    /// Tone's `cancel`: drop every event at or after `after`.
    pub fn cancel(&mut self, after: f64) {
        let len = self.events.len();
        if len > 1 {
            let mut index = self.search(after);
            if index >= 0 {
                if eq(self.t(index as usize), after) {
                    let mut i = index;
                    while i >= 0 && eq(self.t(i as usize), after) {
                        index = i;
                        i -= 1;
                    }
                    self.events.truncate(index as usize);
                } else {
                    self.events.truncate(index as usize + 1);
                }
            } else {
                self.events.clear();
            }
        } else if len == 1 && (gt(self.t(0), after) || eq(self.t(0), after)) {
            self.events.clear();
        }
    }
}

/// Tone's `Param.units`, where they change behaviour: conversion, `rampTo`'s curve, the range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Units {
    Number,
    Gain,
    Decibels,
    Frequency,
    Time,
    NormalRange,
    AudioRange,
    Positive,
    Cents,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToneKind {
    SetValue,
    LinearRamp,
    ExponentialRamp,
    SetTarget,
}

#[derive(Clone, Copy, Debug)]
struct ToneEvent {
    kind: ToneKind,
    time: f64,
    value: f64,
    constant: f64,
}

pub fn db_to_gain(db: f64) -> f64 {
    fdlibm::pow(10.0, db / 20.0)
}

pub fn gain_to_db(gain: f64) -> f64 {
    20.0 * (fdlibm::log(gain) / std::f64::consts::LN_10)
}

/// Tone's `Param` over a Blink [`AudioParam`]. Values are in the param's units, times in seconds;
/// every call also takes the frame being rendered, for Blink's context time.
pub struct ToneParam {
    pub native: AudioParam,
    events: Timeline<ToneEvent>,
    units: Units,
    convert: bool,
    min_value: Option<f64>,
    max_value: Option<f64>,
    /// A signal drives this param: Tone schedules only zeros on it.
    overridden: bool,
    /// The native param's default value (Tone's `_initialValue`).
    initial_value: f64,
    sample_time: f64,
}

/// Tone's `_minOutput`: the floor for an exponential target of zero.
const MIN_OUTPUT: f64 = 1e-7;

impl ToneParam {
    /// Tone's constructor: a `value` that differs from the native default is set at time 0.
    pub fn new(native: AudioParam, units: Units, value: Option<f64>, frame: u64) -> Self {
        let initial_value = native.default_value() as f64;
        let sample_time = 1.0 / native.sample_rate;
        let mut p = ToneParam {
            native,
            events: Timeline::new(TONE_MEMORY, |e: &ToneEvent| e.time),
            units,
            convert: true,
            min_value: None,
            max_value: None,
            overridden: false,
            initial_value,
            sample_time,
        };
        p.set_initial(value, frame);
        p
    }

    fn set_initial(&mut self, value: Option<f64>, frame: u64) {
        if let Some(v) = value {
            if v != self.in_units(self.initial_value) {
                self.set_value_at_time(v, 0.0, frame);
            }
        }
    }

    /// Back to a freshly built param with `value` (Tone builds a new one; the engine reuses this).
    pub fn reset(&mut self, value: Option<f64>, frame: u64) {
        self.events.clear();
        self.native.reset();
        self.overridden = false;
        self.set_initial(value, frame);
    }

    /// Tone's `minValue`/`maxValue` options (range asserts only).
    pub fn with_range(mut self, min: Option<f64>, max: Option<f64>) -> Self {
        self.min_value = min;
        self.max_value = max;
        self
    }

    /// Tone's `convert: false`.
    pub fn without_conversion(mut self) -> Self {
        self.convert = false;
        self
    }

    pub fn units(&self) -> Units {
        self.units
    }

    pub fn overridden(&self) -> bool {
        self.overridden
    }

    fn min_value(&self) -> f64 {
        match self.min_value {
            Some(v) => v,
            None => match self.units {
                Units::Time | Units::Frequency | Units::NormalRange | Units::Positive => 0.0,
                Units::AudioRange => -1.0,
                Units::Decibels => f64::NEG_INFINITY,
                _ => self.native.min_value() as f64,
            },
        }
    }

    fn max_value(&self) -> f64 {
        match self.max_value {
            Some(v) => v,
            None => match self.units {
                Units::NormalRange | Units::AudioRange => 1.0,
                _ => self.native.max_value() as f64,
            },
        }
    }

    fn assert_range(&self, value: f64) {
        debug_assert!(
            self.numeric(self.min_value()) <= value && value <= self.numeric(self.max_value()),
            "{value} outside the param's range"
        );
    }

    /// Tone's `_fromType`: a value in the param's units as the native param takes it.
    fn numeric(&self, value: f64) -> f64 {
        if self.convert && !self.overridden {
            if self.units == Units::Decibels {
                db_to_gain(value)
            } else {
                value
            }
        } else if self.overridden {
            0.0
        } else {
            value
        }
    }

    /// Tone's `_toType`: a native value in the param's units.
    fn in_units(&self, value: f64) -> f64 {
        if self.convert && self.units == Units::Decibels {
            gain_to_db(value)
        } else {
            value
        }
    }

    /// The `value` getter at Tone's `now`.
    pub fn value(&self, now: f64) -> f64 {
        self.get_value_at_time(now)
    }

    /// The `value` setter at Tone's `now`: cancel from now, set at now.
    pub fn set_value(&mut self, value: f64, now: f64, frame: u64) {
        self.cancel_scheduled_values(now, frame);
        self.set_value_at_time(value, now, frame);
    }

    pub fn set_value_at_time(&mut self, value: f64, time: f64, frame: u64) {
        let numeric = self.numeric(value);
        self.assert_range(numeric);
        self.events.add(ToneEvent { kind: ToneKind::SetValue, time, value: numeric, constant: 0.0 });
        self.native.set_value_at_time(numeric as f32, time, frame);
    }

    /// Tone's own model of the curve (JS doubles), in the param's units.
    pub fn get_value_at_time(&self, time: f64) -> f64 {
        let time = time.max(0.0);
        let after = self.events.get_after(time);
        let before = self.events.get(time);
        let value = match before {
            None => self.initial_value,
            Some(b) if b.kind == ToneKind::SetTarget && after.is_none_or(|a| a.kind == ToneKind::SetValue) => {
                let previous = self.events.get_before(b.time).map_or(self.initial_value, |p| p.value);
                exponential_approach(b.time, previous, b.value, b.constant, time)
            }
            Some(b) => match after {
                None => b.value,
                Some(a) if matches!(a.kind, ToneKind::LinearRamp | ToneKind::ExponentialRamp) => {
                    let before_value = if b.kind == ToneKind::SetTarget {
                        self.events.get_before(b.time).map_or(self.initial_value, |p| p.value)
                    } else {
                        b.value
                    };
                    if a.kind == ToneKind::LinearRamp {
                        before_value + (a.value - before_value) * ((time - b.time) / (a.time - b.time))
                    } else {
                        before_value * fdlibm::pow(a.value / before_value, (time - b.time) / (a.time - b.time))
                    }
                }
                Some(_) => b.value,
            },
        };
        self.in_units(value)
    }

    pub fn set_ramp_point(&mut self, time: f64, frame: u64) {
        let mut current = self.get_value_at_time(time);
        self.cancel_and_hold_at_time(time, frame);
        if self.numeric(current) == 0.0 {
            current = self.in_units(MIN_OUTPUT);
        }
        self.set_value_at_time(current, time, frame);
    }

    pub fn linear_ramp_to_value_at_time(&mut self, value: f64, end_time: f64, frame: u64) {
        let numeric = self.numeric(value);
        self.assert_range(numeric);
        self.events.add(ToneEvent { kind: ToneKind::LinearRamp, time: end_time, value: numeric, constant: 0.0 });
        self.native.linear_ramp_to_value_at_time(numeric as f32, end_time, frame);
    }

    pub fn exponential_ramp_to_value_at_time(&mut self, value: f64, end_time: f64, frame: u64) {
        let mut numeric = self.numeric(value);
        if eq(numeric, 0.0) {
            numeric = MIN_OUTPUT;
        }
        self.assert_range(numeric);
        self.events.add(ToneEvent { kind: ToneKind::ExponentialRamp, time: end_time, value: numeric, constant: 0.0 });
        self.native.exponential_ramp_to_value_at_time(numeric as f32, end_time, frame);
    }

    pub fn exponential_ramp_to(&mut self, value: f64, ramp_time: f64, start_time: f64, frame: u64) {
        self.set_ramp_point(start_time, frame);
        self.exponential_ramp_to_value_at_time(value, start_time + ramp_time, frame);
    }

    pub fn linear_ramp_to(&mut self, value: f64, ramp_time: f64, start_time: f64, frame: u64) {
        self.set_ramp_point(start_time, frame);
        self.linear_ramp_to_value_at_time(value, start_time + ramp_time, frame);
    }

    pub fn target_ramp_to(&mut self, value: f64, ramp_time: f64, start_time: f64, frame: u64) {
        self.set_ramp_point(start_time, frame);
        self.exponential_approach_value_at_time(value, start_time, ramp_time, frame);
    }

    /// A setTarget whose time constant reaches the value in `ramp_time`, finished by a linear ramp
    /// over its last 10 %.
    pub fn exponential_approach_value_at_time(&mut self, value: f64, time: f64, ramp_time: f64, frame: u64) {
        let time_constant = fdlibm::log(ramp_time + 1.0) / fdlibm::log(200.0);
        self.set_target_at_time(value, time, time_constant, frame);
        self.cancel_and_hold_at_time(time + ramp_time * 0.9, frame);
        self.linear_ramp_to_value_at_time(value, time + ramp_time, frame);
    }

    pub fn set_target_at_time(&mut self, value: f64, start_time: f64, time_constant: f64, frame: u64) {
        let numeric = self.numeric(value);
        debug_assert!(time_constant.is_finite() && time_constant > 0.0, "timeConstant must be greater than 0");
        self.assert_range(numeric);
        self.events.add(ToneEvent { kind: ToneKind::SetTarget, time: start_time, value: numeric, constant: time_constant });
        self.native.set_target_at_time(numeric as f32, start_time, time_constant, frame);
    }

    pub fn cancel_scheduled_values(&mut self, time: f64, frame: u64) {
        self.events.cancel(time);
        self.native.cancel_scheduled_values(time, frame);
    }

    /// Tone's own cancel-and-hold: it cuts a following ramp at `time` with its own value and sets
    /// that value; Blink's cancelAndHold runs only when an event sits exactly at `time` and none
    /// follows.
    pub fn cancel_and_hold_at_time(&mut self, time: f64, frame: u64) {
        let value_at_time = self.numeric(self.get_value_at_time(time));
        let before = self.events.get(time);
        let after = self.events.get_after(time);
        if before.is_some_and(|b| eq(b.time, time)) {
            if let Some(a) = after {
                self.native.cancel_scheduled_values(a.time, frame);
                self.events.cancel(a.time);
            } else {
                self.native.cancel_and_hold_at_time(time, frame);
                self.events.cancel(time + self.sample_time);
            }
        } else if let Some(a) = after {
            self.native.cancel_scheduled_values(a.time, frame);
            self.events.cancel(a.time);
            if a.kind == ToneKind::LinearRamp {
                self.linear_ramp_to_value_at_time(self.in_units(value_at_time), time, frame);
            } else if a.kind == ToneKind::ExponentialRamp {
                self.exponential_ramp_to_value_at_time(self.in_units(value_at_time), time, frame);
            }
        }
        self.events.add(ToneEvent { kind: ToneKind::SetValue, time, value: value_at_time, constant: 0.0 });
        self.native.set_value_at_time(value_at_time as f32, time, frame);
    }

    /// Exponential for frequency and decibels, linear otherwise.
    pub fn ramp_to(&mut self, value: f64, ramp_time: f64, start_time: f64, frame: u64) {
        if matches!(self.units, Units::Frequency | Units::Decibels) {
            self.exponential_ramp_to(value, ramp_time, start_time, frame);
        } else {
            self.linear_ramp_to(value, ramp_time, start_time, frame);
        }
    }

    /// Tone's `connectSignal` into this param: its schedule is cancelled and zeroed, so the signal's
    /// values replace it (they sum into the native param); a Signal destination is also marked
    /// overridden, so Tone schedules only zeros on it from then on.
    pub fn connect_signal(&mut self, destination_is_signal: bool, frame: u64) {
        self.cancel_scheduled_values(0.0, frame);
        self.set_value_at_time(0.0, 0.0, frame);
        if destination_is_signal {
            self.overridden = true;
        }
        self.native.set_connected(true);
    }
}

fn exponential_approach(t0: f64, v0: f64, v1: f64, time_constant: f64, t: f64) -> f64 {
    v1 + (v0 - v1) * fdlibm::exp(-(t - t0) / time_constant)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 48000.0;

    fn render(p: &mut AudioParam, frames: usize) -> Vec<f32> {
        let mut out = vec![0.0; frames];
        p.fill(0, &mut out);
        out
    }

    fn gain() -> AudioParam {
        AudioParam::new(RATE, 1.0, f32::MIN, f32::MAX, Rate::A)
    }

    // The Web Audio spec's formulas, evaluated in f64, at non-integer event times. Blink's float
    // stepping stays within a few float ulps of them.
    fn near(a: f32, b: f64, tol: f64) -> bool {
        (a as f64 - b).abs() <= tol * b.abs().max(1.0)
    }

    #[test]
    fn set_value_lands_on_the_first_frame_at_or_after_its_time() {
        let mut p = gain();
        p.set_value_at_time(0.25, 100.3 / RATE, 0);
        let v = render(&mut p, 256);
        assert!(v[..101].iter().all(|&x| x == 1.0));
        assert!(v[101..].iter().all(|&x| x == 0.25));
    }

    #[test]
    fn linear_ramp_follows_the_spec_formula() {
        let mut p = gain();
        let (t0, t1) = (10.5 / RATE, 300.25 / RATE);
        p.set_value_at_time(0.2, t0, 0);
        p.linear_ramp_to_value_at_time(0.9, t1, 0);
        let v = render(&mut p, 512);
        for (n, &x) in v.iter().enumerate() {
            let t = n as f64 / RATE;
            let want = if t < t0 { 1.0 } else if t < t1 { 0.2 + 0.7 * (t - t0) / (t1 - t0) } else { 0.9 };
            assert!(near(x, want, 1e-5), "frame {n}: {x} vs {want}");
        }
    }

    #[test]
    fn exponential_ramp_follows_the_spec_formula() {
        let mut p = gain();
        let (t0, t1) = (7.7 / RATE, 900.4 / RATE);
        p.set_value_at_time(0.01, t0, 0);
        p.exponential_ramp_to_value_at_time(0.8, t1, 0);
        let v = render(&mut p, 1024);
        for (n, &x) in v.iter().enumerate() {
            let t = n as f64 / RATE;
            let want = if t < t0 { 1.0 } else if t < t1 { 0.01 * (0.8f64 / 0.01).powf((t - t0) / (t1 - t0)) } else { 0.8 };
            assert!(near(x, want, 1e-5), "frame {n}: {x} vs {want}");
        }
    }

    #[test]
    fn set_target_follows_the_spec_formula() {
        let mut p = gain();
        let (t0, tau) = (33.3 / RATE, 0.004);
        p.set_value_at_time(0.9, 1.5 / RATE, 0);
        p.set_target_at_time(0.1, t0, tau, 0);
        let v = render(&mut p, 2048);
        for (n, &x) in v.iter().enumerate().skip(34) {
            let t = n as f64 / RATE;
            let want = 0.1 + 0.8 * (-(t - t0) / tau).exp();
            assert!(near(x, want, 1e-5), "frame {n}: {x} vs {want}");
        }
    }

    #[test]
    fn cancel_scheduled_values_drops_events_from_its_time() {
        let mut p = gain();
        p.set_value_at_time(0.5, 50.5 / RATE, 0);
        p.set_value_at_time(0.25, 200.5 / RATE, 0);
        p.cancel_scheduled_values(150.0 / RATE, 0);
        let v = render(&mut p, 384);
        assert!(v[51..].iter().all(|&x| x == 0.5));
    }

    #[test]
    fn cancel_and_hold_freezes_a_ramp_at_its_value_then() {
        let mut p = gain();
        let (t0, t1, tc) = (0.5 / RATE, 1000.5 / RATE, 400.25 / RATE);
        p.set_value_at_time(0.0, t0, 0);
        p.linear_ramp_to_value_at_time(1.0, t1, 0);
        p.cancel_and_hold_at_time(tc, 0);
        let v = render(&mut p, 1280);
        let held = (tc - t0) / (t1 - t0);
        for (n, &x) in v.iter().enumerate().skip(1) {
            let t = n as f64 / RATE;
            let want = if t < tc { (t - t0) / (t1 - t0) } else { held };
            assert!(near(x, want, 1e-5), "frame {n}: {x} vs {want}");
        }
    }

    #[test]
    fn k_rate_holds_one_value_per_quantum() {
        let mut p = AudioParam::new(RATE, 0.0, f32::MIN, f32::MAX, Rate::K);
        p.set_value_at_time(0.0, 0.0, 0);
        p.linear_ramp_to_value_at_time(1.0, 1000.0 / RATE, 0);
        let v = render(&mut p, 1024);
        for q in v.chunks(QUANTUM) {
            assert!(q.iter().all(|&x| x == q[0]));
        }
        assert!(near(v[256], 256.0 / 1000.0, 1e-6));
    }

    #[test]
    fn tone_exponential_approach_matches_tones_model() {
        let mut p = ToneParam::new(gain(), Units::Gain, None, 0);
        p.set_value_at_time(0.0, 0.0, 0);
        p.set_value_at_time(1.0, 0.02, 0);
        p.exponential_approach_value_at_time(0.0, 0.02, 0.1, 0);
        let frames = (0.12 * RATE) as usize;
        let v = render(&mut p.native, frames);
        for n in (0..frames).step_by(97) {
            let t = n as f64 / RATE;
            assert!(near(v[n], p.get_value_at_time(t), 1e-4), "frame {n}: {} vs {}", v[n], p.get_value_at_time(t));
        }
    }

    #[test]
    fn tone_ramps_start_from_the_value_at_their_start() {
        let mut p = ToneParam::new(gain(), Units::Frequency, Some(440.0), 0);
        p.ramp_to(880.0, 0.01, 0.005, 0);
        let v = render(&mut p.native, 1024);
        assert_eq!(v[0], 440.0);
        assert!(near(v[(0.01 * RATE) as usize], 440.0 * 2f64.powf(0.5), 1e-5));
        assert_eq!(v[1000], 880.0);
        let mut d = ToneParam::new(gain(), Units::Decibels, Some(-6.0), 0);
        assert!((d.value(0.0) + 6.0).abs() < 1e-9);
        d.set_value(0.0, 0.0, 0);
        assert_eq!(render(&mut d.native, 128)[5], 1.0);
    }

    #[test]
    fn a_connected_signal_zeroes_the_schedule_and_sums_in() {
        let mut p = ToneParam::new(gain(), Units::Gain, Some(0.5), 0);
        p.connect_signal(false, 0);
        let mut values = [0.0; QUANTUM];
        let input = [0.25; QUANTUM];
        p.native.calculate_sample_accurate_values(0, &mut values, Some(&input));
        assert!(values.iter().all(|&x| x == 0.25));
        assert!(!p.overridden());
    }

    #[test]
    fn tone_timeline_keeps_equal_times_in_insertion_order() {
        let mut t = Timeline::new(3, |e: &(f64, u8)| e.0);
        t.add((1.0, 0));
        t.add((1.0, 1));
        t.add((0.5, 2));
        assert_eq!(t.events(), &[(0.5, 2), (1.0, 0), (1.0, 1)]);
        t.add((2.0, 3));
        assert_eq!(t.events(), &[(1.0, 0), (1.0, 1), (2.0, 3)]);
        assert_eq!(t.get(1.0), Some((1.0, 1)));
        assert_eq!(t.get_after(1.0), Some((2.0, 3)));
        t.cancel(1.0);
        assert!(t.is_empty());
    }
}
