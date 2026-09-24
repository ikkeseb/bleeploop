//! Buffer playback: Blink's AudioBufferSourceNode ([`BufferSource`]), Tone's `ToneBufferSource`
//! ([`ToneBufferSource`]) and Tone's looped-table `Noise` ([`Noise`]) on top of it.
//!
//! [`BufferSource`] ports `modules/webaudio/audio_buffer_source_handler.cc` and the scheduling of
//! `audio_scheduled_source_handler.cc` (plus `TimeToSampleFrame` from
//! `platform/audio/audio_utilities.cc`) at Chromium 153.0.8010.12: a start frame rounded up, a
//! sub-frame start carried into the read index, an offset rounded to a frame when the rates are at
//! their defaults (else read at a fractional index with linear interpolation), looping, and a
//! playback rate (the buffer's rate over the context's, times the k-rate `playbackRate` and
//! `2^(detune/1200)`, read once per quantum). One call renders one quantum.
//!
//! [`ToneBufferSource`] ports Tone 15.1.22's `source/buffer/ToneBufferSource.js` and its
//! `source/OneShotSource.js` gain envelope (fade in, fade out, linear or exponential). Tone stops the
//! native source from its clock's next tick after the stop time; here the native stop lands on the
//! first quantum boundary past it, which renders the same (the envelope is at zero from the stop
//! time on). [`Noise`] ports
//! `source/Noise.js` with the start/stop state of `source/Source.js`: every start plays the table from
//! a random offset, `random * (duration - 0.001)`, with `random` a draw the caller makes (Tone's
//! `Math.random`); a start while playing stops the old source and starts a new one.
//!
//! Tone builds a node per start; this module keeps a fixed pool and resets a finished source, so
//! starting and rendering never allocate.
//!
//! Ported from Chromium (Blink), Copyright The Chromium Authors, BSD-3-Clause.

use std::sync::Arc;

use super::gain::GainNode;
use super::param::{self, time_to_sample_frame, AudioParam, Rate, Rounding, Timeline, ToneParam, Units, QUANTUM};

/// Channels a source renders (the noise tables are stereo).
pub const MAX_CHANNELS: usize = 2;

/// Blink's upper limit on the computed playback rate.
const MAX_RATE: f64 = 1024.0;
/// Blink's grain duration before one is given.
const DEFAULT_GRAIN_DURATION: f64 = 0.020;

/// An AudioBuffer: channels of f32 at a sample rate that may differ from the context's.
pub struct AudioBuffer {
    sample_rate: f32,
    length: usize,
    channels: Vec<Vec<f32>>,
}

