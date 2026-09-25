//! OWNS: Share output (`docs/plans/native-engine.md` § Stage 4, STATUS E2 "user-picked endpoint"): while
//! ASIO plays (its output bypasses the Windows audio engine, so no app capture can hear it), the
//! engine's post-limiter stereo master is mirrored to a WASAPI render endpoint the user picked, for
//! OBS, browsers and voice chat. On WASAPI no mirror opens: app capture takes the main output.
//!
//! Share-output lane: build this module. The contract with the device owner and the callback is the
//! two items below; their bodies are the lane's.

use lf_engine::grid::Frame;

/// The callback's end: owned inside the engine lock (`super::Rt`), fed every block after the limiter.
/// Drop-on-full, never blocks; a full ring counts `IoCounters::share_overruns`.
pub(crate) struct ShareTap {
    _todo: (),
}

impl ShareTap {
    /// Push one block of the stereo master (post-limiter, what the main output plays).
    pub(crate) fn push(&mut self, left: &[f32], right: &[f32], counters: &super::IoCounters) {
        let _ = (left, right, counters);
        todo!("share lane")
    }
}

/// The device owner's end: the mirror's WASAPI stream. Dropping it stops only the mirror.
pub(crate) struct ShareOutput {
    _todo: (),
}

impl ShareOutput {
    /// Open the mirror on `endpoint` (a WASAPI render device id) for an engine at `engine_rate` Hz,
    /// rendering `block`-frame blocks. The tap goes into the callback's `Rt`.
    pub(crate) fn open(endpoint: &str, engine_rate: u32, block: Frame, core: std::sync::Arc<super::Core>) -> Result<(ShareOutput, ShareTap), String> {
        let _ = (endpoint, engine_rate, block, core);
        todo!("share lane")
    }

    /// The stream died (its error callback latched): the owner drops the mirror and reports it.
    pub(crate) fn faulted(&self) -> bool {
        todo!("share lane")
    }
}
