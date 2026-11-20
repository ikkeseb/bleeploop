//! OWNS: the device owner's decisions, as pure functions (the kernel): what reaching a device takes from where the owner stands
//! ([`steps`]), when a request only changes the channel ([`same_device`]), when the device that runs
//! follows a WASAPI default Windows moved rather than reopening there ([`follows`], [`reads_past`]), where a lost device
//! falls back to ([`fallbacks`]), which capture channel a request selects for each slot
//! ([`input_channel`], [`open_channels`]), which buffer an ASIO open and its preopen ask the driver for
//! ([`asio_block`], [`preopen_block`]), and the rate it runs at ([`open_rate`]). `owner.rs` carries out
//! the first five, `cpal_driver.rs` the rest.

use lf_engine::SLOT_COUNT;

use super::callback::PICK_CHANNELS;
use super::driver::Spec;
use super::{DeviceRequest, InputPick, SAMPLE_RATES};
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

/// The buffer an ASIO open asks the driver for: `requested` when the driver takes it (`min..=max`), else
/// the power of two inside the range nearest to it, else `min` (a driver fixed at, say, 480 frames).
/// cpal refuses a fixed size outside the range, and a driver whose buffer is set in its own control
/// panel offers exactly one size. `src/ui/state/audio-settings.ts` mirrors it for the Buffer select.
#[cfg_attr(not(feature = "asio"), allow(dead_code))]
pub(crate) fn asio_block(requested: u32, min: u32, max: u32) -> u32 {
    if (min..=max).contains(&requested) {
        return requested;
    }
    (0..32)
        .map(|k| 1u32 << k)
        .filter(|b| (min..=max).contains(b))
        .min_by_key(|&b| b.abs_diff(requested))
        .unwrap_or(min)
}

/// The block the ASIO preopen runs the driver at before an open at `block` (`cpal_driver`): another size
/// the driver takes (`range`; `None` = not known, any), or none when it takes only `block`: the preopen
/// is then skipped rather than asked for a size the driver refuses.
#[cfg_attr(not(feature = "asio"), allow(dead_code))]
pub(crate) fn preopen_block(block: u32, range: Option<(u32, u32)>) -> Option<u32> {
    let (min, max) = range.unwrap_or((1, u32::MAX));
    [256, 128, 512, 64, 1024, min, max].into_iter().find(|&b| b != block && b > 0 && (min..=max).contains(&b))
}

/// The rate an ASIO open runs the driver at: the pick when it is one of [`SAMPLE_RATES`] and the driver
/// runs it (`runs`, the probe's `AsioCache::sample_rates`), else the driver's own `rate` (`None` = no
/// pick). Never an error: a pick the driver cannot take opens at its own rate (the caller logs why).
#[cfg_attr(not(feature = "asio"), allow(dead_code))]
pub(crate) fn open_rate(pick: Option<u32>, rate: u32, runs: &[u32]) -> u32 {
    pick.filter(|r| SAMPLE_RATES.contains(r) && runs.contains(r)).unwrap_or(rate)
}

/// `next` asks for the device that runs, at most with other capture channels: the owner changes the
/// channels in place instead (a rebuilt ASIO input would register after the output and add a block).
/// ASIO ignores the WASAPI ids (one cached duplex driver) but not the driver: `drivers` is the one the
/// device runs on and the one a resolve would take now, which a driver switch replaced. WASAPI ignores
/// the buffer (the audio engine's period) and the rate pick (the endpoint's own runs: `open_rate`). Where
/// a default is now is [`follows`]'s.
pub(crate) fn same_device(running: &DeviceRequest, next: &DeviceRequest, drivers: (&str, Option<&str>)) -> bool {
    running.backend == next.backend
        && match next.backend {
            AudioBackend::Asio => running.buffer == next.buffer && running.sample_rate == next.sample_rate && Some(drivers.0) == drivers.1,
            AudioBackend::Wasapi => running.input == next.input && running.output == next.output,
        }
}

