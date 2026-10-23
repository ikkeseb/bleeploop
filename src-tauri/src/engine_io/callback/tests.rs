use super::*;

#[test]
fn stereo_lands_on_the_first_two_channels_and_a_mono_device_gets_the_mean() {
    let (left, right) = ([0.5f32, -0.25], [0.25f32, 0.75]);
    let mut quad = [9.0f32; 8];
    write(&mut quad, 4, &left, &right);
    assert_eq!(quad, [0.5, 0.25, 0.0, 0.0, -0.25, 0.75, 0.0, 0.0]);
    let mut mono = [0i16; 2];
    write(&mut mono, 1, &left, &right);
    assert_eq!(mono, [i16::from_sample(0.375f32), i16::from_sample(0.25f32)]);
    let mut stereo = [0i32; 4];
    write(&mut stereo, 2, &left, &right);
    assert_eq!(stereo, [i32::from_sample(0.5f32), i32::from_sample(0.25f32), i32::from_sample(-0.25f32), i32::from_sample(0.75f32)]);
    let mut unsigned = [0u16; 2];
    silence(&mut unsigned);
    assert_eq!(unsigned, [u16::EQUILIBRIUM; 2]);
}

#[test]
fn only_a_buffer_found_empty_lost_frames_and_only_what_played_past_it() {
    let frames = |k: f64| Duration::from_secs_f64(k / 48_000.0);
    assert_eq!(dry_frames(frames(441.0), 48_000, 441, 970), 0, "on time");
    assert_eq!(dry_frames(frames(1_300.0), 48_000, 882, 970), 0, "late, but frames were still queued");
    assert_eq!(dry_frames(frames(960.0), 48_000, 970, 970), 0, "empty just as it came");
    assert_eq!(dry_frames(frames(1_323.0), 48_000, 970, 970), 353, "dry for what played past the buffer");
}

/// Feed `slips` the callbacks `blocks` gives (how many frames after its ideal entry each enters, and
/// its frames), from `at` frames on a clock running `ppm` fast: the slips, the furthest a wake came
/// more than a period behind the floor, and what the old rule counted (runs of such wakes).
fn phase_slips(slips: &mut PhaseSlips, at: &mut f64, ppm: f64, blocks: impl IntoIterator<Item = (f64, usize)>) -> (u64, u64, u64) {
    const RATE: u32 = 48_000;
    let base = *BASE;
    let (mut count, mut max, mut late_runs, mut late) = (0, 0, 0, false);
    for (offset, n) in blocks {
        // A second's lead, so an early first wake stays after `BASE`.
        let entry = base + Duration::from_secs_f64((RATE as f64 + *at + offset) * (1.0 + ppm / 1e6) / RATE as f64);
        if let Some((behind, slipped)) = slips.wake(entry, n, RATE) {
            let behind = late_past_period(behind, n);
            count += u64::from(slipped);
            max = max.max(behind);
            late_runs += u64::from(behind > 0 && !late);
            late = behind > 0;
        }
        *at += n as f64;
    }
    (count, max, late_runs)
}

#[test]
fn a_callback_finishes_late_from_2_4_periods_after_the_best_phase() {
    // 2.4 periods of 64 frames: 153.6.
    assert!(!finishes_late(100.0, 53.5, 64), "just inside");
    assert!(finishes_late(100.0, 53.6, 64), "at it");
    assert!(finishes_late(0.0, 153.6, 64), "a duration alone");
    assert!(!finishes_late(512.0, 100.0, 256), "two periods behind and quick");
}

static BASE: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);

