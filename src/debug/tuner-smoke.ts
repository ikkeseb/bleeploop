/**
 * DEV probe: the tuner in the real app on the real device (`docs/VERIFY.md`, native:tuner).
 * `pnpm native:tuner` launches the ASIO dev app in engine-smoke's profile, and this page drives the
 * tuner as the player does (the N key, the command bar's key, Escape, the overlay's close, a lane's
 * REC, an Audio Settings reopen), reading back through the feed, the DOM and the engine's scope taps:
 *
 *   device    the buffer (default 128) and rate pick as engine-smoke's; master volume 0.05 (the click
 *             faint, but above 0: the monitor's tap sits under it); slot 1 Off on input `--channel=`,
 *             no plugin, live (the input dry, MIC); the scope taps on (`SetScope`, the command the
 *             stage view's owner sends; the view itself would make the command bar inert)
 *   loop      a FIXED 1-bar first take on lane 1 at 120 BPM with the click, playing under the rest
 *   baseline  the tuner off, the monitor's scope columns are not all exactly zero (the input's noise).
 *             The feed sends a column to 3 decimals, so under the master's 0.05 only an input above
 *             about -40 dBFS shows: an unplugged input's noise reads 0, and the run then checks the rest
 *             and ends `mute: not measured` (the engine's tests hold the mute: `tests/tuner.rs`); with
 *             `--signal=1` a mute that cannot be measured fails the run
 *   open by N `Tuner` on arrives; the overlay shows; readings arrive; the monitor's columns from 28 ms
 *             after the event's frame until it closes are exactly zero, while lane 1 keeps PLAYING, the
 *             click's beats keep coming and the master's columns are not all zero
 *   quiet     (default) no reading has a pitch and the overlay says no signal after about a second;
 *             `--signal=1` (a guitar played into the input) wants a reading with a pitch instead
 *   Esc       `Tuner` off arrives, the overlay goes, the monitor's columns are not all zero again
 *   key/close the command bar's key opens it and the overlay's close closes it, with the same checks
 *   REC       REC on lane 2 over the tuner closes it no later than the take's first frame, and the take
 *             is one bar long; N while it records is refused (`TunerCapturing`) with the lane's cue
 *   device    a reopen at another buffer closes it; then back to the first buffer
 *
 * It hears nothing: the mute is measured on the engine's own taps, not in the room. It ends with
 * every lane empty, the tuner and the click off. `[tuner-smoke]` lines go through `console.error`.
 *
 * Trigger: `VITE_LF_PROBE=tuner-smoke` at Vite start (DEV only). Knobs:
 *   `VITE_LF_PROBE_BUFFER`   the ASIO buffer in frames (default 128)
 *   `VITE_LF_PROBE_RATE`     the rate pick, 44100 or 48000 (default: none, the driver's own rate)
 *   `VITE_LF_PROBE_CHANNEL`  0-based input channel slot 1 reads (default 0 = input 1)
 *   `VITE_LF_PROBE_SIGNAL`   `1`: a guitar plays into that input, so a reading must carry a pitch and the mute must be
 *                            measured
 */
import { framesPerBar } from '../ui/state/quantize';
import { usingAsio } from '../ui/state/audio-devices';
import { writeAudioDeviceSettings, type BufferFrames } from '../ui/state/audio-settings';
import { nativeHostReady, selectOff, setActiveSlot, slotPlugins } from '../ui/state/instrument';
import { slotOff } from '../ui/state/instrument-slots';
import { goLive, inputArmed } from '../ui/state/native-io';
import { platform, type EngineEvent } from '../platform';
import { SCOPE_MASTER, SCOPE_MONITOR, clock, looper, master, type ScopeView, type TunerReading } from '../ui/state/audio';
import { engineDevice, engineResets, onEngineEvent, setEngineInputChannel } from '../ui/state/engine-store';
import { pickDevice, probeRate } from './engine-smoke';

const TAG = '[tuner-smoke]';
const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
const log = (...args: unknown[]) => console.error(TAG, ...args);