/// A side left to Windows' default that Windows moved since `running` opened (a headset plugged in),
/// resolved again as `moved`: true when the run follows it there rather than reopening. cpal 0.18.1 opens
/// a default side as Windows' virtual default endpoint (`wasapi/device.rs`, `activate_audio_interface_sync`),
/// whose stream Windows reroutes to the new default itself. The output converts to the new endpoint's
/// format (cpal opens it with `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`); the capture, opened without that
/// flag, keeps the width and rate it opened at, and whether Windows converts it across another format
/// is unmeasured (a dead capture is a loss, as any). At the engine's rate the run already plays where
/// `moved` names, and a restart would only end a take. It reopens only for what the running streams
/// cannot give: another engine rate (a fresh open runs the endpoint's own), a capture where the run
/// has none (`input_open` false: none opened, or none to open) and Windows now offers another, or a
/// slot's pick (`picks`, read on `moved`'s inputs) on a channel past the width the capture opened at
/// (`running.in_channels`), one the stream cannot read ([`reads_past`]). A wider endpoint whose extra
/// inputs no pick reads plays on, as a narrower one does; the owner reads the picks again against it.
pub(crate) fn follows(running: &Spec, input_open: bool, moved: &Spec, picks: [Option<InputPick>; SLOT_COUNT]) -> bool {
    let capture_to_start = !input_open && moved.in_channels > 0 && moved.endpoints.input != running.endpoints.input;
    let past = input_open && moved.in_channels > 0 && reads_past(moved.in_channels, picks, running.in_channels);
    running.backend == moved.backend && running.rate == moved.rate && !capture_to_start && !past
}

/// Whether `picks`, as an open on an input with `inputs` channels reads them ([`open_channels`]: auto
/// is input 2 there), name a channel past `width`, the channels a running capture opened with.
pub(crate) fn reads_past(inputs: usize, picks: [Option<InputPick>; SLOT_COUNT], width: usize) -> bool {
    open_channels(inputs, picks).0.iter().any(|pick| pick.sides().iter().any(|&c| c as usize >= width))
}

/// Where a lost device falls back to, tried in order: ASIO rebuilds from the cache once, then falls back
/// to the WASAPI defaults; a lost WASAPI endpoint falls back to the default endpoint (the side that kept
/// playing keeps its pick), then both sides to the defaults (a yanked interface takes both endpoints,
/// and one side's error callback may not have fired yet). A fallback equal to `lost` is a recovery.
pub(crate) fn fallbacks(lost: &DeviceRequest, input_lost: bool, output_lost: bool) -> Vec<DeviceRequest> {
    match lost.backend {
        AudioBackend::Asio => vec![
            lost.clone(),
            DeviceRequest { backend: AudioBackend::Wasapi, input: None, output: None, buffer: None, ..lost.clone() },
        ],
        AudioBackend::Wasapi => {
            let kept = DeviceRequest {
                input: if input_lost { None } else { lost.input.clone() },
                output: if output_lost { None } else { lost.output.clone() },
                ..lost.clone()
            };
            let defaults = DeviceRequest { input: None, output: None, ..lost.clone() };
            if kept == defaults { vec![kept] } else { vec![kept, defaults] }
        }
    }
}

/// What `requested` reads on an input with `channels` channels: auto is input 2 on a device with two or
/// more (where an instrument input usually sits), as `audio_input` picks it, mono. A pick the device
/// lacks, either side of a pair, is refused: a change on the running device
/// (`EngineHost::set_slot_input_channel`). So is a channel past the [`PICK_CHANNELS`] the callback's
/// pick word can carry, on a device that has it (cpal 0.18.1 counts channels in a `u16`, so none does).
pub(crate) fn input_channel(channels: usize, requested: Option<InputPick>) -> Result<InputPick, String> {
    let channels = channels.max(1);
    match requested {
        Some(pick) => match pick.sides().into_iter().find(|&c| c as usize >= channels.min(PICK_CHANNELS)) {
            None => Ok(pick),
            Some(channel) if (channel as usize) < channels => {
                Err(format!("input channel {} is past the {PICK_CHANNELS} a slot can read", channel as u64 + 1))
            }
            Some(channel) => Err(format!("input channel {} is unavailable; this device has {channels} channels", channel as u64 + 1)),
        },
        None => Ok(InputPick::Mono(if channels >= 2 { 1 } else { 0 })),
    }
}

