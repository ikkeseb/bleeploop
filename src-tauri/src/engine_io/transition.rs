//! OWNS: the device owner's decisions, as pure functions (the kernel `host/native_io.rs`'s
//! `transition_action` is for the live line): what reaching a device takes from where the owner stands
//! ([`steps`]), when a request only changes the channel ([`same_device`]), where a lost device falls back
//! to ([`fallbacks`]), and which capture channel a request selects ([`input_channel`]). `owner.rs` carries
//! them out.

use super::DeviceRequest;
use crate::audio_output::AudioBackend;

/// What a transition does, in this order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Steps {
    /// Ramp the output to silence first (the streams still play).
    pub(crate) fade_out: bool,
    /// Drop the streams (output, then input) and punch out a take in flight.
    pub(crate) stop: bool,
    /// Hand every plugin unit back to its owner and drop the engine (another rate).
    pub(crate) evict: bool,
    /// Build an engine at the new device's rate.
    pub(crate) build: bool,
    /// Open the new device's streams and fade in.
    pub(crate) start: bool,
}

/// What reaching `target` (a device at that rate; `None` = none: a close or a loss) takes. `engine` is
/// the engine's rate, if one exists; `running`, streams are open; `healthy`, they still play (not lost).
pub(crate) fn steps(engine: Option<u32>, running: bool, healthy: bool, target: Option<u32>) -> Steps {
    let (fade_out, stop) = (running && healthy, running);
    match target {
        None => Steps { fade_out, stop, ..Steps::default() },
        Some(rate) => Steps {
            fade_out,
            stop,
            evict: engine.is_some_and(|r| r != rate),
            build: engine != Some(rate),
            start: true,
        },
    }
}

/// `next` asks for the device that runs, at most with another capture channel: the owner changes the
/// channel in place instead (a rebuilt ASIO input would register after the output and add a block).
/// ASIO ignores the WASAPI ids (one cached duplex driver); WASAPI ignores the buffer (the audio engine's
/// period).
pub(crate) fn same_device(running: &DeviceRequest, next: &DeviceRequest) -> bool {
    running.backend == next.backend
        && match next.backend {
            AudioBackend::Asio => running.buffer == next.buffer,
            AudioBackend::Wasapi => running.input == next.input && running.output == next.output,
        }
}

/// Where a lost device falls back to, tried in order: ASIO rebuilds from the cache once, then falls back
/// to the WASAPI defaults; a lost WASAPI endpoint falls back to the default endpoint (the side that kept
/// playing keeps its pick). A fallback equal to `lost` is a recovery.
pub(crate) fn fallbacks(lost: &DeviceRequest, input_lost: bool, output_lost: bool) -> Vec<DeviceRequest> {
    match lost.backend {
        AudioBackend::Asio => vec![
            lost.clone(),
            DeviceRequest { backend: AudioBackend::Wasapi, input: None, output: None, buffer: None, ..lost.clone() },
        ],
        AudioBackend::Wasapi => vec![DeviceRequest {
            input: if input_lost { None } else { lost.input.clone() },
            output: if output_lost { None } else { lost.output.clone() },
            ..lost.clone()
        }],
    }
}