/// The rig's driver at 64 frames: each wake's offset from its ideal entry, uniform over [-44, 48]
/// frames (a fixed xorshift sequence).
fn spread(seed: u64) -> impl FnMut() -> f64 {
    let mut x = seed;
    move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        -44.0 + 92.0 * (x >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// At 64 frames and 48 kHz a window is 375 wakes: wake 1_125 opens the fourth.
const WINDOW_END: usize = 1_124;

#[test]
fn the_rigs_spread_wakes_on_a_clock_drifting_100_ppm_are_no_slip_where_the_old_rule_counted_thousands() {
    for ppm in [100.0, -100.0] {
        let (mut slips, mut at, mut wake) = (PhaseSlips::new(), 0.0, spread(7));
        // Three minutes at 64 frames.
        let blocks = (0..3 * 60 * 750).map(|_| (wake(), 64));
        let (count, max, late_runs) = phase_slips(&mut slips, &mut at, ppm, blocks);
        assert_eq!(count, 0, "{ppm} ppm");
        assert!(late_runs > 1_000, "the old rule: {late_runs} at {ppm} ppm");
        assert!((85..=100).contains(&max), "the spread, behind the floor: {max} at {ppm} ppm");
    }
}

#[test]
fn a_lasting_move_of_half_a_period_or_more_is_one_slip_and_a_smaller_one_none() {
    // (frames the phase moves later, the wake it moves at, slips): mid-window, on a window's last
    // wake and on the next one's first.
    let cases = [(128.0, 1_000, 1), (128.0, WINDOW_END, 1), (128.0, WINDOW_END + 1, 1), (64.0, 1_000, 1), (19.2, 1_000, 0)];
    for (jump, from, slips) in cases {
        let (mut phase, mut at, mut wake) = (PhaseSlips::new(), 0.0, spread(11));
        let blocks = (0..20 * 750).map(|k| (wake() + if k >= from { jump } else { 0.0 }, 64));
        assert_eq!(phase_slips(&mut phase, &mut at, 100.0, blocks).0, slips, "{jump} frames from wake {from}");
    }
}

#[test]
fn half_a_period_is_the_threshold_and_a_rise_split_over_two_windows_is_one_slip() {
    for (jump, slips) in [(36.0, 1), (28.0, 0)] {
        let (mut phase, mut at) = (PhaseSlips::new(), 0.0);
        let blocks = (0..20 * 750).map(|k| (if k >= 1_000 { jump } else { 0.0 }, 64));
        assert_eq!(phase_slips(&mut phase, &mut at, 0.0, blocks).0, slips, "{jump} frames");
    }
    // Jitter of -44, 0, 48, 0: a move one wake into a window leaves that window's floor at the
    // first wake's, so the floor rises over two windows (by about 92, then 36 frames).
    let (mut phase, mut at) = (PhaseSlips::new(), 0.0);
    let jitter = [-44.0, 0.0, 48.0, 0.0];
    let blocks = (0..20 * 750).map(|k| (jitter[k % 4] + if k >= 751 { 128.0 } else { 0.0 }, 64));
    assert_eq!(phase_slips(&mut phase, &mut at, 0.0, blocks).0, 1);
}

#[test]
fn a_stall_the_driver_makes_up_within_a_window_is_no_slip() {
    // Two periods late, then back to back: three wakes enter at once, mid-window and across a
    // window's end.
    for stall in [1_000, WINDOW_END - 1, WINDOW_END] {
        let (mut slips, mut at) = (PhaseSlips::new(), 0.0);
        let blocks = (0..20 * 750).map(|k| (if k == stall { 128.0 } else if k == stall + 1 { 64.0 } else { 0.0 }, 64));
        assert_eq!(phase_slips(&mut slips, &mut at, 0.0, blocks), (0, 128, 1), "from wake {stall}");
    }
}

#[test]
fn a_runs_start_and_a_new_block_size_are_no_slip_and_a_move_after_them_is_one() {
    let (mut slips, mut at) = (PhaseSlips::new(), 0.0);
    // Two periods later for good from the start's middle (a window is 94 wakes at 256 frames).
    let start = (0..375).map(|k| (if k >= 50 { 512.0 } else { 0.0 }, 256));
    assert_eq!(phase_slips(&mut slips, &mut at, 0.0, start).0, 0);
    // The driver resets to 64 frames, six 256-frame periods later.
    at += 1_536.0;
    assert_eq!(phase_slips(&mut slips, &mut at, 0.0, (0..1_500).map(|_| (0.0, 64))).0, 0);
    // Two periods later for good, past the new size's start.
    assert_eq!(phase_slips(&mut slips, &mut at, 0.0, (0..1_500).map(|_| (128.0, 64))).0, 1);
}

#[test]
fn a_falling_floor_and_entries_that_stand_still_or_run_back_are_no_slip() {
    // Two periods earlier for good.
    let (mut slips, mut at) = (PhaseSlips::new(), 0.0);
    let blocks = (0..20 * 750).map(|k| (if k >= 1_000 { -128.0 } else { 0.0 }, 64));
    assert_eq!(phase_slips(&mut slips, &mut at, 0.0, blocks).0, 0);
    // Every wake at one instant, then each further before the first.
    let mut slips = PhaseSlips::new();
    let now = *BASE + Duration::from_secs(10);
    for k in 0..10_000u64 {
        let entry = if k < 5_000 { now } else { now - Duration::from_micros(k) };
        assert!(!slips.wake(entry, 64, 48_000).is_some_and(|(_, slipped)| slipped), "wake {k}");
    }
}

#[test]
fn the_fade_ramps_both_ways_and_holds_silence() {
    let step = 0.25;
    let (mut l, mut r) = ([1.0f32; 6], [1.0f32; 6]);
    let mut gain = 0.0;
    fade(&mut gain, step, 1.0, &mut l, &mut r);
    assert_eq!(l, [0.25, 0.5, 0.75, 1.0, 1.0, 1.0]);
    let (mut l, mut r) = ([1.0f32; 6], [1.0f32; 6]);
    fade(&mut gain, step, 0.0, &mut l, &mut r);
    assert_eq!(r, [0.75, 0.5, 0.25, 0.0, 0.0, 0.0]);
    let (mut l, mut r) = ([1.0f32; 2], [1.0f32; 2]);
    fade(&mut gain, step, 0.0, &mut l, &mut r);
    assert_eq!((l, r), ([0.0; 2], [0.0; 2]));
}

/// Share output's tap in a test: what it was fed, both channels, up to the capacity it was built
/// with (it never allocates).
struct KeptTap(Arc<Mutex<Vec<[f32; 2]>>>);

impl Tap for KeptTap {
    fn push(&mut self, left: &[f32], right: &[f32], _counters: &IoCounters) {
        let mut kept = self.0.lock().unwrap();
        for (&l, &r) in left.iter().zip(right) {
            if kept.len() < kept.capacity() {
                kept.push([l, r]);
            }
        }
    }
}

/// A jam on the WASAPI join with Share output's tap in from the first block, the callback rendering
/// the engine in 128-frame slices: the live input monitored, a count-in and a one-bar take at 240
/// BPM, the loop panned left, CLICK on, half a second with the engine faulted (nothing renders),
/// then the fade-out. `click` is the click's volume. Returns the device output and what the tap was
/// fed, frame by frame.
fn share_jam(click: f32) -> (Vec<[f32; 2]>, Vec<[f32; 2]>) {
    let mut j = Join::sliced(Some(128));
    let kept = Arc::new(Mutex::new(Vec::with_capacity(6 * RATE as usize)));
    j.core.rt.lock().unwrap().tap = Some(Box::new(KeptTap(kept.clone())));
    for command in [lf_engine::Command::SetClickVolume(click), lf_engine::Command::SetBpm(240.0), lf_engine::Command::SetSlotLive(0, true), lf_engine::Command::SetMetronome(true), lf_engine::Command::RecDub(0)] {
        j.send(0, command);
    }
    let mut device = Vec::new();
    let mut play = |j: &mut Join, seconds: f64| {
        for _ in 0..(seconds * RATE as f64 / N as f64).round() as usize {
            device.extend(j.cycle(1).chunks_exact(2).map(|s| [s[0], s[1]]));
        }
    };
    play(&mut j, 2.5);
    let frame = j.frame();
    j.send(frame, lf_engine::Command::RecDub(0));
    j.send(frame, lf_engine::Command::SetPan(0, -0.6));
    play(&mut j, 1.5);
    j.core.rt.lock().unwrap().faulted = true;
    play(&mut j, 0.5);
    j.core.rt.lock().unwrap().faulted = false;
    play(&mut j, 0.5);
    j.render.run.fade_out.store(true, Release);
    play(&mut j, 0.1);
    let kept = kept.lock().unwrap().clone();
    (device, kept)
}

#[test]
fn share_output_is_fed_the_share_mix_in_the_device_outputs_slices_and_fades() {
    let (device, share) = share_jam(0.7);
    let (clean, clean_share) = share_jam(0.0);
    assert_eq!(share.len(), device.len(), "fed every frame, the faulted engine's too");
    assert!(device.iter().zip(&clean).any(|(a, b)| a != b), "the device output carries the click");
    // The click at 0 changes nothing else: the Share mix with the click on is the device output with
    // it at 0, both channels, from the fade-in through the faulted engine's silence to the fade-out.
    for (k, ((s, c), z)) in share.iter().zip(&clean).zip(&clean_share).enumerate() {
        assert!(s == c && z == c, "frame {k}: share {s:?}, the clean device output {c:?}, its share {z:?}");
    }
    let panned = clean.iter().filter(|[l, r]| l != r).count();
    assert!(panned > RATE as usize / 2, "the panned loop plays on its two channels apart: {panned} frames");
    assert_eq!(*clean.last().unwrap(), [0.0; 2], "the run faded out");
}

#[test]
fn the_block_load_bins_by_share_of_the_period() {
    use super::super::{LoadHistogram, LOAD_BINS};
    let load = LoadHistogram::default();
    let period = Duration::from_secs_f64(256.0 / 48_000.0);
    for _ in 0..998 {
        load.record(period.mul_f64(0.105), 256, 48_000);
    }
    load.record(period.mul_f64(0.455), 256, 48_000);
    load.record(period * 3, 256, 48_000);
    let all = load.snapshot();
    assert_eq!(all.count(), 1000);
    assert_eq!(all.quantile(0.5), Some(10));
    assert_eq!(all.quantile(0.999), Some(45));
    assert_eq!(all.max(), Some(LOAD_BINS - 1), "longer than the last bin lands in it");
    assert_eq!(all.since(&all).quantile(0.5), None, "an empty phase has no quantile");
}

#[test]
fn a_callback_at_or_over_its_period_counts_as_over_budget() {
    use super::super::LoadHistogram;
    let load = LoadHistogram::default();
    let period = Duration::from_secs_f64(64.0 / 48_000.0);
    for share in [0.5, 0.995, 1.0005, 1.5, 3.0] {
        load.record(period.mul_f64(share), 64, 48_000);
    }
    let all = load.snapshot();
    assert_eq!(all.over_budget(), 3, "99 % is inside the period; 100 %, 150 % and the last bin are past it");
    assert_eq!(all.text(), "p50<101% p99.9>=199% max>=199% over_budget=3");
    assert_eq!(all.since(&all).over_budget(), 0);
    // Inside the period the line stays as it was.
    let calm = LoadHistogram::default();
    calm.record(period.mul_f64(0.375), 64, 48_000);
    assert_eq!(calm.snapshot().text(), "p50<38% p99.9<38% max<38%");
}

#[test]
fn every_slots_pick_travels_in_one_word() {
    let run = Run::new([3, u32::MAX]);
    assert_eq!(run.slot_channels(), [3, u32::MAX]);
    run.set_slot_channels([u32::MAX, 0]);
    assert_eq!(run.slot_channels(), [u32::MAX, 0], "a swap lands whole");
    assert_eq!(run.picks(2), [1, 0], "a pick past the device's channels reads its last");
}

const RATE: u32 = 48_000;
/// A WASAPI period at `RATE`: 10 ms.
const N: usize = 480;

/// The input's frame code: every frame its own value, never silence.
fn code(frame: Frame) -> f32 {
    0.25 + (frame % 4096) as f32 / 16384.0
}

/// WASAPI's join by hand, on synthetic time: [`JoinInput`] pushes 10 ms periods of [`code`] and
/// [`Render`] pulls 10 ms blocks through the join pipe into an engine.
struct Join {
    core: Arc<Core>,
    input: JoinInput,
    render: Render,
    handle: lf_engine::EngineHandle,
    pushed: Frame,
    entry: Instant,
    events: Vec<lf_engine::Event>,
}

/// The device side's shared state with an engine at `RATE` in its lock, and the engine's handle.
fn engine_core() -> (Arc<Core>, lf_engine::EngineHandle) {
    let core = Arc::new(Core::new());
    let config = lf_engine::EngineConfig { max_loop_seconds: 4.0, ..lf_engine::EngineConfig::new(RATE) };
    let (engine, handle) = lf_engine::Engine::new(config);
    core.rt.lock().unwrap().engine = Some(engine);
    core.rate.store(RATE, Relaxed);
    core.max_block.store(config.max_block as u32, Relaxed);
    (core, handle)
}

impl Join {
    fn new() -> Join {
        Join::sliced(None)
    }

    /// [`Join::new`], the callback rendering the engine in slices of at most `max_block` frames.
    fn sliced(max_block: Option<u32>) -> Join {
        use super::super::pipes::{pipe, PipeConfig};
        let (core, handle) = engine_core();
        if let Some(max_block) = max_block {
            core.max_block.store(max_block, Relaxed);
        }
        let run = Arc::new(Run::new([0, 0]));
        // The device owner's join (`owner.rs`): 500 ms of ring, a 25 ms setpoint.
        let join = PipeConfig { in_rate: RATE, out_rate: RATE, channels: SLOT_COUNT, capacity: RATE as usize / 2, setpoint: 0.025, max_pull: MAX_DEVICE_BLOCK };
        let (push, pull) = pipe(join).unwrap();
        let input = JoinInput::new(core.clone(), run.clone(), 1, RATE, push);
        let render = Render::join(core.clone(), run, 2, RATE, pull);
        Join { core, input, render, handle, pushed: 0, entry: Instant::now(), events: Vec::new() }
    }

    /// `periods` input periods at once, then one output block, on time: what it played (stereo,
    /// interleaved).
    fn cycle(&mut self, periods: usize) -> [f32; 2 * N] {
        for _ in 0..periods {
            let data: Vec<f32> = (0..N as Frame).map(|k| code(self.pushed + k)).collect();
            self.input.capture(&data, None);
            self.pushed += N as Frame;
        }
        let mut out = [0.0f32; 2 * N];
        self.render.render(&mut out, self.entry, None);
        self.entry += Duration::from_millis(10);
        while let Ok(e) = self.handle.events.pop() {
            self.events.push(e);
        }
        out
    }

    fn send(&mut self, frame: Frame, command: lf_engine::Command) {
        self.handle.commands.push(lf_engine::TimedCommand { frame: Some(frame), command }).expect("command ring full");
    }

    fn frame(&self) -> Frame {
        self.core.frame.load(Relaxed)
    }

    fn engine<R>(&self, f: impl FnOnce(&lf_engine::Engine) -> R) -> R {
        f(self.core.rt.lock().unwrap().engine.as_ref().unwrap())
    }

    /// On time up to frame `trim`, then the output stalls while the input runs on (half a second of
    /// input arrives at once): the ring fills, the input past it is dropped, and the next pull trims
    /// the ring back to its 25 ms. Its last 1200 frames before the drop play over that block and the
    /// next, and the seam falls 240 frames into the third.
    fn overrun_at(&mut self, trim: Frame) {
        while self.frame() < trim {
            self.cycle(1);
        }
        assert_eq!(self.frame(), trim);
        self.cycle(50);
        assert!(self.core.counters.join_overruns.load(Relaxed) > 0, "the ring ran over");
        assert_eq!(self.core.counters.join_trims.load(Relaxed), 1);
    }
}

#[test]
fn a_join_overrun_rejects_the_take_its_seam_falls_in() {
    use lf_engine::{Command, Event};
    let mut j = Join::new();
    for _ in 0..20 {
        j.cycle(1);
    }
    let now = j.frame();
    for command in [Command::SetBpm(120.0), Command::SetSlotLive(0, true), Command::SetFixedLength(true), Command::SetFixedBars(1.0)] {
        j.send(now, command);
    }
    j.cycle(1);
    // A first take of one bar: its window opens a bar of count-in (4 x 24000 frames) plus the
    // alignment after the press. Press so that it opens 100 frames into a block.
    let k = 4 * 24_000 + j.core.align_frames.load(Relaxed) + j.engine(|e| e.limiter_latency());
    let press = j.frame() + 480 + (100 - k).rem_euclid(N as Frame);
    j.send(press, Command::RecDub(0));
    j.cycle(1);
    j.cycle(1);
    let start = press + k;
    assert_eq!(j.engine(|e| e.looper().recorder()), Some((0, Some(start), Some(start + 96_000))), "the window");
    // The take opens 100 frames into the block the seam falls in, before the seam.
    j.overrun_at(start - 100 - 2 * N as Frame);
    while j.frame() < start + 96_000 + 4 * N as Frame {
        j.cycle(1);
    }
    assert!(j.events.iter().any(|e| matches!(e, Event::TakeRejected { lane: 0, overdub: false, .. })), "the spliced take is rejected");
}

#[test]
fn a_join_overrun_rejects_the_layer_its_seam_falls_in_and_keeps_the_loop() {
    use lf_engine::{Command, Event};
    let mut j = Join::new();
    for _ in 0..20 {
        j.cycle(1);
    }
    let now = j.frame();
    for command in [Command::SetBpm(120.0), Command::SetSlotLive(0, true), Command::SetFixedLength(true), Command::SetFixedBars(1.0), Command::SetDubFeedback(0, 0.5)] {
        j.send(now, command);
    }
    j.cycle(1);
    let now = j.frame();
    j.send(now, Command::RecDub(0));
    while j.engine(|e| e.looper().master() == 0 || e.looper().recorder().is_some()) {
        j.cycle(1);
    }
    let before = j.engine(|e| e.looper().loop_pcm(0));
    assert!(before.iter().all(|&x| x != 0.0), "a clean one-bar loop");
    // The layer opens 100 frames into the block the seam falls in, after the trimmed pull's two
    // blocks, and is armed well before them.
    let k = j.core.align_frames.load(Relaxed) + j.engine(|e| e.limiter_latency());
    let start = j.frame() + 8 * N as Frame + 100;
    let trim = start - 100 - 2 * N as Frame;
    assert!(start - k < trim, "the press lands before the stall");
    j.send(start - k, Command::RecDub(0));
    while j.frame() < trim {
        j.cycle(1);
    }
    assert_eq!(j.engine(|e| e.looper().recorder()), Some((0, Some(start), None)), "the layer's window");
    j.overrun_at(trim);
    while j.frame() < start + 48_000 {
        j.cycle(1);
    }
    let now = j.frame();
    j.send(now, Command::RecDub(0));
    while j.engine(|e| e.looper().recorder().is_some() || e.looper().busy()) {
        j.cycle(1);
    }
    assert!(j.events.iter().any(|e| matches!(e, Event::TakeRejected { lane: 0, overdub: true, .. })), "the spliced layer is rejected");
    assert!(j.engine(|e| e.looper().loop_pcm(0)) == before, "the loop before the layer, bit for bit");
}

/// CLIP lights at full scale in the device's own sample format, either rail, and never one step
/// under it, through both input callbacks: an I32's top step under converts to 1.0, and an F32's is
/// within a 16-bit step of it.
#[test]
fn clip_lights_at_each_input_formats_full_scale_and_never_one_step_under_it() {
    // Whether `x`, every sample of a mono input, lights each slot's clip (both read the one channel):
    // ASIO's cycle, then WASAPI's join until its pull comes back whole.
    macro_rules! lit {
        ($x:expr) => {{
            let data = [$x; N];
            let mut out = [0.0f32; 2 * N];
            let (core, _handle) = engine_core();
            let run = Arc::new(Run::new([0, 0]));
            let mut input = DuplexInput::new(core.clone(), run.clone(), 1, RATE);
            let mut render = Render::duplex(core.clone(), run, 2, RATE);
            input.capture(&data, None);
            render.render(&mut out, Instant::now(), None);
            let duplex = core.meter_clip.each_ref().map(|c| c.swap(false, Relaxed));
            let mut j = Join::new();
            for _ in 0..8 {
                j.input.capture(&data, None);
                j.render.render(&mut out, j.entry, None);
                j.entry += Duration::from_millis(10);
            }
            let join = j.core.meter_clip.each_ref().map(|c| c.swap(false, Relaxed));
            [duplex, join]
        }};
    }
    macro_rules! check {
        ($T:ty, rails: $rails:expr, under: $under:expr) => {
            for x in $rails {
                assert_eq!(lit!(x), [[true; SLOT_COUNT]; 2], "{}: {x:?} is full scale", stringify!($T));
            }
            for x in $under {
                assert_eq!(lit!(x), [[false; SLOT_COUNT]; 2], "{}: {x:?} is one step under full scale", stringify!($T));
            }
        };
    }
    let below_one = f32::from_bits(1.0f32.to_bits() - 1);
    check!(f32, rails: [1.0f32, -1.0, 1.5], under: [below_one, -below_one]);
    check!(i32, rails: [i32::MAX, i32::MIN], under: [i32::MAX - 1, i32::MIN + 1]);
    check!(i16, rails: [i16::MAX, i16::MIN], under: [i16::MAX - 1, i16::MIN + 1]);
    check!(u16, rails: [u16::MAX, u16::MIN], under: [u16::MAX - 1, u16::MIN + 1]);
}

#[test]
fn a_lock_held_across_cycles_is_one_input_gap_whichever_input_it_costs() {
    // ASIO by hand: the engine lock taken just before or after a cycle's input, and given back just
    // before or after another's. The first output that takes it again follows the misses, and when
    // its input is out of step it is a duplex fault too: one block, one input gap.
    const B: usize = 256;
    for (after_input, before_input) in [(false, false), (false, true), (true, false), (true, true)] {
        let (core, _handle) = engine_core();
        let run = Arc::new(Run::new([0, 0]));
        let mut input = DuplexInput::new(core.clone(), run.clone(), 1, RATE);
        let mut render = Render::duplex(core.clone(), run, 2, RATE);
        let x = [0.25f32; B];
        let mut out = [0.0f32; 2 * B];
        let mut output = |render: &mut Render| render.render(&mut out, Instant::now(), None);
        for _ in 0..4 {
            input.capture(&x, None);
            output(&mut render);
        }
        if after_input {
            input.capture(&x, None);
        }
        let held = core.rt.lock().unwrap();
        if !after_input {
            input.capture(&x, None);
        }
        output(&mut render);
        for _ in 0..3 {
            input.capture(&x, None);
            output(&mut render);
        }
        if !before_input {
            input.capture(&x, None);
        }
        drop(held);
        if before_input {
            input.capture(&x, None);
        }
        output(&mut render);
        for _ in 0..4 {
            input.capture(&x, None);
            output(&mut render);
        }
        let case = format!("taken {} an input, given back {} one", if after_input { "after" } else { "before" }, if before_input { "before" } else { "after" });
        let counters = &core.counters;
        assert_eq!(counters.lock_misses.load(Relaxed), 7 + u64::from(!after_input) + u64::from(!before_input), "{case}");
        let faults = counters.duplex_faults.load(Relaxed);
        assert_eq!(faults, u64::from(after_input == before_input), "{case}: the resumed output's input is out of step");
        let xruns = core.rt.lock().unwrap().engine.as_ref().unwrap().diag().xruns;
        assert_eq!(xruns, 1, "{case}: one input gap");
    }
}

#[test]
fn the_latency_is_the_running_median_then_frozen() {
    let mut probe = Probe::new();
    let ms = |ms: u64| Some(Duration::from_millis(ms));
    assert_eq!(probe.observe(ms(10), 48_000), Some(480));
    assert_eq!(probe.observe(ms(30), 48_000), Some(960));
    assert_eq!(probe.observe(None, 48_000), None, "no report");
    assert_eq!(probe.observe(ms(2_000), 48_000), None, "not a latency");
    assert_eq!(probe.observe(ms(20), 48_000), Some(960));
    for _ in 3..LATENCY_SAMPLES {
        probe.observe(ms(20), 48_000);
    }
    assert_eq!(probe.observe(ms(50), 48_000), None, "frozen after the first reports");
}

/// The RT allocation count covers each callback whole: an allocation where the join's pull runs
/// (before the engine lock) and one in the input callback before its guard both count.
#[cfg(debug_assertions)]
#[test]
fn an_allocation_anywhere_in_a_join_callback_is_counted() {
    let mut j = Join::new();
    for _ in 0..20 {
        j.cycle(1);
    }
    let counted = |j: &Join| j.core.counters.rt_allocs.load(Relaxed);
    assert_eq!(counted(&j), 0, "the join allocates nothing");
    for site in ["join pull", "join input"] {
        let before = counted(&j);
        inject::alloc_at(site);
        j.cycle(1);
        assert_eq!(counted(&j), before + 1, "an allocation at the {site} counts");
    }
    assert!(!in_rt_callback(), "each callback's section ended with it");
}

/// A section inside another counts nothing twice, and the allocation counter's own guard inside one
/// leaves the outer counting.
#[cfg(debug_assertions)]
#[test]
fn a_section_inside_another_counts_once_and_the_outer_keeps_counting() {
    let counters = IoCounters::default();
    let alloc = || drop(std::hint::black_box(vec![0u8; 16]));
    {
        let _outer = RtSection::enter(&counters);
        {
            let _inner = RtSection::enter(&counters);
            alloc();
        }
        assert!(in_rt_callback(), "still inside the outer section");
        drop(crate::host::rt_alloc::guard());
        alloc();
    }
    assert!(!in_rt_callback());
    assert_eq!(counters.rt_allocs.load(Relaxed), 2);
}