/// Each slot's input at an open (every reopen and recovery too): its pick, or auto for a slot whose
/// pick the device lacks, either side of a pair (one saved on a driver with more inputs), so an open
/// never fails on a channel. With it, the slots that fell back, for the owner's log; an output-only
/// device (`channels` 0) captures nothing, so no slot falls back there.
pub(crate) fn open_channels(channels: usize, requested: [Option<InputPick>; SLOT_COUNT]) -> ([InputPick; SLOT_COUNT], [bool; SLOT_COUNT]) {
    let mut fell_back = [false; SLOT_COUNT];
    let picks = std::array::from_fn(|slot| {
        input_channel(channels, requested[slot]).unwrap_or_else(|_| {
            fell_back[slot] = channels > 0;
            input_channel(channels, None).unwrap_or(InputPick::Mono(0))
        })
    });
    (picks, fell_back)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine_io::driver::Endpoints;
    use InputPick::{Mono, Pair};

    fn wasapi(input: Option<&str>, output: Option<&str>) -> DeviceRequest {
        DeviceRequest {
            backend: AudioBackend::Wasapi,
            input: input.map(str::to_string),
            output: output.map(str::to_string),
            input_channels: [Some(Mono(0)); SLOT_COUNT],
            buffer: None,
            sample_rate: None,
        }
    }

    fn asio(buffer: Option<u32>) -> DeviceRequest {
        DeviceRequest { backend: AudioBackend::Asio, input: None, output: None, input_channels: [Some(Mono(1)), None], buffer, sample_rate: None }
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
        other_channel.input_channels = [Some(Mono(0)), Some(Pair([0, 1]))];
        let a = ("A", Some("A"));
        assert!(same_device(&wasapi(Some("in"), Some("out")), &other_channel, a));
        assert!(!same_device(&wasapi(Some("in"), Some("out")), &wasapi(Some("in"), None), a), "another output");
        assert!(!same_device(&wasapi(Some("in"), None), &wasapi(None, None), a), "another input");
        assert!(!same_device(&wasapi(None, None), &asio(None), a), "another backend");
        assert!(!same_device(&asio(Some(256)), &asio(Some(128)), a), "another buffer");
        let mut ids = asio(Some(256));
        ids.input = Some("ignored".into());
        assert!(same_device(&asio(Some(256)), &ids, a), "ASIO ignores the WASAPI ids");
        assert!(!same_device(&asio(Some(256)), &asio(Some(256)), ("A", Some("B"))), "another ASIO driver since");
        assert!(!same_device(&asio(Some(256)), &asio(Some(256)), ("A", None)), "no ASIO driver cached any more");
        assert!(same_device(&wasapi(None, None), &wasapi(None, None), ("Speakers", Some("B"))), "WASAPI ignores the ASIO driver");
        let sized = DeviceRequest { buffer: Some(128), ..wasapi(Some("in"), Some("out")) };
        assert!(same_device(&wasapi(Some("in"), Some("out")), &sized, a), "WASAPI ignores the buffer");
        let rated = |rate| DeviceRequest { sample_rate: rate, ..asio(Some(256)) };
        assert!(!same_device(&rated(None), &rated(Some(44_100)), a), "a rate pick on ASIO");
        assert!(!same_device(&rated(Some(48_000)), &rated(Some(44_100)), a), "another rate pick on ASIO");
        let picked = DeviceRequest { sample_rate: Some(44_100), ..wasapi(Some("in"), Some("out")) };
        assert!(same_device(&wasapi(Some("in"), Some("out")), &picked, a), "WASAPI ignores the rate pick");
    }

    #[test]
    fn a_default_windows_moved_is_followed_at_the_engines_rate() {
        let spec = |name: &str, rate, in_rate, in_channels| Spec {
            backend: AudioBackend::Wasapi,
            rate,
            in_rate,
            in_channels,
            out_channels: 2,
            block: 0,
            input_name: name.into(),
            output_name: name.into(),
            endpoints: Endpoints { input: Some(name.into()), output: Some(name.into()) },
        };
        let running = spec("Mic", 48_000, 48_000, 2);
        let (stereo, auto, upper) = ([Some(Pair([0, 1])), None], [None, None], [Some(Pair([0, 1])), Some(Pair([2, 3]))]);
        assert!(follows(&running, true, &spec("Headset", 48_000, 48_000, 2), stereo), "rerouted: the stream plays there already");
        assert!(follows(&running, true, &spec("Headset", 48_000, 48_000, 1), stereo), "a mono mic: the capture runs on at its width");
        assert!(follows(&running, true, &spec("Headset", 48_000, 44_100, 2), stereo), "a mic at another rate: the capture runs on at the rate it opened at");
        assert!(follows(&running, true, &spec("Speakers", 48_000, 48_000, 0), upper), "no capture default left: nothing to reopen for");
        assert!(follows(&running, true, &spec("Interface", 48_000, 48_000, 4), stereo), "a wider capture whose extra inputs no pick reads");
        assert!(!follows(&running, true, &spec("Interface", 48_000, 48_000, 4), upper), "a pick on inputs 3+4: past the running stream's two");
        assert!(follows(&running, true, &spec("Interface", 48_000, 48_000, 2), upper), "the pick the endpoint lacks reads auto, input 2");
        let mono = spec("Mic", 48_000, 48_000, 1);
        assert!(!follows(&mono, true, &spec("Interface", 48_000, 48_000, 2), auto), "auto is input 2 on a stereo endpoint: past a mono stream");
        assert!(follows(&mono, true, &spec("Interface", 48_000, 48_000, 2), [Some(Mono(0)); SLOT_COUNT]), "input 1 the mono stream reads");
        assert!(!follows(&running, true, &spec("Headset", 44_100, 44_100, 2), stereo), "another engine rate opens at the endpoint's own");
        assert!(!follows(&running, false, &spec("Headset", 48_000, 48_000, 2), stereo), "a capture the run lacks starts there");
        let output_moved = Spec { endpoints: Endpoints { input: Some("Mic".into()), output: Some("Headset".into()) }, ..spec("Headset", 48_000, 48_000, 2) };
        assert!(follows(&running, false, &output_moved, upper), "the capture that did not open is where it was");
    }

    #[test]
    fn an_asio_open_runs_the_pick_only_where_the_driver_runs_it() {
        let both = [44_100, 48_000];
        // (pick, the driver's rate, the rates it runs) → the rate opened
        let table = [
            ((None, 48_000, &both[..]), 48_000, "no pick: the driver's own"),
            ((Some(44_100), 48_000, &both[..]), 44_100, "a pick the driver runs"),
            ((Some(48_000), 44_100, &both[..]), 48_000, "the other way"),
            ((Some(48_000), 44_100, &[44_100][..]), 44_100, "a pick the driver cannot run: its own"),
            ((Some(44_100), 96_000, &[][..]), 96_000, "a driver that runs neither: its own"),
            ((Some(96_000), 48_000, &[48_000, 96_000][..]), 48_000, "a rate no player can pick: its own"),
        ];
        for ((pick, rate, runs), want, why) in table {
            assert_eq!(open_rate(pick, rate, runs), want, "{why}");
        }
    }

    #[test]
    fn a_lost_device_falls_back_in_order() {
        let lost = asio(Some(128));
        let tried = fallbacks(&lost, true, true);
        assert_eq!(tried[0], lost, "ASIO first rebuilds from the cache");
        assert_eq!(tried[1], DeviceRequest { backend: AudioBackend::Wasapi, input: None, output: None, input_channels: [Some(Mono(1)), None], buffer: None, sample_rate: None });
        let picked = DeviceRequest { sample_rate: Some(44_100), ..lost.clone() };
        assert_eq!(fallbacks(&picked, true, true)[1].sample_rate, Some(44_100), "the fallback keeps the pick");
        assert_eq!(tried.len(), 2);

        let lost = wasapi(Some("in"), Some("out"));
        assert_eq!(fallbacks(&lost, false, true), vec![wasapi(Some("in"), None), wasapi(None, None)], "the lost output goes to the default, then both");
        assert_eq!(fallbacks(&lost, true, false), vec![wasapi(None, Some("out")), wasapi(None, None)], "the lost input goes to the default, then both");
        assert_eq!(fallbacks(&lost, true, true), vec![wasapi(None, None)], "both lost: the defaults, once");
        assert_eq!(fallbacks(&wasapi(None, None), true, true), vec![wasapi(None, None)], "the default is tried again");
    }

    #[test]
    fn an_asio_open_never_asks_for_a_buffer_outside_the_drivers_range() {
        // (requested, min, max) → the block asked for
        let table = [
            ((64, 512, 512), 512, "a driver fixed in its control panel takes its one size"),
            ((64, 128, 2048), 128, "too small: the nearest power of two in range"),
            ((2048, 32, 1024), 1024, "too large: the nearest power of two in range"),
            ((256, 32, 1024), 256, "in range: the request"),
            ((64, 480, 480), 480, "no power of two in range: the minimum"),
            ((1024, 100, 300), 256, "the nearest power of two, not the bound"),
        ];
        for ((requested, min, max), want, why) in table {
            assert_eq!(asio_block(requested, min, max), want, "{why}");
        }
    }

    #[test]
    fn the_preopen_takes_another_size_the_driver_accepts_or_none() {
        assert_eq!(preopen_block(512, Some((512, 512))), None, "a one-size driver has no other size");
        assert_eq!(preopen_block(480, Some((480, 480))), None);
        assert_eq!(preopen_block(256, Some((64, 2048))), Some(128));
        assert_eq!(preopen_block(128, Some((64, 2048))), Some(256));
        assert_eq!(preopen_block(512, Some((512, 1024))), Some(1024));
        assert_eq!(preopen_block(128, Some((100, 200))), Some(100), "no listed size in range: a bound");
        assert_eq!(preopen_block(256, None), Some(128), "an unknown range: as before");
        assert_eq!(preopen_block(64, None), Some(256));
    }

    #[test]
    fn auto_picks_input_two_and_a_change_to_a_missing_channel_is_refused() {
        assert_eq!(input_channel(2, None), Ok(Mono(1)));
        assert_eq!(input_channel(1, None), Ok(Mono(0)));
        assert_eq!(input_channel(4, Some(Mono(3))), Ok(Mono(3)));
        assert!(input_channel(2, Some(Mono(2))).is_err());
        assert_eq!(input_channel(0, None), Ok(Mono(0)), "a device reporting no channels still reads one");
        assert_eq!(input_channel(4, Some(Pair([2, 3]))), Ok(Pair([2, 3])), "a pair, each side as it is");
        assert_eq!(input_channel(4, Some(Pair([3, 0]))), Ok(Pair([3, 0])), "in either order");
        assert!(input_channel(2, Some(Pair([0, 2]))).is_err(), "a pair whose right side the device lacks");
        assert!(input_channel(2, Some(Pair([2, 0]))).is_err(), "or its left");
        let wide = PICK_CHANNELS + 1;
        assert_eq!(input_channel(wide, Some(Mono(PICK_CHANNELS as u32 - 1))), Ok(Mono(PICK_CHANNELS as u32 - 1)), "the last channel the pick word carries");
        assert!(input_channel(wide, Some(Mono(PICK_CHANNELS as u32))).is_err(), "past it: refused, never read as another");
        assert!(input_channel(wide, Some(Pair([0, PICK_CHANNELS as u32]))).is_err(), "either side of a pair");
        assert_eq!(open_channels(wide, [Some(Mono(PICK_CHANNELS as u32)), None]), ([Mono(1), Mono(1)], [true, false]), "an open takes auto, flagged");
    }

    #[test]
    fn an_open_takes_each_slots_pick_and_auto_for_a_slot_whose_pick_the_device_lacks() {
        let m = |a, b| [Mono(a), Mono(b)];
        assert_eq!(open_channels(4, [Some(Mono(0)), Some(Mono(3))]), (m(0, 3), [false, false]));
        assert_eq!(open_channels(2, [None, Some(Mono(0))]), (m(1, 0), [false, false]), "auto is input 2 for each slot on its own");
        assert_eq!(open_channels(1, [None, None]), (m(0, 0), [false, false]));
        assert_eq!(open_channels(2, [Some(Mono(0)), Some(Mono(7))]), (m(0, 1), [false, true]), "a stale pick takes auto for that slot alone");
        assert_eq!(open_channels(2, [Some(Mono(2)), Some(Mono(3))]), (m(1, 1), [true, true]));
        assert_eq!(open_channels(0, [Some(Mono(1)), None]), (m(0, 0), [false, false]), "an output-only device captures nothing");
        assert_eq!(open_channels(4, [Some(Pair([0, 1])), Some(Pair([3, 2]))]), ([Pair([0, 1]), Pair([3, 2])], [false, false]), "a pair per slot");
        assert_eq!(open_channels(2, [Some(Pair([0, 1])), Some(Pair([1, 2]))]), ([Pair([0, 1]), Mono(1)], [false, true]), "a pair missing one side: auto, mono, flagged");
        assert_eq!(open_channels(2, [Some(Pair([3, 1])), None]), ([Mono(1), Mono(1)], [true, false]), "the other side missing too");
    }
}
