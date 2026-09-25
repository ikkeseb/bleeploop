//! OWNS: the pipe that joins two clocks: frames pushed on one (a capture callback, the engine) are
//! pulled on another (the engine's output callback, the Share mirror's), resampled, with a drift
//! controller trimming the ratio so the ring holds its setpoint. The WASAPI join (input → engine) and
//! Share output (engine → mirror endpoint) both run on it.
//!
//! Reshaped from `host/transport.rs`'s `InPipe`/`OutMonitorPipe`/`DriftController`, which stay where
//! they are for the live line until Stage 6 deletes them with the WebView bridge.
//!
//! Pipes-and-share lane: build this module. The items below are the contract the device owner and the
//! share mirror use; their bodies are the lane's.

/// How a pipe is built.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PipeConfig {
    /// The pushing side's rate and the pulling side's, Hz.
    pub(crate) in_rate: u32,
    pub(crate) out_rate: u32,
    /// Interleaved channels per frame (1 for the join, 2 for Share output).
    pub(crate) channels: usize,
    /// Ring capacity, in frames at `in_rate`.
    pub(crate) capacity: usize,
    /// The fill the drift controller holds, in seconds.
    pub(crate) setpoint: f64,
    /// The largest `pull`, in frames at `out_rate`: the pipe's buffers are sized for it at build.
    pub(crate) max_pull: usize,
}

/// The pushing side. Never blocks, never allocates.
pub(crate) struct PushEnd {
    _todo: (),
}

impl PushEnd {
    /// Push interleaved frames; returns how many frames did not fit (dropped: the caller counts them).
    pub(crate) fn push(&mut self, frames: &[f32]) -> usize {
        let _ = frames;
        todo!("pipes lane")
    }
}

/// The pulling side. Never blocks, never allocates after `pipe` built it.
pub(crate) struct PullPipe {
    _todo: (),
}

/// Build a pipe (allocates: off the audio thread).
pub(crate) fn pipe(config: PipeConfig) -> Result<(PushEnd, PullPipe), String> {
    let _ = config;
    todo!("pipes lane")
}

impl PullPipe {
    /// Fill `out` (interleaved, `out.len() / channels` frames at `out_rate`, at most `max_pull`):
    /// resampled from the ring, the ratio trimmed toward the setpoint. A short ring zero-fills the rest
    /// (silence, never stale); returns how many frames were zero-filled (0 = none). The first pull that
    /// finds the ring at or above the setpoint drops any startup backlog down to it (a capture that ran
    /// before the output opened must not ride along as latency).
    pub(crate) fn pull(&mut self, out: &mut [f32]) -> usize {
        let _ = out;
        todo!("pipes lane")
    }

    /// What the pipe delays a frame by once settled, in frames at `out_rate`: the setpoint plus the
    /// resampler's own delay. The join's share of `ProcessContext::input_frames`.
    pub(crate) fn delay_frames(&self) -> f64 {
        todo!("pipes lane")
    }

    /// The clock drift the controller has learned, in ppm (diagnostics).
    pub(crate) fn drift_ppm(&self) -> f64 {
        todo!("pipes lane")
    }

    /// The ring's fill now, in frames at `in_rate`.
    pub(crate) fn fill(&self) -> usize {
        todo!("pipes lane")
    }
}