/** The overlay's words (`src/ui/tuner/TunerView.tsx`) and the refusal's (`src/ui/looper/gates.ts`). */
const NO_SIGNAL_LINE = "no signal on the input, check the cable and the slot's input";
const REFUSED_LINE = 'a take is recording, the tuner opens after it';
/** The engine's mute is exactly silent this long after the tuner's frame (the 2 ms glide, a 4 ms
 * column and the block around it). */
const MUTE_MS = 28;
/** The tuner stays on this long in each open. */
const HOLD_MS = 2500;

async function until(label: string, predicate: () => boolean, seconds: number): Promise<void> {
  for (let i = 0; i < seconds * 50; i++) {
    if (predicate()) return;
    await sleep(20);
  }
  throw Error(`timed out after ${seconds} s waiting for ${label}`);
}

function check(ok: boolean, what: string): void {
  if (!ok) throw Error(what);
}

const lane = (i: number) => looper.track(i)();
const allEmpty = () => Array.from({ length: looper.trackCount }, (_, i) => lane(i).state).every((s) => s === 'EMPTY');
const overlay = () => document.querySelector<HTMLElement>('.tn');
const dB = (x: number) => (x > 0 ? (20 * Math.log10(x)).toFixed(1) : '-inf');

type Ev<T extends EngineEvent['type']> = Extract<EngineEvent, { type: T }>;

/** Every engine event since the probe began, in feed order. */
const events: EngineEvent[] = [];
/** The master volume the probe found, put back at the end of a run, a failed one too. */
let volumeBefore: number | null = null;
function eventsFrom<T extends EngineEvent['type']>(from: number, type: T): Ev<T>[] {
  return events.slice(from).filter((e): e is Ev<T> => e.type === type);
}
async function nextTuner(from: number, on: boolean, seconds = 5): Promise<Ev<'Tuner'>> {
  await until(`the feed's Tuner ${on ? 'on' : 'off'}`, () => eventsFrom(from, 'Tuner').some((e) => e.on === on), seconds);
  return eventsFrom(from, 'Tuner').find((e) => e.on === on)!;
}

/** A real keydown where the app listens (the window's transport keys), from what has the focus. */
function press(key: string, code: string): void {
  const target = document.activeElement ?? document.body;
  target.dispatchEvent(new KeyboardEvent('keydown', { key, code, bubbles: true, cancelable: true }));
}

/** One scope column of the monitor and the master: its first frame and its peak magnitude. */
interface Column {
  frame: number;
  mon: number;
  mas: number;
}

/** The scope mirror's columns as they arrive, each once, read every 30 ms (the ring holds ~4 s). */
function watchScope() {
  const view: ScopeView = { lo: null, hi: null, at: 0, count: 0, frame: 0, bin: 0, epoch: -1 };
  const cols: Column[] = [];
  let last = -Infinity;
  let bin = 0;
  const timer = setInterval(() => {
    looper.scopeInto(view);
    const { lo, hi } = view;
    if (!lo || !hi || view.count === 0) return;
    if (view.frame < last) last = -Infinity; // a new engine's frames start over
    bin = view.bin;
    const size = lo[0].length;
    const fresh: Column[] = [];
    for (let j = 0; j < view.count; j++) {
      const frame = view.frame - j * view.bin;
      if (frame <= last) break;
      const k = (view.at - 1 - j + size) % size;
      fresh.push({
        frame,
        mon: Math.max(Math.abs(lo[SCOPE_MONITOR][k]), Math.abs(hi[SCOPE_MONITOR][k])),
        mas: Math.max(Math.abs(lo[SCOPE_MASTER][k]), Math.abs(hi[SCOPE_MASTER][k])),
      });
    }
    last = view.frame;
    for (let i = fresh.length - 1; i >= 0; i--) cols.push(fresh[i]);
  }, 30);
  return { cols, bin: () => bin, stop: () => clearInterval(timer) };
}