/// The capture channel `requested` selects on an input with `channels` channels: auto is input 2 on a
/// device with two or more (where an instrument input usually sits), as `audio_input` picks it. A
/// fallback (`lenient`) falls back to auto when the new device lacks the pick.
pub(crate) fn input_channel(channels: usize, requested: Option<u32>, lenient: bool) -> Result<u32, String> {
    let channels = channels.max(1);
    match requested {
        Some(channel) if (channel as usize) < channels => Ok(channel),
        Some(channel) if !lenient => {
            Err(format!("input channel {} is unavailable; this device has {channels} channels", channel as u64 + 1))
        }
        _ => Ok(if channels >= 2 { 1 } else { 0 }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wasapi(input: Option<&str>, output: Option<&str>) -> DeviceRequest {
        DeviceRequest {
            backend: AudioBackend::Wasapi,
            input: input.map(str::to_string),
            output: output.map(str::to_string),
            input_channel: Some(0),
            buffer: None,
        }
    }

    fn asio(buffer: Option<u32>) -> DeviceRequest {
        DeviceRequest { backend: AudioBackend::Asio, input: None, output: None, input_channel: Some(1), buffer }
    }

    #[test]
    fn every_transition_takes_the_fewest_steps_in_order() {
        const F: bool = false;
        const T: bool = true;
        let s = |fade_out, stop, evict, build, start| Steps { fade_out, stop, evict, build, start };
        // (engine rate, running, healthy, target rate) → steps
        let table = [
            ((None, F, T, Some(48_000)), s(F, F, F, T, T), "the first open builds the engine"),
            ((Some(48_000), T, T, Some(48_000)), s(T, T, F, F, T), "a switch at the same rate keeps the engine"),
            ((Some(48_000), T, T, Some(44_100)), s(T, T, T, T, T), "a switch to another rate evicts and rebuilds"),
            ((Some(48_000), F, T, Some(48_000)), s(F, F, F, F, T), "a reopen after a close starts the streams"),
            ((Some(48_000), F, T, Some(44_100)), s(F, F, T, T, T), "a reopen at another rate rebuilds"),
            ((Some(48_000), T, F, Some(48_000)), s(F, T, F, F, T), "a lost device drops without a fade"),
            ((Some(48_000), T, F, Some(44_100)), s(F, T, T, T, T), "a fallback at another rate rebuilds"),
            ((Some(48_000), T, T, None), s(T, T, F, F, F), "a close fades out and stops"),
            ((Some(48_000), T, F, None), s(F, T, F, F, F), "a loss with no fallback just stops"),
            ((Some(48_000), F, T, None), s(F, F, F, F, F), "a close with nothing open does nothing"),
            ((None, F, T, None), s(F, F, F, F, F), "a close before any open does nothing"),
        ];
        for ((engine, running, healthy, target), want, why) in table {
            assert_eq!(steps(engine, running, healthy, target), want, "{why}");
        }
    }

    #[test]
    fn a_channel_change_is_not_a_new_device() {
        let mut other_channel = wasapi(Some("in"), Some("out"));
        other_channel.input_channel = Some(1);
        assert!(same_device(&wasapi(Some("in"), Some("out")), &other_channel));
        assert!(!same_device(&wasapi(Some("in"), Some("out")), &wasapi(Some("in"), None)), "another output");
        assert!(!same_device(&wasapi(Some("in"), None), &wasapi(None, None)), "another input");
        assert!(!same_device(&wasapi(None, None), &asio(None)), "another backend");
        assert!(!same_device(&asio(Some(256)), &asio(Some(128))), "another buffer");
        let mut ids = asio(Some(256));
        ids.input = Some("ignored".into());
        assert!(same_device(&asio(Some(256)), &ids), "ASIO ignores the WASAPI ids");
        let sized = DeviceRequest { buffer: Some(128), ..wasapi(Some("in"), Some("out")) };
        assert!(same_device(&wasapi(Some("in"), Some("out")), &sized), "WASAPI ignores the buffer");
    }

    #[test]
    fn a_lost_device_falls_back_in_order() {
        let lost = asio(Some(128));
        let tried = fallbacks(&lost, true, true);
        assert_eq!(tried[0], lost, "ASIO first rebuilds from the cache");
        assert_eq!(tried[1], DeviceRequest { backend: AudioBackend::Wasapi, input: None, output: None, input_channel: Some(1), buffer: None });
        assert_eq!(tried.len(), 2);

        let lost = wasapi(Some("in"), Some("out"));
        assert_eq!(fallbacks(&lost, false, true), vec![wasapi(Some("in"), None)], "the lost output goes to the default");
        assert_eq!(fallbacks(&lost, true, false), vec![wasapi(None, Some("out"))], "the lost input goes to the default");
        assert_eq!(fallbacks(&wasapi(None, None), true, true), vec![wasapi(None, None)], "the default is tried again");
    }

    #[test]
    fn auto_picks_input_two_and_a_fallback_forgives_a_missing_channel() {
        assert_eq!(input_channel(2, None, false), Ok(1));
        assert_eq!(input_channel(1, None, false), Ok(0));
        assert_eq!(input_channel(4, Some(3), false), Ok(3));
        assert!(input_channel(2, Some(2), false).is_err());
        assert_eq!(input_channel(2, Some(2), true), Ok(1));
        assert_eq!(input_channel(0, None, false), Ok(0), "a device reporting no channels still reads one");
    }
}