impl AudioBuffer {
    pub fn new(sample_rate: f32, channels: Vec<Vec<f32>>) -> Self {
        assert!((1..=MAX_CHANNELS).contains(&channels.len()), "1 or 2 channels");
        let length = channels[0].len();
        assert!(channels.iter().all(|c| c.len() == length), "channels of equal length");
        AudioBuffer { sample_rate, length, channels }
    }

    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    pub fn len(&self) -> usize {
        self.length
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub fn number_of_channels(&self) -> usize {
        self.channels.len()
    }

    pub fn channel(&self, c: usize) -> &[f32] {
        &self.channels[c]
    }

    pub fn duration(&self) -> f64 {
        self.length as f64 / self.sample_rate as f64
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackState {
    Unscheduled,
    Scheduled,
    Playing,
    Finished,
}

/// Blink's AudioBufferSourceNode.
pub struct BufferSource {
    sample_rate: f32,
    buffer: Option<Arc<AudioBuffer>>,
    /// k-rate. Tone wraps it in a Param; detune stays native.
    pub playback_rate: ToneParam,
    pub detune: AudioParam,
    state: PlaybackState,
    start_time: f64,
    end_time: Option<f64>,
    is_grain: bool,
    grain_offset: f64,
    grain_duration: f64,
    is_duration_given: bool,
    is_looping: bool,
    loop_start: f64,
    loop_end: f64,
    effective_loop_start: f64,
    effective_loop_end: f64,
    virtual_read_index: f64,
    buffer_played_frames: f64,
    out: [[f32; QUANTUM]; MAX_CHANNELS],
    silent: bool,
}

impl BufferSource {
    pub fn new(sample_rate: f32, frame: u64) -> Self {
        let rate = AudioParam::new(sample_rate as f64, 1.0, f32::MIN, f32::MAX, Rate::K);
        BufferSource {
            sample_rate,
            buffer: None,
            playback_rate: ToneParam::new(rate, Units::Positive, None, frame),
            detune: AudioParam::new(sample_rate as f64, 0.0, f32::MIN, f32::MAX, Rate::K),
            state: PlaybackState::Unscheduled,
            start_time: 0.0,
            end_time: None,
            is_grain: false,
            grain_offset: 0.0,
            grain_duration: DEFAULT_GRAIN_DURATION,
            is_duration_given: false,
            is_looping: false,
            loop_start: 0.0,
            loop_end: 0.0,
            effective_loop_start: 0.0,
            effective_loop_end: 0.0,
            virtual_read_index: 0.0,
            buffer_played_frames: 0.0,
            out: [[0.0; QUANTUM]; MAX_CHANNELS],
            silent: true,
        }
    }

    /// Back to a freshly built node with no buffer.
    pub fn reset(&mut self, frame: u64) {
        self.buffer = None;
        self.playback_rate.reset(None, frame);
        self.detune.reset();
        self.state = PlaybackState::Unscheduled;
        self.start_time = 0.0;
        self.end_time = None;
        self.is_grain = false;
        self.grain_offset = 0.0;
        self.grain_duration = DEFAULT_GRAIN_DURATION;
        self.is_duration_given = false;
        self.is_looping = false;
        self.loop_start = 0.0;
        self.loop_end = 0.0;
        self.effective_loop_start = 0.0;
        self.effective_loop_end = 0.0;
        self.virtual_read_index = 0.0;
        self.buffer_played_frames = 0.0;
        self.silent = true;
    }

    pub fn state(&self) -> PlaybackState {
        self.state
    }

    pub fn buffer(&self) -> Option<&Arc<AudioBuffer>> {
        self.buffer.as_ref()
    }

    /// The buffer (once; Blink refuses a second non-null buffer).
    pub fn set_buffer(&mut self, buffer: Arc<AudioBuffer>) {
        debug_assert!(self.buffer.is_none(), "the buffer is set once");
        self.buffer = Some(buffer);
        if self.is_grain {
            self.clamp_grain_parameters();
        }
        self.update_effective_loop_points();
        self.virtual_read_index = 0.0;
        self.buffer_played_frames = 0.0;
    }

    pub fn set_loop(&mut self, looping: bool) {
        self.is_looping = looping;
        self.update_effective_loop_points();
    }

    pub fn set_loop_start(&mut self, seconds: f64) {
        self.loop_start = seconds;
        self.update_effective_loop_points();
    }

    pub fn set_loop_end(&mut self, seconds: f64) {
        self.loop_end = seconds;
        self.update_effective_loop_points();
    }

    pub fn loop_end(&self) -> f64 {
        self.loop_end
    }

    pub fn loop_start(&self) -> f64 {
        self.loop_start
    }

    /// `start(when)`: play from the buffer's start.
    pub fn start(&mut self, when: f64, frame: u64) {
        if self.state != PlaybackState::Unscheduled || when < 0.0 {
            debug_assert!(false, "start once, at a time >= 0");
            return;
        }
        self.start_time = when.max(param::context_time(frame, self.sample_rate as f64));
        self.state = PlaybackState::Scheduled;
    }

    /// `start(when, offset[, duration])`: play from `offset` seconds into the buffer, for `duration`
    /// seconds of buffer when given.
    pub fn start_grain(&mut self, when: f64, offset: f64, duration: Option<f64>, frame: u64) {
        if self.state != PlaybackState::Unscheduled || when < 0.0 || offset < 0.0 || duration.is_some_and(|d| d < 0.0) {
            debug_assert!(false, "start once, with times >= 0");
            return;
        }
        self.is_duration_given = duration.is_some();
        self.is_grain = true;
        self.grain_offset = offset;
        self.grain_duration = duration.unwrap_or_else(|| self.buffer.as_ref().map_or(0.0, |b| b.duration()));
        self.start_time = when.max(param::context_time(frame, self.sample_rate as f64));
        if self.buffer.is_some() {
            self.clamp_grain_parameters();
        }
        self.state = PlaybackState::Scheduled;
    }

    /// `stop(when)`: the last call wins until the source has stopped.
    pub fn stop(&mut self, when: f64) {
        if self.state == PlaybackState::Unscheduled || when < 0.0 {
            debug_assert!(false, "stop after start, at a time >= 0");
            return;
        }
        self.end_time = Some(when.max(0.0));
    }

    fn clamp_grain_parameters(&mut self) {
        let buffer = self.buffer.as_ref().expect("a buffer");
        let duration = buffer.duration();
        self.grain_offset = self.grain_offset.clamp(0.0, duration);
        self.grain_duration = if self.is_duration_given { self.grain_duration.max(0.0) } else { f64::INFINITY };
        // Blink reads the params' intrinsic values here (the main thread's view).
        if self.playback_rate.native.intrinsic_value() == 1.0 && self.detune.intrinsic_value() == 0.0 {
            self.virtual_read_index = time_to_sample_frame(self.grain_offset, buffer.sample_rate() as f64, Rounding::Nearest) as f64;
        } else {
            self.virtual_read_index = self.grain_offset * buffer.sample_rate() as f64;
        }
    }

    fn update_effective_loop_points(&mut self) {
        let Some(buffer) = &self.buffer else {
            self.effective_loop_start = 0.0;
            self.effective_loop_end = 0.0;
            return;
        };
        let duration = buffer.duration();
        if !self.is_looping {
            self.effective_loop_start = 0.0;
            self.effective_loop_end = duration;
            return;
        }
        let start = self.loop_start.clamp(0.0, duration);
        let end = if self.loop_end == 0.0 { duration } else { self.loop_end.clamp(0.0, duration) };
        if start < end {
            self.effective_loop_start = start;
            self.effective_loop_end = end;
        } else {
            self.effective_loop_start = 0.0;
            self.effective_loop_end = duration;
        }
    }

    /// The last rendered quantum, one slot per buffer channel.
    pub fn output(&self) -> &[[f32; QUANTUM]] {
        &self.out[..self.channels()]
    }

    /// Whether the last rendered quantum is silent (Blink's bus flag).
    pub fn silent(&self) -> bool {
        self.silent
    }

    fn channels(&self) -> usize {
        self.buffer.as_ref().map_or(1, |b| b.number_of_channels())
    }

    fn zero(&mut self) {
        for c in self.out.iter_mut() {
            c.fill(0.0);
        }
        self.silent = true;
    }

    fn finish(&mut self) {
        self.state = PlaybackState::Finished;
    }

    /// Render the quantum at `quantum_start`.
    pub fn process(&mut self, quantum_start: u64) {
        if self.buffer.is_none() {
            self.zero();
            if self.state != PlaybackState::Unscheduled {
                self.finish();
            }
            return;
        }
        let (offset, frames, start_time_offset) = self.update_scheduling_info(quantum_start);
        if frames == 0 {
            self.zero();
            return;
        }
        if !self.render_from_buffer(quantum_start, offset, frames, start_time_offset) {
            self.zero();
            return;
        }
        self.silent = false;
    }

    /// Blink's `UpdateSchedulingInfo`: where in this quantum the source sounds.
    fn update_scheduling_info(&mut self, quantum_start: u64) -> (usize, usize, f64) {
        let sample_rate = self.sample_rate as f64;
        let quantum_end = quantum_start + QUANTUM as u64;
        let start_frame = time_to_sample_frame(self.start_time, sample_rate, Rounding::Up);
        let end_frame = self.end_time.map(|t| time_to_sample_frame(t, sample_rate, Rounding::Up));
        if end_frame.is_some_and(|e| e <= quantum_start) {
            self.finish();
        }
        if matches!(self.state, PlaybackState::Unscheduled | PlaybackState::Finished) || start_frame >= quantum_end {
            self.zero();
            return (0, 0, 0.0);
        }
        let mut start_frame_offset = 0.0;
        if self.state == PlaybackState::Scheduled {
            self.state = PlaybackState::Playing;
            start_frame_offset = self.start_time * sample_rate - start_frame as f64;
        }
        let quantum_frame_offset = (start_frame.saturating_sub(quantum_start) as usize).min(QUANTUM);
        let mut frames = QUANTUM - quantum_frame_offset;
        if frames == 0 {
            self.zero();
            return (quantum_frame_offset, 0, start_frame_offset);
        }
        for c in self.out.iter_mut() {
            c[..quantum_frame_offset].fill(0.0);
        }
        if let Some(end_frame) = end_frame {
            if end_frame >= quantum_start && end_frame <= quantum_end {
                if end_frame < quantum_end {
                    let zero_start = (end_frame - quantum_start) as usize;
                    let to_zero = QUANTUM - zero_start;
                    frames = frames.saturating_sub(to_zero);
                    for c in self.out.iter_mut() {
                        c[zero_start..].fill(0.0);
                    }
                }
                self.finish();
            }
        }
        (quantum_frame_offset, frames, start_frame_offset)
    }

    fn compute_playback_rate(&mut self, quantum_start: u64) -> f64 {
        let buffer = self.buffer.as_ref().expect("a buffer");
        let sample_rate_factor = buffer.sample_rate() as f64 / self.sample_rate as f64;
        let base = self.playback_rate.native.final_value(quantum_start, None) as f64;
        let mut rate = sample_rate_factor * base;
        // Blink divides the float detune by 1200 in float.
        rate *= super::fdlibm::pow(2.0, (self.detune.final_value(quantum_start, None) / 1200.0) as f64);
        rate.clamp(-MAX_RATE, MAX_RATE)
    }

    fn render_from_buffer(&mut self, quantum_start: u64, destination_offset: usize, number_of_frames: usize, start_time_offset: f64) -> bool {
        let computed_playback_rate = self.compute_playback_rate(quantum_start);
        let buffer = Arc::clone(self.buffer.as_ref().expect("a buffer"));
        let channels = buffer.number_of_channels();
        for c in self.out[..channels].iter_mut() {
            c[..destination_offset].fill(0.0);
        }
        let mut write_index = destination_offset;
        let buffer_length = buffer.len();
        let buffer_sample_rate = buffer.sample_rate() as f64;
        let virtual_start_frame = self.effective_loop_start * buffer_sample_rate;
        let virtual_end_frame = self.effective_loop_end * buffer_sample_rate;
        let virtual_delta_frames = virtual_end_frame - virtual_start_frame;
        if computed_playback_rate.abs() > virtual_delta_frames {
            return false;
        }

        let mut virtual_read_index = self.virtual_read_index;
        let mut frames_to_process = number_of_frames as i64;
        if computed_playback_rate >= 0.0 {
            if self.is_looping && virtual_read_index >= virtual_end_frame {
                virtual_read_index = virtual_start_frame.min((buffer_length - 1) as f64);
                self.virtual_read_index = virtual_read_index;
            }
            if start_time_offset < 0.0 && computed_playback_rate != 0.0 {
                let skipped = (start_time_offset * computed_playback_rate).abs();
                virtual_read_index += skipped;
                self.buffer_played_frames += skipped;
            }
        } else {
            if self.is_looping && virtual_read_index < virtual_start_frame {
                virtual_read_index = virtual_start_frame;
                self.virtual_read_index = virtual_read_index;
            }
            if start_time_offset < 0.0 {
                let skipped = (start_time_offset * computed_playback_rate).abs();
                virtual_read_index -= skipped;
                self.buffer_played_frames += skipped;
            }
            while frames_to_process > 0 && virtual_read_index >= buffer_length as f64 {
                for c in self.out[..channels].iter_mut() {
                    c[write_index] = 0.0;
                }
                write_index += 1;
                virtual_read_index += computed_playback_rate;
                frames_to_process -= 1;
            }
        }

        let mut stopping = false;
        if self.is_duration_given {
            let max_source_frames = self.grain_duration * buffer_sample_rate;
            let left = max_source_frames - self.buffer_played_frames;
            if left <= 0.0 {
                frames_to_process = 0;
                stopping = true;
            } else if computed_playback_rate != 0.0 {
                let until_limit = (left / computed_playback_rate.abs()).ceil();
                if until_limit < frames_to_process as f64 {
                    frames_to_process = until_limit as i64;
                    stopping = true;
                }
            }
        }
        if !self.is_looping
            && ((computed_playback_rate >= 0.0 && virtual_read_index >= buffer_length as f64) || (computed_playback_rate < 0.0 && virtual_read_index < 0.0))
        {
            virtual_read_index = virtual_read_index.clamp(0.0, buffer_length as f64);
            frames_to_process = 0;
            stopping = true;
        }

        let (w, v) = if computed_playback_rate == 1.0
            && virtual_read_index == virtual_read_index.floor()
            && virtual_delta_frames == virtual_delta_frames.floor()
            && virtual_end_frame == virtual_end_frame.floor()
        {
            self.process_fast_path(&buffer, virtual_delta_frames, virtual_end_frame, frames_to_process, write_index, virtual_read_index)
        } else {
            self.process_interpolated_path(
                &buffer,
                virtual_start_frame,
                virtual_delta_frames,
                virtual_end_frame,
                computed_playback_rate,
                frames_to_process,
                write_index,
                virtual_read_index,
            )
        };
        write_index = w;
        virtual_read_index = v;

        if computed_playback_rate != 0.0 {
            let processed = (write_index - destination_offset) as f64;
            self.buffer_played_frames += processed * computed_playback_rate.abs();
        }
        if stopping {
            for c in self.out[..channels].iter_mut() {
                c[write_index..destination_offset + number_of_frames].fill(0.0);
            }
            self.finish();
        }
        self.virtual_read_index = virtual_read_index;
        true
    }

    fn render_silence_and_finish(&mut self, channels: usize, index: usize, frames: usize) {
        for c in self.out[..channels].iter_mut() {
            c[index..index + frames].fill(0.0);
        }
        self.finish();
    }

    fn process_fast_path(
        &mut self,
        buffer: &AudioBuffer,
        virtual_delta_frames: f64,
        virtual_end_frame: f64,
        mut frames_to_process: i64,
        mut write_index: usize,
        virtual_read_index: f64,
    ) -> (usize, f64) {
        let channels = buffer.number_of_channels();
        let mut read_index = virtual_read_index as u32;
        let delta_frames = virtual_delta_frames as u32;
        let end_frame = virtual_end_frame as u32;
        while frames_to_process > 0 {
            let frames_to_end = end_frame.wrapping_sub(read_index) as i32 as i64;
            let n = frames_to_process.min(frames_to_end).max(0) as usize;
            for c in 0..channels {
                let src = &buffer.channel(c)[read_index as usize..read_index as usize + n];
                self.out[c][write_index..write_index + n].copy_from_slice(src);
            }
            write_index += n;
            read_index += n as u32;
            frames_to_process -= n as i64;

            let mut temp_read_index = read_index as f64;
            if delta_frames == 0 {
                self.finish();
                break;
            }
            if temp_read_index >= end_frame as f64 {
                if !self.is_looping {
                    self.render_silence_and_finish(channels, write_index, frames_to_process as usize);
                    read_index = read_index.min(buffer.len() as u32);
                    break;
                }
                let overflow = temp_read_index - end_frame as f64;
                temp_read_index = end_frame as f64 - delta_frames as f64 + overflow % delta_frames as f64;
            }
            read_index = temp_read_index as u32;
        }
        (write_index, read_index as f64)
    }

    #[allow(clippy::too_many_arguments)]
    fn process_interpolated_path(
        &mut self,
        buffer: &AudioBuffer,
        virtual_start_frame: f64,
        virtual_delta_frames: f64,
        virtual_end_frame: f64,
        computed_playback_rate: f64,
        frames_to_process: i64,
        mut write_index: usize,
        mut virtual_read_index: f64,
    ) -> (usize, f64) {
        let channels = buffer.number_of_channels();
        let buffer_length = buffer.len();
        let max_index = buffer_length as u32 - 1;
        for i in 0..frames_to_process {
            let frames_remaining = (frames_to_process - i - 1) as usize;
            let mut read_index = virtual_read_index as u32;
            let interpolation_factor = virtual_read_index - read_index as f64;
            let mut read_index2 = read_index + 1;
            if self.is_looping && (read_index as f64) < virtual_end_frame && read_index2 as f64 >= virtual_end_frame {
                let wrapped = if virtual_delta_frames >= 1.0 {
                    virtual_read_index + 1.0 - virtual_delta_frames
                } else if virtual_delta_frames > 0.0 {
                    virtual_start_frame + (virtual_read_index + 1.0 - virtual_start_frame) % virtual_delta_frames
                } else {
                    virtual_start_frame
                };
                read_index2 = if wrapped >= 0.0 { wrapped as u32 } else { 0 };
            } else if read_index2 as usize >= buffer_length {
                read_index2 = read_index;
            }
            read_index = read_index.min(max_index);
            read_index2 = read_index2.min(max_index);
            for c in 0..channels {
                let source = buffer.channel(c);
                let sample = if read_index == read_index2 && read_index >= 1 {
                    let s1 = source[read_index as usize - 1] as f64;
                    let s2 = source[read_index as usize] as f64;
                    s2 + (s2 - s1) * interpolation_factor
                } else {
                    let s1 = source[read_index as usize] as f64;
                    let s2 = source[read_index2 as usize] as f64;
                    (1.0 - interpolation_factor) * s1 + interpolation_factor * s2
                };
                self.out[c][write_index] = sample.clamp(f32::MIN as f64, f32::MAX as f64) as f32;
            }
            write_index += 1;
            virtual_read_index += computed_playback_rate;

            if virtual_delta_frames <= 0.0 {
                self.finish();
                break;
            }
            if computed_playback_rate >= 0.0 && virtual_read_index >= virtual_end_frame {
                if !self.is_looping {
                    self.render_silence_and_finish(channels, write_index, frames_remaining);
                    virtual_read_index = virtual_read_index.min(buffer_length as f64);
                    break;
                }
                let overflow = virtual_read_index - virtual_end_frame;
                virtual_read_index = virtual_end_frame - virtual_delta_frames + overflow % virtual_delta_frames;
            } else if computed_playback_rate < 0.0 && virtual_read_index < virtual_start_frame {
                if !self.is_looping {
                    self.render_silence_and_finish(channels, write_index, frames_remaining);
                    virtual_read_index = virtual_read_index.max(0.0);
                    break;
                }
                let overflow = virtual_start_frame - virtual_read_index;
                virtual_read_index = virtual_start_frame + virtual_delta_frames - overflow % virtual_delta_frames;
            }
        }
        (write_index, virtual_read_index)
    }
}

// ── Tone ────────────────────────────────────────────────────────────────────────────────────────

/// OneShotSource's fade curve.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FadeCurve {
    Linear,
    Exponential,
}

/// ToneBufferSource: a BufferSource through OneShotSource's gain envelope.
pub struct ToneBufferSource {
    pub source: BufferSource,
    /// OneShotSource's output gain (built at 0; the start sets it).
    pub gain: GainNode,
    buffer: Arc<AudioBuffer>,
    pub fade_in: f64,
    pub fade_out: f64,
    pub curve: FadeCurve,
    start_time: f64,
    stop_time: f64,
    source_started: bool,
    out: [[f32; QUANTUM]; MAX_CHANNELS],
    silent: bool,
}

impl ToneBufferSource {
    pub fn new(sample_rate: f32, buffer: Arc<AudioBuffer>, frame: u64) -> Self {
        let mut s = ToneBufferSource {
            source: BufferSource::new(sample_rate, frame),
            gain: GainNode::new(sample_rate as f64, 0.0, Units::Gain, frame),
            buffer,
            fade_in: 0.0,
            fade_out: 0.0,
            curve: FadeCurve::Linear,
            start_time: -1.0,
            stop_time: -1.0,
            source_started: false,
            out: [[0.0; QUANTUM]; MAX_CHANNELS],
            silent: true,
        };
        s.reset(frame);
        s
    }

    /// Back to a freshly built ToneBufferSource over the same buffer: not looping, loop points 0,
    /// playback rate 1, no fades.
    pub fn reset(&mut self, frame: u64) {
        self.source.reset(frame);
        self.gain.reset(0.0, frame);
        self.fade_in = 0.0;
        self.fade_out = 0.0;
        self.curve = FadeCurve::Linear;
        self.start_time = -1.0;
        self.stop_time = -1.0;
        self.source_started = false;
        self.silent = true;
    }

    pub fn set_loop(&mut self, looping: bool) {
        self.source.set_loop(looping);
    }

    /// Whether the node has played out: started, and the native source finished.
    pub fn finished(&self) -> bool {
        self.source.state() == PlaybackState::Finished || (self.start_time != -1.0 && !self.source_started)
    }

    /// Tone's `start(time, offset, duration, gain)`; `now` is Tone's current time.
    pub fn start(&mut self, time: f64, offset: Option<f64>, duration: Option<f64>, gain: f64, now: f64, frame: u64) {
        self.start_gain(time, gain, now, frame);
        let looping = self.source.is_looping;
        let offset = offset.unwrap_or(if looping { self.source.loop_start() } else { 0.0 });
        let mut computed_offset = offset.max(0.0);
        let duration_s = self.buffer.duration();
        if looping {
            let loop_end = if self.source.loop_end() != 0.0 { self.source.loop_end() } else { duration_s };
            let loop_start = self.source.loop_start();
            let loop_duration = loop_end - loop_start;
            if param::gte(computed_offset, loop_end) {
                computed_offset = (computed_offset - loop_start) % loop_duration + loop_start;
            }
            if param::eq(computed_offset, duration_s) {
                computed_offset = 0.0;
            }
        }
        let loop_end = if self.source.loop_end() != 0.0 { self.source.loop_end() } else { duration_s };
        self.source.set_buffer(Arc::clone(&self.buffer));
        self.source.set_loop_end(loop_end);
        if param::lt(computed_offset, duration_s) {
            self.source_started = true;
            self.source.start_grain(time, computed_offset, None, frame);
        }
        if let Some(d) = duration {
            self.stop(time + d.max(0.0), now, frame);
        }
    }

    fn start_gain(&mut self, time: f64, gain: f64, now: f64, frame: u64) {
        debug_assert!(self.start_time == -1.0, "a source starts once");
        self.start_time = (time + self.fade_in).max(now);
        let g = &mut self.gain.gain;
        if self.fade_in > 0.0 {
            g.set_value_at_time(0.0, time, frame);
            if self.curve == FadeCurve::Linear {
                g.linear_ramp_to_value_at_time(gain, time + self.fade_in, frame);
            } else {
                g.exponential_approach_value_at_time(gain, time, self.fade_in, frame);
            }
        } else {
            g.set_value_at_time(gain, time, frame);
        }
    }

    /// Tone's `stop(time)`: the fade out from `time`, the native stop where it ends.
    pub fn stop(&mut self, time: f64, now: f64, frame: u64) {
        debug_assert!(self.start_time != -1.0, "start before stop");
        let sample_time = 1.0 / self.gain.gain.native.sample_rate();
        // cancelStop: drop a previous stop's envelope.
        self.gain.gain.cancel_scheduled_values(self.start_time + sample_time, frame);
        self.stop_time = (time + self.fade_out).max(now);
        let g = &mut self.gain.gain;
        if self.fade_out > 0.0 {
            if self.curve == FadeCurve::Linear {
                g.linear_ramp_to(0.0, self.fade_out, time, frame);
            } else {
                g.target_ramp_to(0.0, self.fade_out, time, frame);
            }
        } else {
            g.cancel_and_hold_at_time(time, frame);
            g.set_value_at_time(0.0, time, frame);
        }
        if self.source_started && self.source.state() != PlaybackState::Finished {
            // Tone's timeout stops the native source on a clock tick after the stop time; the first
            // quantum boundary past it renders the same, the envelope being zero from the stop time.
            let rate = self.gain.gain.native.sample_rate();
            let quantum = QUANTUM as f64;
            self.source.stop(((self.stop_time * rate / quantum).floor() + 1.0) * quantum / rate);
        }
    }

    /// Render the quantum at `quantum_start`.
    pub fn process(&mut self, quantum_start: u64) {
        self.source.process(quantum_start);
        let channels = self.buffer.number_of_channels();
        let input = (!self.source.silent()).then(|| self.source.output());
        self.silent = self.gain.process(quantum_start, input, &mut self.out[..channels]);
    }

    pub fn output(&self) -> &[[f32; QUANTUM]] {
        &self.out[..self.buffer.number_of_channels()]
    }

    pub fn silent(&self) -> bool {
        self.silent
    }
}

/// Sources one Noise keeps: the playing one plus those still fading or finishing after a restart.
pub const NOISE_POOL: usize = 4;

#[derive(Clone, Copy, Debug)]
struct State {
    time: f64,
    started: bool,
}

/// Tone's Noise over a table (white or pink): restarts from a random offset, output through Tone's
/// `Volume` (0 dB unless set).
pub struct Noise {
    sources: [ToneBufferSource; NOISE_POOL],
    /// The pool slot Tone calls `_source` (the current one), if any.
    current: Option<usize>,
    /// Source.js's `_state` (increasing, memory 100).
    state: Timeline<State>,
    pub volume: GainNode,
    pub fade_in: f64,
    pub fade_out: f64,
    buffer: Arc<AudioBuffer>,
    mix: [[f32; QUANTUM]; MAX_CHANNELS],
    out: [[f32; QUANTUM]; MAX_CHANNELS],
    silent: bool,
}

impl Noise {
    pub fn new(sample_rate: f32, table: Arc<AudioBuffer>, frame: u64) -> Self {
        let mut state = Timeline::new(100, |s: &State| s.time);
        state.add(State { time: 0.0, started: false });
        Noise {
            sources: std::array::from_fn(|_| ToneBufferSource::new(sample_rate, Arc::clone(&table), frame)),
            current: None,
            state,
            volume: GainNode::new(sample_rate as f64, 0.0, Units::Decibels, frame),
            fade_in: 0.0,
            fade_out: 0.0,
            buffer: table,
            mix: [[0.0; QUANTUM]; MAX_CHANNELS],
            out: [[0.0; QUANTUM]; MAX_CHANNELS],
            silent: true,
        }
    }

    fn state_at(&self, time: f64) -> bool {
        self.state.get(time).is_some_and(|s| s.started)
    }

    /// Tone's `start(time)`; `random` is the `Math.random()` draw for the table offset, `now`
    /// Tone's current time.
    pub fn start(&mut self, time: f64, random: f64, now: f64, frame: u64) {
        let time = time.max(now);
        if self.state_at(time) {
            debug_assert!(self.state.get(time).is_some_and(|s| param::gt(time, s.time)), "start after the previous start");
            self.state.cancel(time);
            self.state.add(State { time, started: true });
            // Source.restart: its cancel drops the start just added.
            if self.state_at(time) {
                self.state.cancel(time);
                self.stop_source(time, now, frame);
                self.start_source(time, random, now, frame);
            }
        } else {
            self.state.add(State { time, started: true });
            self.start_source(time, random, now, frame);
        }
    }

    /// Tone's `stop(time)`.
    pub fn stop(&mut self, time: f64, now: f64, frame: u64) {
        let time = time.max(now);
        let next_start = {
            let i = self.state.search(time);
            i >= 0 && self.state.events()[i as usize..].iter().any(|s| s.started)
        };
        if self.state_at(time) || next_start {
            self.stop_source(time, now, frame);
            self.state.cancel(time);
            self.state.add(State { time, started: false });
        }
    }

    fn start_source(&mut self, time: f64, random: f64, now: f64, frame: u64) {
        // A free slot: never started, or played out. With none free, the slot after the current one
        // is taken over.
        let slot = self
            .sources
            .iter()
            .position(|s| s.start_time == -1.0 || s.finished())
            .unwrap_or_else(|| (self.current.map_or(0, |c| c + 1)) % NOISE_POOL);
        let s = &mut self.sources[slot];
        s.reset(frame);
        s.fade_in = self.fade_in;
        s.fade_out = self.fade_out;
        s.set_loop(true);
        let offset = random * (self.buffer.duration() - 0.001);
        s.start(time, Some(offset), None, 1.0, now, frame);
        self.current = Some(slot);
    }

    fn stop_source(&mut self, time: f64, now: f64, frame: u64) {
        if let Some(c) = self.current.take() {
            self.sources[c].stop(time, now, frame);
        }
    }

    /// Render the quantum at `quantum_start`.
    pub fn process(&mut self, quantum_start: u64) {
        let channels = self.buffer.number_of_channels();
        let mut any = false;
        for c in self.mix.iter_mut() {
            c.fill(0.0);
        }
        for s in self.sources.iter_mut().filter(|s| s.start_time != -1.0) {
            s.process(quantum_start);
            if !s.silent() {
                any = true;
                for (m, o) in self.mix.iter_mut().zip(s.output()) {
                    for (m, &x) in m.iter_mut().zip(o) {
                        *m += x;
                    }
                }
            }
        }
        let input = any.then_some(&self.mix[..channels]);
        self.silent = self.volume.process(quantum_start, input, &mut self.out[..channels]);
    }

    pub fn output(&self) -> &[[f32; QUANTUM]] {
        &self.out[..self.buffer.number_of_channels()]
    }

    pub fn silent(&self) -> bool {
        self.silent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = 48000.0;

    fn ramp(rate: f32, frames: usize) -> Arc<AudioBuffer> {
        Arc::new(AudioBuffer::new(rate, vec![(0..frames).map(|k| k as f32).collect()]))
    }

    fn render(s: &mut BufferSource, quanta: u64) -> Vec<f32> {
        let mut out = Vec::new();
        for q in 0..quanta {
            s.process(q * QUANTUM as u64);
            out.extend_from_slice(&s.output()[0]);
        }
        out
    }

    #[test]
    fn default_rates_copy_from_the_nearest_frame_and_loop() {
        let mut s = BufferSource::new(RATE, 0);
        s.set_buffer(ramp(RATE, 300));
        s.set_loop(true);
        s.start_grain(0.0, 10.4 / RATE as f64, None, 0);
        let out = render(&mut s, 4);
        for (n, &x) in out.iter().enumerate() {
            assert_eq!(x, ((10 + n) % 300) as f32, "frame {n}");
        }
    }

    #[test]
    fn a_buffer_at_another_rate_reads_at_the_rate_ratio() {
        let mut s = BufferSource::new(RATE, 0);
        s.set_buffer(ramp(24000.0, 1000));
        s.start(0.0, 0);
        let out = render(&mut s, 2);
        for (n, &x) in out.iter().enumerate() {
            assert_eq!(x, n as f32 * 0.5, "frame {n}");
        }
    }

    #[test]
    fn a_start_between_frames_skips_the_fraction() {
        let mut s = BufferSource::new(RATE, 0);
        s.set_buffer(ramp(24000.0, 1000));
        s.start(100.25 / RATE as f64, 0);
        let out = render(&mut s, 2);
        assert!(out[..101].iter().all(|&x| x == 0.0));
        // Frame 101 is 0.75 frames after the start, 0.375 frames into the buffer at half rate.
        assert_eq!(out[101], 0.375);
        assert_eq!(out[102], 0.875);
    }

    #[test]
    fn detune_scales_the_rate_per_quantum() {
        let mut s = BufferSource::new(RATE, 0);
        s.set_buffer(ramp(RATE, 1000));
        s.detune.set_value_at_time(1200.0, 0.0, 0);
        s.start(0.0, 0);
        let out = render(&mut s, 2);
        for (n, &x) in out.iter().enumerate() {
            assert_eq!(x, 2.0 * n as f32, "frame {n}");
        }
    }

    #[test]
    fn stop_silences_from_the_frame_rounded_up() {
        let mut s = BufferSource::new(RATE, 0);
        s.set_buffer(ramp(RATE, 1000));
        s.start(0.0, 0);
        s.stop(50.5 / RATE as f64);
        let out = render(&mut s, 2);
        assert_eq!(out[50], 50.0);
        assert!(out[51..].iter().all(|&x| x == 0.0));
        assert_eq!(s.state(), PlaybackState::Finished);
    }

    #[test]
    fn a_noise_restart_cuts_the_old_source_and_plays_from_a_new_offset() {
        let table: Vec<Vec<f32>> = (0..2).map(|c| (0..1000).map(|k| (k + 1000 * c) as f32).collect()).collect();
        let table = Arc::new(AudioBuffer::new(RATE, table));
        let mut noise = Noise::new(RATE, Arc::clone(&table), 0);
        noise.start(0.0, 0.5, 0.0, 0);
        noise.start(256.0 / RATE as f64, 0.0, 0.0, 0);
        noise.stop(400.0 / RATE as f64, 0.0, 0);
        let mut out = Vec::new();
        for q in 0..5u64 {
            noise.process(q * QUANTUM as u64);
            out.extend_from_slice(&noise.output()[1]);
        }
        let first = time_to_sample_frame(0.5 * (table.duration() - 0.001), RATE as f64, Rounding::Nearest) as usize;
        for (n, &x) in out.iter().enumerate() {
            let want = match n {
                0..256 => (1000 + (first + n) % 1000) as f32,
                256..400 => (1000 + n - 256) as f32,
                _ => 0.0,
            };
            assert_eq!(x, want, "frame {n}");
        }
    }
}