/** The tuner's readings as they arrive (a new arrival stamp is a new reading), read every 10 ms. */
function watchReadings() {
  const view: TunerReading = { hz: null, level: -120, at: 0 };
  const seen: TunerReading[] = [];
  let at = 0;
  const timer = setInterval(() => {
    looper.tunerInto(view);
    if (view.at > 0 && view.at !== at) seen.push({ ...view });
    at = view.at;
  }, 10);
  return { seen, stop: () => clearInterval(timer) };
}

export async function runTunerSmoke(): Promise<void> {
  try {
    await run();
  } catch (e) {
    // Empty the looper and close the tuner first, so the runner's window close meets no jam question.
    looper.closeTuner();
    looper.clearAll();
    void looper.setScope(false);
    clock.setMetronome(false);
    if (volumeBefore !== null) master.setVolume(volumeBefore);
    await sleep(1000);
    log(`FAIL ${String(e instanceof Error ? e.message : e)}`);
  }
}

async function run(): Promise<void> {
  check(platform.engine.available, 'this platform has no engine');
  onEngineEvent((ev) => events.push(ev));
  const signal = import.meta.env.VITE_LF_PROBE_SIGNAL === '1';
  const channel = Number(import.meta.env.VITE_LF_PROBE_CHANNEL ?? 0);
  check(Number.isInteger(channel) && channel >= 0, `CHANNEL must be a 0-based channel, got ${import.meta.env.VITE_LF_PROBE_CHANNEL}`);

  // ── 1. Device ───────────────────────────────────────────────────────────────────────────────────
  await until('the engine device', () => engineDevice() !== null, 90);
  check(usingAsio(), 'ASIO is not in use: this probe runs on the ASIO driver');
  looper.closeTuner();
  looper.clearAll();
  await until('an empty looper', () => allEmpty() && looper.masterLengthFrames() === 0 && !clock.bpmLocked(), 5);
  const buffer = Number(import.meta.env.VITE_LF_PROBE_BUFFER ?? 128) as BufferFrames;
  const pick = probeRate();
  await pickDevice(buffer, pick);
  const device = engineDevice();
  check(device !== null && device.backend === 'Asio', 'no ASIO device runs');
  const rate = device!.sampleRate;
  check(pick === null || rate === pick, `the device runs at ${rate} Hz, not the ${pick} Hz picked`);
  const muteFrames = Math.ceil((MUTE_MS / 1000) * rate);
  // As Audio Settings' channel picker does: saved (a reopen takes it) and switched now.
  writeAudioDeviceSettings({ inputChannel: String(channel) });
  setEngineInputChannel(String(channel));
  volumeBefore = master.volume();
  master.setMuted(false);
  master.setVolume(0.05);
  // Slot 1 Off and live, as the player does it: the input dry.
  await until('the plugin host', () => nativeHostReady(), 300);
  setActiveSlot(0);
  selectOff(0);
  await until('slot 1 Off with no plugin', () => slotOff()[0] && !slotPlugins()[0], 30);
  await goLive(0);
  await until('the feed to show slot 1 live', () => inputArmed()[0], 5);
  check(await looper.setScope(true), 'SetScope(true) did not reach the engine');
  const scope = watchScope();
  await until('scope columns on the feed', () => scope.cols.length > 50, 5);
  log(
    `device: ${device!.backend} ${device!.inputName} → ${device!.outputName}, ${rate} Hz, ${device!.block} frames; slot 1 Off and live on input ${channel + 1} (channel ${channel}); master 0.05; scope on, ${scope.bin()}-frame columns; ${signal ? 'a guitar plays (--signal=1)' : 'a quiet input'}`,
  );

  // ── 2. Loop: a FIXED 1-bar first take on lane 1 ─────────────────────────────────────────────────
  clock.setBpm(120);
  await until('120 BPM on the feed', () => clock.bpm() === 120, 5);
  clock.setMetronome(true);
  looper.setFixedLengthEnabled(true);
  looper.setFixedLengthBars(1);
  const bar = framesPerBar(120, rate);
  void looper.recDub(0);
  await until('lane 1 recording', () => lane(0).state === 'RECORDING' && !lane(0).armed, 10);
  await until('lane 1 PLAYING', () => lane(0).state === 'PLAYING', 10);
  check(lane(0).lengthFrames === bar && looper.masterLengthFrames() === bar, `the loop is ${lane(0).lengthFrames} frames, master ${looper.masterLengthFrames()}, one bar is ${bar}`);
  log(`loop: lane 1 PLAYING, ${bar} frames (one bar at 120 BPM), the click on`);

  // ── 3. Baseline: the monitor carries the input's noise ──────────────────────────────────────────
  const b0 = scope.cols.length;
  // Slot 1's input meter beside it: it tells a silent input from a monitor that does not carry it.
  let inputPeak = 0;
  const meter = setInterval(() => (inputPeak = Math.max(inputPeak, looper.levelValue(0))), 20);
  await sleep(1200);
  clearInterval(meter);
  const base = scope.cols.slice(b0);
  const baseNz = base.filter((c) => c.mon !== 0);
  const basePeak = Math.max(0, ...base.map((c) => c.mon));
  log(
    `baseline: ${base.length} monitor columns over 1.2 s with the tuner off, ${baseNz.length} non-zero, peak ${basePeak.toExponential(2)} (${dB(basePeak)} dBFS after the master's 0.05); slot 1's input meter peak ${inputPeak.toExponential(2)} (${dB(inputPeak)} dBFS)`,
  );
  check(base.length > 100, `only ${base.length} scope columns arrived in 1.2 s`);
  // The feed sends a scope column to 3 decimals (`round`, src-tauri/src/engine_io/feed.rs): a monitor
  // under 5e-4 reads exactly 0, so a quiet input under the master's 0.05 cannot show the mute. Every
  // other check still runs; the verdict says `mute: not measured`, and fails only under `--signal=1`.
  const unmeasured =
    baseNz.length > 0
      ? null
      : inputPeak > 0
        ? `the monitor's columns read exactly 0 with the tuner off: slot 1's input peaks at ${inputPeak.toExponential(2)} (${dB(inputPeak)} dBFS), and the feed rounds a scope column to 3 decimals (under 5e-4 reads 0), so under the master's 0.05 the monitor shows only an input above about 1e-2 (-40 dBFS): the mute cannot be shown on this input (a guitar played: --signal=1)`
        : `the monitor's columns are all exactly zero with the tuner off, and slot 1's input meter reads 0: the input is digitally silent, so the mute cannot be shown`;
  if (unmeasured) log(`baseline: the mute is not measurable, the run goes on${signal ? ' and fails at the end' : ''}: ${unmeasured}`);

  // ── 4-6. Open by N, the quiet input (or a pitch), Esc closes ────────────────────────────────────
  const byKey = await session(
    'open by N',
    () => press('n', 'KeyN'),
    () => press('Escape', 'Escape'),
    { scope, rate, muteFrames, quiet: !signal, signal, measured: unmeasured === null },
  );
  log(byKey.open);
  log(byKey.input);
  log(`Esc closes: ${byKey.close}`);

  // ── 7. The command bar's key opens it, the overlay's close closes it ────────────────────────────
  const byButton = await session(
    'the command bar key',
    () => {
      const key = document.querySelector<HTMLButtonElement>('.tool--tuner');
      check(key !== null, 'no tuner key in the command bar');
      key!.click();
    },
    () => {
      const close = document.querySelector<HTMLButtonElement>('.tn-close');
      check(close !== null, 'the overlay has no close button');
      close!.click();
    },
    { scope, rate, muteFrames, quiet: !signal, signal, measured: unmeasured === null },
  );
  log(`key and close: ${byButton.open}; ${byButton.input}; ${byButton.close}`);

  // ── 8. REC over the tuner ───────────────────────────────────────────────────────────────────────
  const r0 = events.length;
  press('n', 'KeyN');
  await nextTuner(r0, true);
  await until('the overlay', () => overlay() !== null, 2);
  await sleep(500);
  const r1 = events.length;
  void looper.recDub(1);
  const off = await nextTuner(r1, false);
  await until('lane 2 recording', () => lane(1).state === 'RECORDING' && !lane(1).armed, 6);
  const takeEvents = eventsFrom(r1, 'Lane').filter((e) => e.lane === 1 && e.info.state === 'Recording');
  check(takeEvents.length > 0, 'no Lane event of lane 2 recording on the feed');
  const takeFirst = takeEvents[0].frame;
  const armedFirst = takeEvents.find((e) => e.info.armed)?.frame ?? null;
  const recFirst = takeEvents.find((e) => !e.info.armed)?.frame ?? null;
  check(off.frame <= takeFirst, `the tuner closed at frame ${off.frame}, after the take's first frame ${takeFirst}`);
  await until('the overlay gone', () => overlay() === null, 2);
  // N while the take records: refused, the overlay stays closed, the lane's cue says why.
  const r2 = events.length;
  press('n', 'KeyN');
  await until('a Refused TunerCapturing', () => eventsFrom(r2, 'Refused').some((e) => e.reason === 'TunerCapturing'), 3);
  const refusedWhile = lane(1).state === 'RECORDING';
  await until('the refusal cue on a lane', () => [...document.querySelectorAll('.lp-lane__wellmsg.is-cue')].some((el) => el.textContent?.includes(REFUSED_LINE)), 2);
  await sleep(300);
  check(!looper.tunerOn() && overlay() === null, 'the tuner opened while a take records');
  check(eventsFrom(r2, 'Tuner').length === 0, `the feed carried a Tuner event after the refused press: ${JSON.stringify(eventsFrom(r2, 'Tuner'))}`);
  await until('lane 2 PLAYING', () => lane(1).state === 'PLAYING', 6);
  check(lane(1).lengthFrames === bar, `lane 2's take is ${lane(1).lengthFrames} frames, the master ${bar}`);
  check(refusedWhile, 'the refusal came after lane 2 stopped recording');
  log(
    `REC over the tuner: Tuner off at frame ${off.frame}, lane 2 armed at ${armedFirst ?? 'none'}, recording at ${recFirst ?? 'none'} (off ${takeFirst - off.frame} frames before the take's first frame); the overlay gone; N while recording refused (TunerCapturing) with the lane's cue, no Tuner event; lane 2 PLAYING at ${lane(1).lengthFrames} frames`,
  );

  // ── 9. A stopped device closes it ───────────────────────────────────────────────────────────────
  looper.clearAll();
  await until('an empty looper', () => allEmpty() && looper.masterLengthFrames() === 0, 5);
  const d0 = events.length;
  press('n', 'KeyN');
  const onD = await nextTuner(d0, true);
  await until('the overlay', () => overlay() !== null, 2);
  await sleep(500);
  const other = (buffer === 256 ? 128 : 256) as BufferFrames;
  const resetsBefore = engineResets();
  const d1 = events.length;
  const cols0 = scope.cols.length;
  check(await pickDevice(other, pick), `the device did not reopen at ${other} frames`);
  await until('the tuner off after the reopen', () => !looper.tunerOn(), 5);
  await until('the overlay gone after the reopen', () => overlay() === null, 2);
  await sleep(1000);
  const offD = eventsFrom(d1, 'Tuner');
  check(!looper.tunerOn() && overlay() === null, 'the tuner came back on within a second of the reopen');
  const after = scope.cols.slice(cols0).filter((c) => c.frame > (offD.find((e) => !e.on)?.frame ?? 0) + muteFrames);
  log(
    `device stop: on at frame ${onD.frame}; reopened at ${engineDevice()?.block} frames; Tuner events on the feed since: ${offD.map((e) => `${e.on ? 'on' : 'off'}@${e.frame}`).join(',') || 'none'}; feed resets ${engineResets() - resetsBefore}; the tuner off and the overlay gone; then ${after.filter((c) => c.mon !== 0).length}/${after.length} monitor columns non-zero (info)`,
  );
  check(!offD.some((e) => e.on), 'the feed turned the tuner on again after the reopen');
  check(await pickDevice(buffer, pick), `the device did not reopen at ${buffer} frames`);
  await until('the device back', () => engineDevice()?.block === buffer, 10);

  // ── 10. End ─────────────────────────────────────────────────────────────────────────────────────
  scope.stop();
  void looper.setScope(false);
  looper.clearAll();
  await until('an empty looper at the end', () => allEmpty() && looper.masterLengthFrames() === 0, 5);
  check(!looper.tunerOn() && overlay() === null, 'the tuner is on at the end');
  clock.setMetronome(false);
  master.setVolume(volumeBefore);
  check(unmeasured === null || !signal, `every other check passed, but ${unmeasured}`);
  const mute = unmeasured === null ? `the mute held ${byKey.zero + byButton.zero} monitor columns at exactly 0` : 'mute: not measured (a quiet input)';
  log(`complete: ${rate} Hz, ${buffer} frames; ${mute}, ${byKey.readings + byButton.readings} readings`);
}

interface SessionOpts {
  scope: ReturnType<typeof watchScope>;
  rate: number;
  muteFrames: number;
  /** The input is quiet: no pitch, and the overlay says no signal after about a second. */
  quiet: boolean;
  /** A guitar plays: at least one reading carries a pitch. */
  signal: boolean;
  /** The monitor's columns showed the input with the tuner off, so its mute can be shown. */
  measured: boolean;
}

/**
 * Open the tuner by `open`, hold it `HOLD_MS`, close it by `close`, and check the feed, the overlay, the
 * readings and the scope's monitor and master across it. Returns the log lines and counts.
 */
async function session(label: string, open: () => void, close: () => void, o: SessionOpts) {
  const { scope } = o;
  const readings = watchReadings();
  const i0 = events.length;
  const t0 = performance.now();
  open();
  const on = await nextTuner(i0, true);
  await until('the overlay', () => overlay() !== null, 2);
  const shownMs = Math.round(performance.now() - t0);
  // Sample the lanes and the overlay while it holds.
  const states = new Set<string>();
  let noSignalAt: number | null = null;
  let noteShown = '';
  const sampler = setInterval(() => {
    states.add(lane(0).state);
    const quiet = document.querySelector('.tn-quiet')?.textContent ?? '';
    if (quiet === NO_SIGNAL_LINE && noSignalAt === null) noSignalAt = Math.round(performance.now() - t0);
    const spoken = document.querySelector('.tn-sr')?.textContent ?? '';
    if (/^[A-G]/.test(spoken)) noteShown = spoken;
  }, 20);
  await sleep(HOLD_MS);
  clearInterval(sampler);
  check(looper.tunerOn() && overlay() !== null, `${label}: the tuner closed by itself while held`);
  const i1 = events.length;
  const tClose = performance.now();
  close();
  const off = await nextTuner(i1, false);
  await until('the overlay gone', () => overlay() === null, 2);
  const goneMs = Math.round(performance.now() - tClose);
  const heldS = (tClose - t0) / 1000;
  readings.stop();
  await sleep(700); // the columns after the close reach the mirror
  const bin = scope.bin();

  // While on: the monitor exactly zero from 28 ms after the on frame to the off frame.
  const during = scope.cols.filter((c) => c.frame >= on.frame + o.muteFrames && c.frame + bin <= off.frame);
  const loud = during.filter((c) => c.mon !== 0);
  const masterNz = during.filter((c) => c.mas !== 0).length;
  const masterPeak = Math.max(0, ...during.map((c) => c.mas));
  const afterOff = scope.cols.filter((c) => c.frame >= off.frame + o.muteFrames);
  const afterNz = afterOff.filter((c) => c.mon !== 0).length;
  const beats = eventsFrom(i0, 'Beat').filter((b) => b.frame >= on.frame && b.frame < off.frame);
  const clicked = beats.filter((b) => b.clicked).length;
  const rs = readings.seen;
  const levels = rs.map((r) => r.level);
  const pitched = rs.filter((r) => r.hz !== null);
  const openLine =
    `${label}: Tuner on at frame ${on.frame}, overlay ${shownMs} ms after the press; ${rs.length} readings in ${heldS.toFixed(1)} s (${(rs.length / heldS).toFixed(1)}/s), level ${levels.length ? `${Math.min(...levels).toFixed(1)}..${Math.max(...levels).toFixed(1)}` : 'none'} dBFS; ` +
    `monitor ${during.length - loud.length}/${during.length} columns exactly 0${o.measured ? '' : ' (not measurable: 0 before it too)'} from ${o.muteFrames} frames after the on frame; lane 1 ${[...states].join(',')}, ${clicked}/${beats.length} beats clicked; master ${masterNz}/${during.length} columns non-zero, peak ${dB(masterPeak)} dBFS`;
  const closeLine = `Tuner off at frame ${off.frame} (${off.frame - on.frame} frames on), overlay gone ${goneMs} ms after; monitor ${afterNz}/${afterOff.length} columns non-zero after it`;

  check(rs.length >= heldS * 10, `${label}: only ${rs.length} readings in ${heldS.toFixed(1)} s`);
  // Every column of the interval, less a few at its edges: a hole in the scope could hide sound.
  const owed = Math.floor((off.frame - on.frame - o.muteFrames) / bin);
  check(during.length >= owed - 3, `${label}: only ${during.length} of ${owed} monitor columns while on`);
  if (loud.length) {
    const first = loud[0];
    check(false, `${label}: ${loud.length}/${during.length} monitor columns non-zero while on, the first at frame ${first.frame} (${first.frame - on.frame} frames after the on frame), ${first.mon.toExponential(2)}`);
  }
  check([...states].join() === 'PLAYING', `${label}: lane 1 was ${[...states].join(',')} while the tuner was on`);
  check(beats.length >= Math.floor((off.frame - on.frame) / (o.rate / 2)) && clicked === beats.length, `${label}: ${clicked}/${beats.length} beats clicked in ${heldS.toFixed(1)} s on`);
  check(masterNz > 0, `${label}: the master's columns were all zero while on`);
  check(afterOff.length > 50 && (!o.measured || afterNz > 0), `${label}: the monitor's columns stayed zero after the close (${afterNz}/${afterOff.length} non-zero)`);

  let input = '';
  if (o.quiet) {
    if (pitched.length) {
      const p = pitched[0];
      check(false, `quiet input: ${pitched.length}/${rs.length} readings carried a pitch, the first ${p.hz} Hz at ${p.level} dBFS`);
    }
    check(noSignalAt !== null, `quiet input: the overlay never said no signal in ${HOLD_MS} ms (level ${levels.length ? Math.max(...levels).toFixed(1) : 'none'} dBFS at most)`);
    check(noSignalAt! >= 900 && noSignalAt! <= 2000, `quiet input: the overlay said no signal ${noSignalAt} ms after the press`);
    input = `quiet input: 0/${rs.length} readings carried a pitch; the overlay said no signal ${noSignalAt} ms after the press`;
  } else if (o.signal) {
    check(pitched.length > 0, `signal: no reading carried a pitch (${rs.length} readings, level ${levels.length ? `${Math.min(...levels).toFixed(1)}..${Math.max(...levels).toFixed(1)}` : 'none'} dBFS)`);
    check(noteShown !== '', 'signal: a reading carried a pitch, but the overlay named no note');
    const p = pitched[pitched.length - 1];
    input = `signal: ${pitched.length}/${rs.length} readings carried a pitch, the last ${p.hz} Hz at ${p.level} dBFS; the overlay showed "${noteShown}"`;
  }
  return { open: openLine, close: closeLine, input, zero: during.length, readings: rs.length };
}
