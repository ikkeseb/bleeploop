/**
 * DEV probe: where takes land against the click on the native engine, in the real running app, through
 * a physical loopback cable (an interface output wired into an input). The cable is a player who hits
 * every click exactly as it leaves the interface. The web path's version is `src/debug/loopback-sync.ts`;
 * the bar it serves is `docs/plans/native-engine.md` § Stage 5, "Parity checklist".
 *
 * The engine starts a take the driver's input + output latency, the live effect's latency and the master
 * limiter's pre-delay after its downbeat (`ProcessContext::align_frames`, lf-engine `api.rs`), so a
 * correctly aligned take puts the cable's click ON its beat. Every offset here is the recorded click's
 * onset minus its beat frame in the committed lane PCM (`engine_snapshot`: play order, loop position 0 =
 * the master downbeat), + = late; nothing is fitted. The onset is loopback-sync's (the window's peak,
 * then the first 30 % crossing, each beat searched from half a beat early, the loop read circularly so
 * an early downbeat is found at the loop's end). The engine's click rises over 2 ms, so the crossing
 * lags the click's first frame even on the ideal click: that lag, measured by the same detector on the
 * engine's own formula (`referenceClick`, lf-engine `clock.rs`), is subtracted ("net"; "raw" keeps it).
 *
 * For each buffer size in BUFFERS (the device reopened as Audio Settings does), lanes cleared, 120 BPM:
 *   A  lane 1, a FIXED first take of BARS bars, click on → clickX_A, its spread and drift
 *   B  lane 2, a later take, click off, lane 1 playing: its recorded clicks go out through the cable
 *      → loopX_B, ≈ 2·clickX_A when the record alignment is the only error
 *   STOP ALL → PLAY ALL: an idle transport restarts from the top and re-anchors the grid
 *   C  lane 3, click on, lanes 1–2 muted → clickX_C: does the click keep the grid after the restart?
 *   D  lane 4, click off, lane 1 playing, lanes 2–3 muted → loopX_D: do the loops keep their place?
 *   E  lane 5, a multiply (F14): FIXED at two loops, click on, lanes 1–4 muted → clickX_E; the loop is
 *      two loops long after it, and lane 1 holds its loop twice, bit for bit
 *   F  lane 4 cleared and taken again over the grown loop, click on → clickX_F: the click after the
 *      multiply's re-anchor
 *   CLEAR ALL, then a FIXED first take of half the bars (at least 2) on lane 1, click on: the small loop
 *   G  lane 2, a FREE take (FIXED off, E10), click on for its first 0.6 loops, lane 1 muted, stopped by
 *      REC 1.75 loops in → clickX_G: it records on to two loops and the loop grows to them; lane 1
 *      holds its loop twice, bit for bit
 *   H  lane 2 TRIMmed to half the small loop's bars (F16): its committed PCM is those bars repeated,
 *      bit for bit; then lane 3 takes lane 2 through the cable, click off, lane 1 muted → loopX_H: every
 *      beat of the grown loop clicks (the untrimmed lane is silent past its first loop's clicks), on the
 *      grid of B; UNDO gives lane 2 back as it was, bit for bit
 *   I  FADE (all) over two bars, pressed half a second before a loop boundary with lane 1 (the small
 *      loop's clicks) alone audible, and a take on lane 4 from that boundary, click off: the fading lanes
 *      stop on the downbeat two bars on (the feed's beats and lane events), each beat's click through the
 *      cable falls, at ((8 − k)/8)² of the first (the fade's squared ramp, k the beat), and silence
 *      follows the bar line; PLAY ALL then brings lane 1 back at its level (the input meter's peak, and
 *      its stored volume)
 * then CLEAR ALL, so the runner's window close meets no jam question. The pass bars print per buffer;
 * the verdict is `complete: …` when every bar passes, `FAIL …` otherwise. A whole-beat offset shows as
 * the accent off beat 1; a whole-bar one cannot show (every bar of the loop sounds alike).
 *
 * Feedback: the record tap is the live slot's output, and the engine plays that same wet after the
 * limiter, so the cable closes a loop (input → slot gain → master volume → out → cable → input). The
 * slot gain scales the heard and the recorded wet alike and the master volume scales the click with the
 * wet, so no setting records the cable without hearing it: the probe runs the product path (the effect's
 * default gain, master 1) and reports the loop gain it measured. A clipping input stops the live slot and
 * fails the run. `[engine-loopback]` lines go through `console.error` (→ the same log as the Rust host).
 *
 * Trigger: `VITE_LF_PROBE=engine-loopback` at Vite start (DEV only). Knobs:
 *   `VITE_LF_PROBE_CHANNEL`  0-based input channel of the cable (default `1` = input 2)
 *   `VITE_LF_PROBE_BUFFERS`  ASIO buffer sizes, in order (default `64,128,256`)
 *   `VITE_LF_PROBE_BARS`     take length in bars at 120 BPM (default 8)
 *   `VITE_LF_PROBE_PLUGIN`   `<name substring>[:<format>]` loaded into slot 1 and taken live (default
 *                            `Pro-Q:vst3`); empty or `none`: MIC, the input dry through an empty live slot
 *   `VITE_LF_PROBE_ECHO`     `1`: IN FX's ECHO (1/16, no feedback, level 0.5) is on while A records, and
 *                            every click in A must have its echo clear of the noise floor, each one
 *                            sixteenth later (within 0.05 ms) at half its level (0.45..0.55), beat by
 *                            beat: a lost echo is named, never averaged away. The check first passes a
 *                            synthetic take with every echo and fails one whose echoes stop 20 beats
 *                            into 32 (`echoSelfTest`). C (echo off) = A is then the bar that the echo
 *                            leaves the dry click where it was
 *   `VITE_LF_PROBE_UNLOAD_FIRST`  with MIC only: `<name substring>[:<format>]` loaded into slot 1 and
 *                            unloaded before MIC takes that slot, so the MIC takes' peak gain shows what
 *                            the unload left on the slot (compare it with a run without this knob)
 */
import { framesPerBar } from '../audio/quantize';
import { setBufferSize, usingAsio } from '../audio/audio-devices';
import { BUFFER_FRAMES_OPTIONS, writeAudioDeviceSettings, type BufferFrames } from '../audio/audio-settings';
import { availablePlugins, clearPlugin, nativeHostReady, pluginGain, selectPlugin, slotPlugins } from '../audio/instrument';
import { goLive, inputArmed, stopLive } from '../audio/native-io';
import { engineMode, type DeviceStatus, type PluginDescriptor } from '../platform';
import { clock, looper, master, session } from '../ui/state/audio';
import { engineDevice, engineFade, engineInputSends, onEngineEvent, openEngineDevice, setEngineInputChannel, trimLane } from '../ui/state/engine-store';

const TAG = '[engine-loopback]';
const BPM = 120;
/** The accent marks beat 1 of each bar: a take a whole beat off, or a click whose bar restarts on
 * another beat, shows only there. */
const BEATS_PER_BAR = 4;
/** The engine's default click volume: the accent peaks at 0.7, under the limiter's −1 dB threshold. */
const CLICK_VOLUME = 0.7;
/** A beat's peak must stand this far above the take's median |x| (the noise floor) to count as a click:
 * loopback-sync's 8 would let a noise peak cross 30 % of a weak click; 20 keeps 0.3 × peak above it. */
const FLOOR_FACTOR = 20;

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
const log = (msg: string) => console.error(`${TAG} ${msg}`);
const toMs = (frames: number, rate: number) => (frames / rate) * 1000;
const signed = (x: number, digits = 2) => `${x >= 0 ? '+' : ''}${x.toFixed(digits)}`;
const median = (xs: readonly number[]) => {
  const s = [...xs].sort((a, b) => a - b);
  return s.length ? (s[(s.length - 1) >> 1] + s[s.length >> 1]) / 2 : NaN;
};

function check(ok: boolean, what: string): void {
  if (!ok) throw Error(what);
}

const lane = (i: number) => looper.track(i)();
const allEmpty = () => Array.from({ length: looper.trackCount }, (_, i) => lane(i).state).every((s) => s === 'EMPTY');

// ── The input guard: a clipping input means the loop through the cable runs away ─────────────────────

const guard = { max: 0, clipped: false, timer: null as ReturnType<typeof setInterval> | null };

function watchInput(): void {
  guard.max = 0;
  guard.clipped = false;
  guard.timer = setInterval(() => {
    const level = looper.levelValue();
    guard.max = Math.max(guard.max, level);
    if (level >= 1) guard.clipped = true;
  }, 40);
}

function unwatchInput(): void {
  if (guard.timer !== null) clearInterval(guard.timer);
  guard.timer = null;
}

async function until(label: string, predicate: () => boolean, seconds: number): Promise<void> {
  for (let i = 0; i < seconds * 50; i++) {
    if (guard.clipped) throw Error(`the input clipped while waiting for ${label}: feedback through the cable, or the input gain is too high`);
    if (predicate()) return;
    await sleep(20);
  }
  throw Error(`timed out after ${seconds} s waiting for ${label}`);
}

// ── The click and its onset ───────────────────────────────────────────────────────────────────────────

/** The triangle's Fourier series up to Nyquist (lf-engine `clock.rs` `triangle`). */
function triangle(freq: number, t: number, rate: number): number {
  let sum = 0;
  let sign = 1;
  for (let k = 1; k * freq < rate / 2; k += 2) {
    sum += (sign * Math.sin(2 * Math.PI * k * freq * t)) / (k * k);
    sign = -sign;
  }
  return (sum * 8) / (Math.PI * Math.PI);
}

/** The engine's click from its first frame, as `ClickVoice::render` (lf-engine `clock.rs`) plays it: a
 * band-limited triangle at 1 kHz (1.5 kHz accented), a 2 ms linear attack, an exponential decay to 1e-4
 * at 60 ms, silence from 70 ms. */
function referenceClick(accent: boolean, rate: number): Float32Array {
  const peak = (accent ? 1 : 0.56) * CLICK_VOLUME;
  const freq = accent ? 1500 : 1000;
  const out = new Float32Array(Math.round(0.07 * rate));
  for (let k = 0; k < out.length; k++) {
    const t = k / rate;
    const gain = t < 0.002 ? (peak * t) / 0.002 : t < 0.06 ? peak * (1e-4 / peak) ** ((t - 0.002) / 0.058) : 1e-4;
    out[k] = gain * triangle(freq, t, rate);
  }
  return out;
}

function peakIn(x: Float32Array, from: number, to: number): { at: number; value: number } {
  let at = -1;
  let value = 0;
  for (let i = Math.max(0, from); i < Math.min(x.length, to); i++) {
    if (Math.abs(x[i]) > value) {
      value = Math.abs(x[i]);
      at = i;
    }
  }
  return { at, value };
}

/** Where |x| first reaches `level` in [from, to), interpolated between samples; -1 if never. */
function crossing(x: Float32Array, from: number, to: number, level: number): number {
  for (let i = Math.max(0, from); i < Math.min(x.length, to); i++) {
    const a = Math.abs(x[i]);
    if (a < level) continue;
    if (i === from) return i;
    const b = Math.abs(x[i - 1]);
    return i - 1 + (level - b) / (a - b);
  }
  return -1;
}

/** The detector on one window: its peak and the 30 % crossing before it. */
function onsetOf(x: Float32Array): { onset: number; peak: number } {
  const p = peakIn(x, 0, x.length);
  return { onset: p.at < 0 ? -1 : crossing(x, 0, p.at + 1, p.value * 0.3), peak: p.value };
}

/** Least-squares slope of y over x. */
function slope(xs: readonly number[], ys: readonly number[]): number {
  const n = xs.length;
  const mx = xs.reduce((a, b) => a + b, 0) / n;
  const my = ys.reduce((a, b) => a + b, 0) / n;
  let num = 0;
  let den = 0;
  for (let i = 0; i < n; i++) {
    num += (xs[i] - mx) * (ys[i] - my);
    den += (xs[i] - mx) ** 2;
  }
  return den ? num / den : NaN;
}

interface Reference {
  accent: { onset: number; peak: number };
  plain: { onset: number; peak: number };
}

function reference(rate: number): Reference {
  return { accent: onsetOf(referenceClick(true, rate)), plain: onsetOf(referenceClick(false, rate)) };
}

interface TakeStats {
  found: number;
  beats: number;
  /** ms, net of the detector's lag on the ideal click: median, min, max. */
  x: number;
  min: number;
  max: number;
  /** ms, the median before that subtraction. */
  raw: number;
  /** ms per minute of take. */
  drift: number;
  /** Median recorded peak over the ideal click's peak: the cable's gain times the slot gain (A, C); for
   * B and D that path twice. */
  gain: number;
  /** Beats where the accent and beat 1 of the bar disagree: an accent off beat 1, or a beat 1 unaccented. */
  offBar: number[];
}

/** A take's noise floor, its median |x|, from every 64th sample: sorting the whole take would stall the
 * main thread. */
function noiseFloor(pcm: Float32Array): number {
  return median(Array.from({ length: Math.floor(pcm.length / 64) }, (_, i) => Math.abs(pcm[i * 64])));
}

/** Every beat's click in a committed lane, against its beat frame. */
function analyse(name: string, laneIndex: number, pcm: Float32Array, rate: number, ref: Reference, channel: number): TakeStats {
  const beat = (rate * 60) / BPM;
  const beats = Math.round(pcm.length / beat);
  const half = Math.round(beat / 2);
  const n = pcm.length;
  const floor = noiseFloor(pcm);
  const win = new Float32Array(Math.round(beat));
  const hits: { k: number; raw: number; peak: number }[] = [];
  let loudest = 0;
  for (let k = 0; k < beats; k++) {
    const start = Math.round(k * beat) - half;
    for (let i = 0; i < win.length; i++) win[i] = pcm[(((start + i) % n) + n) % n];
    const { onset, peak } = onsetOf(win);
    loudest = Math.max(loudest, peak);
    if (onset < 0 || peak < Math.max(floor * FLOOR_FACTOR, 1e-6)) continue;
    hits.push({ k, raw: onset - half, peak });
  }
  if (hits.length < beats / 2) {
    throw Error(
      `take ${name} (lane ${laneIndex + 1}): only ${hits.length}/${beats} clicks found (loudest ${loudest.toExponential(2)}, floor ${floor.toExponential(2)}): is the cable in input ${channel + 1} (0-based channel ${channel}) and its gain up?`,
    );
  }
  // The accent peaks 1/0.56 above the other beats: split at the geometric middle.
  const mid = median(hits.map((h) => h.peak));
  const accentAt = Math.sqrt(1 / 0.56);
  const accented = (h: { peak: number }) => h.peak > mid * accentAt;
  const net = hits.map((h) => h.raw - (accented(h) ? ref.accent.onset : ref.plain.onset));
  const netMs = net.map((f) => toMs(f, rate));
  const minutes = hits.map((h) => (h.k * beat) / rate / 60);
  const gain = median(hits.map((h) => h.peak / (accented(h) ? ref.accent.peak : ref.plain.peak)));
  const stats: TakeStats = {
    found: hits.length,
    beats,
    x: median(netMs),
    min: Math.min(...netMs),
    max: Math.max(...netMs),
    raw: toMs(median(hits.map((h) => h.raw)), rate),
    drift: slope(minutes, netMs),
    gain,
    offBar: hits.filter((h) => accented(h) !== (h.k % BEATS_PER_BAR === 0)).map((h) => h.k),
  };
  const accents = hits.filter(accented).length;
  log(
    `take ${name}: lane ${laneIndex + 1}, ${stats.found}/${beats} clicks (${accents} accented, ${stats.offBar.length ? `off beat 1 at ${stats.offBar.join(',')}` : 'all on beat 1'}), offset ${signed(stats.x, 3)} ms net (raw ${signed(stats.raw, 3)}), spread ${signed(stats.min, 3)}..${signed(stats.max, 3)} ms (${(stats.max - stats.min).toFixed(3)}), drift ${signed(stats.drift, 3)} ms/min, peak gain ${stats.gain.toFixed(3)}, floor ${floor.toExponential(2)}`,
  );
  // Every eighth beat's offset, so a step inside the take shows.
  const every = Math.max(1, Math.floor(hits.length / 8));
  log(`  take ${name} beats: ${hits.filter((_, i) => i % every === 0).map((h, i) => `${h.k}:${signed(netMs[i * every], 3)}`).join(' ')}`);
  return stats;
}

/** IN FX's echo in a take of the click (ECHO at 1/16, no feedback), beat by beat. */
interface EchoStats {
  /** Beats whose click stands clear of the noise floor, as `analyse` counts a click. */
  clicks: number;
  /** Of those, the beats whose echo does not. */
  missing: number[];
  /** Each echo found: its onset off one sixteenth after its click's (ms), and its peak over the click's. */
  delays: number[];
  ratios: number[];
}

function echoIn(pcm: Float32Array, rate: number): EchoStats {
  const beat = (rate * 60) / BPM;
  const sixteenth = beat / 4;
  const reach = Math.round(beat / 8);
  const beats = Math.round(pcm.length / beat);
  const clear = Math.max(noiseFloor(pcm) * FLOOR_FACTOR, 1e-6);
  const at = (from: number) => {
    const win = new Float32Array(2 * reach);
    const start = Math.round(from) - reach;
    for (let i = 0; i < win.length; i++) win[i] = pcm[(((start + i) % pcm.length) + pcm.length) % pcm.length];
    const o = onsetOf(win);
    return { onset: o.onset < 0 || o.peak < clear ? NaN : start + o.onset, peak: o.peak };
  };
  const stats: EchoStats = { clicks: 0, missing: [], delays: [], ratios: [] };
  for (let k = 0; k < beats; k++) {
    const click = at(k * beat);
    if (!Number.isFinite(click.onset)) continue;
    stats.clicks++;
    const echo = at(k * beat + sixteenth);
    if (!Number.isFinite(echo.onset)) {
      stats.missing.push(k);
      continue;
    }
    stats.delays.push(toMs(echo.onset - click.onset - sixteenth, rate));
    stats.ratios.push(echo.peak / click.peak);
  }
  return stats;
}

type Bar = { name: string; ok: boolean; value: string };

/** The echo's bars over a take of `clicks` clicks: an echo after every one, each on time and at half its
 * click. Per beat, so a take whose echo stops partway fails however many beats came before. */
function echoBars(echo: EchoStats, clicks: number): Bar[] {
  const found = echo.delays.length;
  const worst = echo.delays.reduce((w, d) => (Math.abs(d) > Math.abs(w) ? d : w), 0);
  const [lo, hi] = found ? [Math.min(...echo.ratios), Math.max(...echo.ratios)] : [NaN, NaN];
  const missing = echo.missing.length ? `, none after beat(s) ${echo.missing.join(',')}` : '';
  return [
    { name: 'an echo in A after every click', ok: echo.missing.length === 0 && found >= clicks, value: `${found}/${clicks} clicks${missing}` },
    { name: 'every echo in A one sixteenth after its click, within 0.05 ms', ok: found > 0 && Math.abs(worst) <= 0.05, value: `worst ${signed(worst, 3)} ms over ${found}` },
    { name: 'every echo in A at half its click (0.45..0.55)', ok: found > 0 && lo >= 0.45 && hi <= 0.55, value: `${lo.toFixed(3)}..${hi.toFixed(3)}` },
  ];
}

/** The echo check against a synthetic take of 32 beats of the ideal click, each echoed at half its level
 * one sixteenth later: it must pass with every echo and fail when the echoes stop after the 20th beat (a
 * review's case: a median over the beats passed it). A check that cannot tell fails the run. */
function echoSelfTest(rate: number): void {
  const beat = (rate * 60) / BPM;
  const beats = 32;
  // Without its last 10 ms at 1e-4, which a real take's noise floor buries and a silent one would not.
  const click = referenceClick(false, rate).subarray(0, Math.round(0.06 * rate));
  const bars = (echoed: number) => {
    const pcm = new Float32Array(Math.round(beats * beat));
    const put = (from: number, gain: number) => {
      const start = Math.round(from);
      for (let i = 0; i < click.length; i++) pcm[(start + i) % pcm.length] += gain * click[i];
    };
    for (let k = 0; k < beats; k++) {
      put(k * beat, 1);
      if (k < echoed) put(k * beat + beat / 4, 0.5);
    }
    return echoBars(echoIn(pcm, rate), beats);
  };
  const whole = bars(beats);
  check(whole.every((b) => b.ok), `the echo check fails a synthetic take with every echo: ${whole.map((b) => `${b.name}: ${b.value}`).join('; ')}`);
  const dropout = bars(20);
  check(!dropout[0].ok && dropout[0].value.endsWith(Array.from({ length: 12 }, (_, i) => 20 + i).join(',')), `the echo check misses the lost echoes of a synthetic take whose echoes stop after 20 of 32 beats: ${dropout[0].value}`);
}

/** Every beat's click in the first `beats` beats of a committed lane (the rest of the take is silent). */
function analyseFirst(name: string, laneIndex: number, pcm: Float32Array, beats: number, rate: number, ref: Reference, channel: number): TakeStats {
  return analyse(name, laneIndex, pcm.subarray(0, Math.round((beats * rate * 60) / BPM)), rate, ref, channel);
}

/** How many samples of `got` differ from `pcm`'s first `frames` repeated out to `got`'s length (the
 * snapshot already held `got` to the master's length). */
function tiledDiff(got: Float32Array, pcm: Float32Array, frames: number): number {
  check(frames > 0 && frames <= pcm.length, `a tile of ${frames} frames from ${pcm.length}`);
  let diff = 0;
  for (let k = 0; k < got.length; k++) if (got[k] !== pcm[k % frames]) diff++;
  return diff;
}

/** The committed PCM of lane `i`, from the engine's snapshot. */
async function committedPcm(i: number, masterFrames: number): Promise<Float32Array> {
  const snap = await session.exportSnapshot();
  const t = snap.tracks.find((tr) => tr.index === i);
  check(t !== undefined, `the snapshot holds no lane ${i + 1}`);
  check(snap.masterLengthFrames === masterFrames && t!.pcm.length === masterFrames, `lane ${i + 1} is ${t!.pcm.length} frames, master ${snap.masterLengthFrames}, expected ${masterFrames}`);
  check(!t!.reversed, `lane ${i + 1} is reversed`);
  return t!.pcm;
}

/** Record lane `i` (a FIXED take of `bars` bars) and wait for it to commit. */
async function take(i: number, name: string, seconds: number): Promise<number> {
  const t0 = performance.now();
  void looper.recDub(i);
  await until(`take ${name} on lane ${i + 1} to start`, () => lane(i).state === 'RECORDING', 5);
  await until(`take ${name} on lane ${i + 1} to commit`, () => lane(i).state === 'PLAYING', seconds);
  return Math.round(performance.now() - t0);
}

/** Phase I's fade: every beat of its bars, and what came after. */
interface FadeStats {
  /** Each beat's click peak in the take over the playing lane's own click at that beat, relative to the
   * first beat's: the fade's gain through the cable. */
  levels: number[];
  /** The loudest window past the fade's bar line, over the take's noise floor bar (0 = silent). */
  after: number;
  /** The fading lanes stopped on a downbeat the fade's bars on (the feed's beats and lane events). */
  onBar: boolean;
  stops: string;
  /** Lane 1's input-meter peak after PLAY ALL over before the fade, and whether its volume stayed. */
  back: number;
  volumeKept: boolean;
}

/** A take's click peak at each of its first `beats` beats (half a beat either side). */
function beatPeaks(pcm: Float32Array, beats: number, rate: number): number[] {
  const beat = (rate * 60) / BPM;
  return Array.from({ length: beats }, (_, k) => peakIn(pcm, Math.round((k - 0.5) * beat), Math.round((k + 0.5) * beat)).value);
}

/** The loudest input-meter reading over `ms`. The meter holds each feed frame's peak for one tick
 * (60 Hz), so it is read faster than that: at 20 ms one frame in six went unread, and with it, in one
 * run, the bar's only accent (the reading then was a plain beat's, 0.56 of it). */
async function meterPeak(ms: number): Promise<number> {
  let max = 0;
  for (const end = performance.now() + ms; performance.now() < end; ) {
    max = Math.max(max, looper.levelValue());
    await sleep(4);
  }
  return max;
}

interface BufferResult {
  device: DeviceStatus;
  a: TakeStats;
  b: TakeStats;
  c: TakeStats;
  d: TakeStats;
  e: TakeStats;
  f: TakeStats;
  g: TakeStats;
  h: TakeStats;
  fade: FadeStats;
  echo: EchoStats | null;
  bars: Bar[];
}

// ── The run ───────────────────────────────────────────────────────────────────────────────────────────

/** The slot the input feeds (the plugin's, or MIC's empty one): taken off live on a failure. */
let liveSlot: 0 | 1 | null = null;

export async function runEngineLoopback(): Promise<void> {
  try {
    await run();
  } catch (e) {
    unwatchInput();
    // Break the loop through the cable first, then empty the looper for the runner's window close.
    if (liveSlot !== null) await stopLive(liveSlot).catch(() => {});
    clock.setMetronome(false);
    looper.clearAll();
    await sleep(1000);
    log(`FAIL ${String(e instanceof Error ? e.message : e)}`);
  }
}

/** Load the scanned plugin matching `name` (and `format`) into slot 1, once the scan has finished. */
async function loadInSlot1(name: string, format: string | undefined): Promise<PluginDescriptor> {
  for (let waited = 0; !(nativeHostReady() && availablePlugins().length > 0); waited += 60) {
    check(waited < 600, 'the plugin scan did not finish within 10 min');
    log(`  waiting for the plugin scan (${waited} s)`);
    const deadline = performance.now() + 60_000;
    while (performance.now() < deadline && !(nativeHostReady() && availablePlugins().length > 0)) await sleep(200);
  }
  const desc = availablePlugins().find((d) => d.name.toLowerCase().includes(name) && (!format || d.format === format));
  check(desc !== undefined, `no scanned plugin matches "${name}${format ? `:${format}` : ''}"`);
  if (slotPlugins()[0]?.id !== desc!.id) await selectPlugin(0, desc!);
  check(slotPlugins()[0]?.id === desc!.id, `could not load ${desc!.name}`);
  return desc!;
}

async function run(): Promise<void> {
  check(engineMode(), 'engine mode is off in this profile (the runner writes its toggle file)');
  const channel = Number(import.meta.env.VITE_LF_PROBE_CHANNEL ?? 1);
  check(Number.isInteger(channel) && channel >= 0, `CHANNEL must be a 0-based channel, got ${import.meta.env.VITE_LF_PROBE_CHANNEL}`);
  const buffers = String(import.meta.env.VITE_LF_PROBE_BUFFERS ?? '64,128,256')
    .split(',')
    .map((s) => Number(s.trim()));
  for (const b of buffers) check((BUFFER_FRAMES_OPTIONS as readonly number[]).includes(b), `BUFFERS: ${b} is not one of ${BUFFER_FRAMES_OPTIONS.join(',')}`);
  const bars = Number(import.meta.env.VITE_LF_PROBE_BARS ?? 8) || 8;
  const pluginKnob = String(import.meta.env.VITE_LF_PROBE_PLUGIN ?? 'Pro-Q:vst3').trim().toLowerCase();
  const [want, format] = pluginKnob === 'none' ? [''] : pluginKnob.split(':');
  const rejected: string[] = [];
  onEngineEvent((ev) => {
    if (ev.type === 'TakeRejected') rejected.push(`lane ${ev.lane + 1}${ev.overdub ? ' overdub' : ''}`);
  });

  // ── The device and the input ────────────────────────────────────────────────────────────────────────
  await until('the engine device', () => engineDevice() !== null, 90);
  check(usingAsio() && engineDevice()?.backend === 'Asio', 'ASIO is not in use: this probe runs on the ASIO driver');
  // As Audio Settings' channel picker does: saved (a reopen takes it) and switched now.
  writeAudioDeviceSettings({ inputChannel: String(channel) });
  setEngineInputChannel(String(channel));
  looper.clearAll();
  await until('an empty looper', () => allEmpty() && looper.masterLengthFrames() === 0, 5);

  let source: string;
  let short: string;
  if (want) {
    const desc = await loadInSlot1(want, format);
    await goLive(0);
    check(inputArmed()[0], `GO LIVE did not take ${desc.name} live`);
    short = `${desc.name} [${desc.format}]`;
    source = `${short} live in slot 1, gain ${pluginGain()[0]}`;
  } else {
    const unloadKnob = String(import.meta.env.VITE_LF_PROBE_UNLOAD_FIRST ?? '').trim().toLowerCase();
    let unloaded = '';
    if (unloadKnob) {
      const [name, fmt] = unloadKnob.split(':');
      const desc = await loadInSlot1(name, fmt);
      await clearPlugin(0);
      check(!slotPlugins()[0], `could not unload ${desc.name}`);
      unloaded = `, after ${desc.name} [${desc.format}] was loaded there and unloaded`;
    }
    check(!slotPlugins()[0] || !slotPlugins()[1], 'MIC needs an empty slot');
    if (!looper.inputArmed()) check(await looper.toggleInput(), 'MIC could not take an empty slot live');
    check(looper.inputArmed(), 'MIC is not live');
    short = 'MIC';
    source = `MIC (the input dry through an empty live slot ${inputArmed()[0] ? 1 : 2}${unloaded})`;
  }
  liveSlot = inputArmed()[0] ? 0 : inputArmed()[1] ? 1 : null;
  master.setMuted(false);
  master.setVolume(1);
  clock.setClickVolume(CLICK_VOLUME);
  log(`input ${channel + 1} (channel ${channel}): ${source}; master 1, click ${CLICK_VOLUME}; ${bars} bars at ${BPM} BPM; buffers ${buffers.join(',')}`);

  const results: BufferResult[] = [];
  const withEcho = !['', '0'].includes(String(import.meta.env.VITE_LF_PROBE_ECHO ?? '').trim());
  for (const buffer of buffers) results.push(await runBuffer(buffer as BufferFrames, bars, channel, rejected, withEcho));

  if (liveSlot !== null) await stopLive(liveSlot);
  liveSlot = null;
  clock.setMetronome(false);
  looper.clearAll();
  await until('an empty looper at the end', () => allEmpty() && looper.masterLengthFrames() === 0, 5);
  const f = (x: number) => signed(x, 2);
  const summary = results
    .map((r) => {
      const passed = r.bars.filter((b) => b.ok).length;
      const w = (s: TakeStats) => (s.max - s.min).toFixed(2);
      return `b${r.device.block} align ${r.device.alignFrames}/${r.device.inputFrames}: A ${f(r.a.x)} (w ${w(r.a)}, drift ${f(r.a.drift)}) B ${f(r.b.x)} (w ${w(r.b)}) C ${f(r.c.x)} (w ${w(r.c)}) D ${f(r.d.x)} (w ${w(r.d)}) gain ${r.a.gain.toFixed(2)}, bars ${passed}/${r.bars.length}`;
    })
    .join(' | ');
  const failed = results.flatMap((r) => r.bars.filter((b) => !b.ok).map((b) => `b${r.device.block} ${b.name}: ${b.value}`));
  if (failed.length) {
    log(`FAIL ${failed.length} bar(s): ${failed.join('; ')} | ${summary}`);
    return;
  }
  log(`complete: input ${channel + 1}, ${short}, ${bars} bars, ms net, rejected ${rejected.length}; ${summary}`);
}

/**
 * Phase I. Lanes 1–3 hold the grown small loop (`grown` frames; lane 1 A2's clicks on every beat); lane 4
 * is EMPTY; FIXED records `grown` frames. Only lane 1 is heard. FADE is pressed half a second before a
 * loop boundary and lane 4's take armed with it, so the take starts on that boundary: the fade's two bars
 * end on the take's bar 2, whatever the press's exact frame, and beat `k` of the take is at ((8 − k)/8)²
 * of beat 0 under the fade's squared ramp (the ratio of two points on it needs no press frame).
 */
async function fadePhase(grown: number, rate: number): Promise<{ fade: FadeStats; msI: number }> {
  const FADE_BARS = 2;
  const fadeBeats = FADE_BARS * BEATS_PER_BAR;
  looper.setMute(0, false);
  looper.setMute(1, true);
  looper.setMute(2, true);
  clock.setMetronome(false);
  engineFade.setBars(FADE_BARS);
  const volume = looper.trackVolume(0);
  const downbeats: number[] = [];
  const stops: { lane: number; frame: number }[] = [];
  const off = onEngineEvent((ev) => {
    if (ev.type === 'Beat' && ev.beatInBar === 0) downbeats.push(ev.frame);
    if (ev.type === 'Lane' && ev.info.state === 'Stopped') stops.push({ lane: ev.lane, frame: ev.frame });
  });
  try {
    const loopMs = (grown / rate) * 1000;
    // Two bars: two accents.
    const before = await meterPeak(4200);
    // Half a second (±0.1 s) before the next loop boundary, as the heard phase reads it.
    await until('half a second before a loop boundary', () => {
      const left = (1 - looper.phaseValue()) * loopMs;
      return left >= 400 && left <= 600;
    }, loopMs / 1000 + 5);
    const t0 = performance.now();
    const pressedAt = downbeats.length;
    engineFade.fadeAll();
    void looper.recDub(3);
    await until('lanes 1–3 fading', () => [0, 1, 2].every((i) => lane(i).fading), 2);
    const end = lane(0).stopAt!;
    await until('take I to start recording', () => lane(3).state === 'RECORDING' && !lane(3).armed, 3);
    await until('lanes 1–3 STOPPED after the fade', () => [0, 1, 2].every((i) => lane(i).state === 'STOPPED'), 10);
    await until('take I to commit', () => lane(3).state === 'PLAYING', loopMs / 1000 + 5);
    const msI = Math.round(performance.now() - t0);
    const take = await committedPcm(3, grown);
    const lane1 = await committedPcm(0, grown);

    // The fade through the cable: each beat's click over lane 1's own click there, from the take's boundary.
    const beats = Math.round(grown / ((rate * 60) / BPM));
    const got = beatPeaks(take, beats, rate);
    const own = beatPeaks(lane1, beats, rate);
    const raw = got.slice(0, fadeBeats).map((p, k) => p / own[k]);
    const levels = raw.map((l) => l / raw[0]);
    // Past the bar line: the loudest beat window over the bar a click must clear (analyse's floor × FLOOR_FACTOR).
    const floor = median(Array.from({ length: Math.floor(take.length / 64) }, (_, i) => Math.abs(take[i * 64])));
    const after = Math.max(...got.slice(fadeBeats)) / Math.max(floor * FLOOR_FACTOR, 1e-6);
    log(`  take I: fade to frame ${end}, beat levels ${levels.map((l) => l.toFixed(3)).join(' ')}, after the bar line ${after.toFixed(3)} of the floor bar, floor ${floor.toExponential(2)}`);

    // The stop: every fading lane reported STOPPED on the fade's end, a downbeat the fade's bars after the
    // first downbeat at or past the press.
    const laneStops = stops.filter((s) => s.lane <= 2);
    const firstBar = downbeats.slice(pressedAt)[0];
    const fpb = framesPerBar(BPM, rate);
    const onBar = laneStops.length === 3 && laneStops.every((s) => s.frame === end) && downbeats.includes(end) && end === firstBar + FADE_BARS * fpb;
    const stopsText = `end ${end}, stops ${laneStops.map((s) => `${s.lane + 1}@${s.frame}`).join(',')}, downbeats ${downbeats.slice(pressedAt, pressedAt + 4).join(',')}`;

    // PLAY ALL: lane 1 back at its own level, the take on lane 4 muted.
    looper.setMute(3, true);
    looper.playAll();
    await until('lanes 1–3 PLAYING again', () => [0, 1, 2].every((i) => lane(i).state === 'PLAYING'), 5);
    await sleep(300);
    const after2 = await meterPeak(4200);
    const fade: FadeStats = { levels, after, onBar, stops: stopsText, back: after2 / before, volumeKept: looper.trackVolume(0) === volume };
    log(`  take I: meter peak ${before.toFixed(3)} before the fade, ${after2.toFixed(3)} after PLAY ALL; ${stopsText}`);
    return { fade, msI };
  } finally {
    off();
  }
}

async function runBuffer(buffer: BufferFrames, bars: number, channel: number, rejected: string[], withEcho: boolean): Promise<BufferResult> {
  // ── Switch the device, as Audio Settings' buffer picker does ───────────────────────────────────────
  if (engineDevice()?.block !== buffer) {
    const t0 = performance.now();
    await setBufferSize(buffer);
    check((await openEngineDevice())?.block === buffer, `the device did not reopen at ${buffer} frames`);
    log(`  switched to ${buffer} frames in ${Math.round(performance.now() - t0)} ms`);
    await sleep(1500);
  }
  const device = engineDevice()!;
  check(device.backend === 'Asio' && device.block === buffer, `the device runs ${device.backend} at ${device.block} frames`);
  const rate = device.sampleRate;
  const ref = reference(rate);
  log(
    `b${buffer}: ${device.inputName} → ${device.outputName}, ${rate} Hz, block ${device.block}, alignFrames ${device.alignFrames} (${toMs(device.alignFrames, rate).toFixed(2)} ms), inputFrames ${device.inputFrames}; detector lag on the ideal click ${toMs(ref.accent.onset, rate).toFixed(3)} ms accent, ${toMs(ref.plain.onset, rate).toFixed(3)} ms plain`,
  );

  looper.clearAll();
  await until('an empty looper', () => allEmpty() && looper.masterLengthFrames() === 0 && !clock.bpmLocked(), 5);
  clock.setBpm(BPM);
  await until(`${BPM} BPM on the feed`, () => clock.bpm() === BPM, 5);
  looper.setFixedLengthEnabled(true);
  looper.setFixedLengthBars(bars);
  const masterFrames = bars * framesPerBar(BPM, rate);
  const takeSeconds = (masterFrames / rate) * 2 + 15; // a boundary wait plus the take, with room
  const rejectedBefore = rejected.length;
  watchInput();
  try {
    // ── A: the click, a FIXED first take ─────────────────────────────────────────────────────────────
    clock.setMetronome(true);
    if (withEcho) {
      echoSelfTest(rate);
      engineInputSends.setValue('echoTime', 3); // 1/16 (the lane delay's divisions)
      engineInputSends.setValue('echoFeedback', 0);
      engineInputSends.setValue('echoLevel', 0.5);
      engineInputSends.setOn('echo', true);
      await sleep(300);
    }
    const msA = await take(0, 'A', 2 + masterFrames / rate + 15);
    if (withEcho) engineInputSends.setOn('echo', false);
    check(looper.masterLengthFrames() === masterFrames, `the master is ${looper.masterLengthFrames()} frames, expected ${masterFrames}`);
    const pcmA = await committedPcm(0, masterFrames);
    const a = analyse('A', 0, pcmA, rate, ref, channel);
    const echo = withEcho ? echoIn(pcmA, rate) : null;
    if (echo) {
      const range = (xs: number[], digits: number) => (xs.length ? `${signed(Math.min(...xs), digits)}..${signed(Math.max(...xs), digits)}` : 'none');
      log(`  take A echo: ${echo.delays.length}/${echo.clicks} clicks echoed${echo.missing.length ? ` (none after beat(s) ${echo.missing.join(',')})` : ''}, ${range(echo.delays, 3)} ms off a sixteenth, at ${range(echo.ratios, 3)} of its click`);
    }
    const inputPeakA = guard.max;

    // ── B: lane 1's clicks through the cable, click off ──────────────────────────────────────────────
    clock.setMetronome(false);
    const msB = await take(1, 'B', takeSeconds);
    const b = analyse('B', 1, await committedPcm(1, masterFrames), rate, ref, channel);

    // ── STOP ALL → PLAY ALL: the idle transport restarts from the top ────────────────────────────────
    looper.stopAll();
    await until('lanes 1–2 STOPPED', () => lane(0).state === 'STOPPED' && lane(1).state === 'STOPPED', 5);
    await sleep(500);
    looper.playAll();
    await until('lanes 1–2 PLAYING', () => lane(0).state === 'PLAYING' && lane(1).state === 'PLAYING', 5);

    // ── C: the click after the restart, lanes 1–2 muted ──────────────────────────────────────────────
    looper.setMute(0, true);
    looper.setMute(1, true);
    clock.setMetronome(true);
    const msC = await take(2, 'C', takeSeconds);
    const c = analyse('C', 2, await committedPcm(2, masterFrames), rate, ref, channel);

    // ── D: lane 1 through the cable after the restart, lanes 2–3 muted ───────────────────────────────
    clock.setMetronome(false);
    looper.setMute(2, true);
    looper.setMute(0, false);
    const msD = await take(3, 'D', takeSeconds);
    const d = analyse('D', 3, await committedPcm(3, masterFrames), rate, ref, channel);

    // ── E: a multiply, FIXED at two loops, the click alone ───────────────────────────────────────────
    looper.setMute(0, true);
    looper.setMute(3, true);
    clock.setMetronome(true);
    looper.setFixedLengthBars(2 * bars);
    const grown = 2 * masterFrames;
    const msE = await take(4, 'E', 2 * takeSeconds);
    check(looper.masterLengthFrames() === grown, `after the multiply the master is ${looper.masterLengthFrames()} frames, expected ${grown}`);
    const e = analyse('E', 4, await committedPcm(4, grown), rate, ref, channel);
    const tiled = await committedPcm(0, grown);
    let tileDiff = 0;
    for (let k = 0; k < grown; k++) if (tiled[k] !== pcmA[k % masterFrames]) tileDiff++;

    // ── F: lane 4 again over the grown loop, the click alone ─────────────────────────────────────────
    looper.clear(3);
    await until('lane 4 EMPTY', () => lane(3).state === 'EMPTY', 5);
    looper.setMute(4, true);
    const msF = await take(3, 'F', 2 * takeSeconds);
    const f = analyse('F', 3, await committedPcm(3, grown), rate, ref, channel);

    // ── A small loop for G and H: CLEAR ALL, a FIXED first take of half the bars, the click on ─────────
    looper.clearAll();
    await until('an empty looper before G', () => allEmpty() && looper.masterLengthFrames() === 0 && !clock.bpmLocked(), 5);
    clock.setBpm(BPM);
    await until(`${BPM} BPM on the feed`, () => clock.bpm() === BPM, 5);
    const small = Math.max(2, Math.floor(bars / 2));
    const smallFrames = small * framesPerBar(BPM, rate);
    looper.setFixedLengthBars(small);
    const msA2 = await take(0, 'A2', 2 + smallFrames / rate + 15);
    const pcmA2 = await committedPcm(0, smallFrames);

    // ── G: a free take (E10), the click for its first 0.6 loops, REC 1.75 loops in: two loops ──────────
    looper.setMute(0, true);
    looper.setFixedLengthEnabled(false);
    const tG = performance.now();
    void looper.recDub(1);
    await until('take G to start recording', () => lane(1).state === 'RECORDING' && !lane(1).armed, smallFrames / rate + 5);
    const recording = performance.now();
    const loopMs = (smallFrames / rate) * 1000;
    await sleep(0.6 * loopMs);
    clock.setMetronome(false);
    await sleep(recording + 1.75 * loopMs - performance.now());
    check(lane(1).state === 'RECORDING', `take G stopped by itself before its REC press (${lane(1).state})`);
    void looper.recDub(1);
    await until('take G to commit', () => lane(1).state === 'PLAYING', loopMs / 1000 + 5);
    const msG = Math.round(performance.now() - tG);
    const grownG = 2 * smallFrames;
    check(looper.masterLengthFrames() === grownG, `after take G the master is ${looper.masterLengthFrames()} frames, expected ${grownG} (two loops)`);
    const pcmG = await committedPcm(1, grownG);
    // TRIM keeps half the small loop's bars: all inside the clicks' 0.6 loops.
    const keep = Math.max(1, Math.floor(small / 2));
    const g = analyseFirst('G', 1, pcmG, 4 * keep, rate, ref, channel);
    const tileDiffG = tiledDiff(await committedPcm(0, grownG), pcmA2, smallFrames);

    // ── H: TRIM lane 2 to its first bars, then lane 3 takes it through the cable ───────────────────────
    const keepFrames = keep * framesPerBar(BPM, rate);
    trimLane(1, keep);
    await until('lane 2 trimmed', () => lane(1).canUndo, 5);
    const trimDiff = tiledDiff(await committedPcm(1, grownG), pcmG, keepFrames);
    looper.setFixedLengthEnabled(true);
    looper.setFixedLengthBars(2 * small);
    const msH = await take(2, 'H', 2 * (grownG / rate) + 15);
    const h = analyse('H', 2, await committedPcm(2, grownG), rate, ref, channel);
    looper.undoLastOverdub(1);
    await sleep(300);
    const undoDiff = tiledDiff(await committedPcm(1, grownG), pcmG, grownG);
    looper.setFixedLengthBars(bars);

    // ── I: FADE over two bars, lane 1 alone audible, taken through the cable by lane 4 ────────────────
    const { fade, msI } = await fadePhase(grownG, rate);
    const inputPeak = guard.max;
    unwatchInput();

    looper.clearAll();
    await until('an empty looper after the takes', () => allEmpty() && looper.masterLengthFrames() === 0, 5);
    check(rejected.length === rejectedBefore, `take rejected: ${rejected.slice(rejectedBefore).join(', ')}`);
    log(`  b${buffer} takes: A ${msA} ms, B ${msB} ms, C ${msC} ms, D ${msD} ms, E ${msE} ms, F ${msF} ms, A2 ${msA2} ms, G ${msG} ms, H ${msH} ms, I ${msI} ms; input peak ${inputPeakA.toFixed(3)} (A), ${inputPeak.toFixed(3)} (all), ${rejected.length - rejectedBefore} rejected`);

    // ── The bars ─────────────────────────────────────────────────────────────────────────────────────
    const takes = [a, b, c, d, e, f, g, h];
    const spreads = takes.map((s) => s.max - s.min);
    const drifts = takes.map((s) => s.drift);
    const barList: Bar[] = [
      { name: '|clickX_A| <= 2 ms', ok: Math.abs(a.x) <= 2, value: signed(a.x, 3) },
      { name: 'accent on beat 1 A-H', ok: takes.every((s) => s.offBar.length === 0), value: takes.map((s) => s.offBar.length).join(',') },
      { name: 'spread A-H <= 1 ms', ok: spreads.every((s) => s <= 1), value: spreads.map((s) => s.toFixed(3)).join(',') },
      { name: '|drift| A-H <= 0.1 ms/min', ok: drifts.every((s) => Math.abs(s) <= 0.1), value: drifts.map((s) => signed(s, 3)).join(',') },
      // The same path measured twice: only the detector differs, so 0.1 ms (4 frames at 44.1 kHz) is room.
      { name: '|loopX_B - 2*clickX_A| <= 0.1 ms', ok: Math.abs(b.x - 2 * a.x) <= 0.1, value: `${signed(b.x, 3)} - 2*${signed(a.x, 3)} = ${signed(b.x - 2 * a.x, 3)}` },
      { name: '|clickX_C - clickX_A| <= 0.1 ms', ok: Math.abs(c.x - a.x) <= 0.1, value: signed(c.x - a.x, 3) },
      { name: '|loopX_D - loopX_B| <= 0.1 ms', ok: Math.abs(d.x - b.x) <= 0.1, value: signed(d.x - b.x, 3) },
      { name: '|clickX_E - clickX_A| <= 0.1 ms (the multiply take)', ok: Math.abs(e.x - a.x) <= 0.1, value: signed(e.x - a.x, 3) },
      { name: 'lane 1 is its loop twice after the multiply', ok: tileDiff === 0, value: `${tileDiff} of ${grown} frames differ` },
      { name: '|clickX_F - clickX_A| <= 0.1 ms (after the re-anchor)', ok: Math.abs(f.x - a.x) <= 0.1, value: signed(f.x - a.x, 3) },
      { name: '|clickX_G - clickX_A| <= 0.1 ms (the free take, E10)', ok: Math.abs(g.x - a.x) <= 0.1, value: signed(g.x - a.x, 3) },
      { name: 'lane 1 is its loop twice after the free take', ok: tileDiffG === 0, value: `${tileDiffG} of ${grownG} frames differ` },
      { name: `lane 2 is its first ${keep} bar(s) repeated after TRIM`, ok: trimDiff === 0, value: `${trimDiff} of ${grownG} frames differ` },
      { name: 'every beat of H clicks (the trimmed lane through the cable)', ok: h.found === h.beats, value: `${h.found}/${h.beats}` },
      { name: '|loopX_H - loopX_B| <= 0.1 ms (the trimmed lane)', ok: Math.abs(h.x - b.x) <= 0.1, value: signed(h.x - b.x, 3) },
      { name: 'UNDO gives lane 2 back as it was before TRIM', ok: undoDiff === 0, value: `${undoDiff} of ${grownG} frames differ` },
      { name: 'I: the fading lanes stop on the downbeat the fade ends on', ok: fade.onBar, value: fade.stops },
      {
        name: "I: each beat's click through the cable falls, at ((8-k)/8)^2 of the first within 10 % (k <= 5)",
        ok: fade.levels.every((l, k) => k === 0 || l < fade.levels[k - 1]) && fade.levels.slice(0, 6).every((l, k) => Math.abs(l / ((8 - k) / 8) ** 2 - 1) <= 0.1),
        value: fade.levels.map((l, k) => `${l.toFixed(3)}/${(((8 - k) / 8) ** 2).toFixed(3)}`).join(' '),
      },
      { name: 'I: silence from the bar line (no click over the floor bar)', ok: fade.after < 1, value: `${fade.after.toFixed(3)} of the floor bar` },
      { name: 'I: PLAY ALL brings lane 1 back at its level (meter 0.8..1.25, volume kept)', ok: fade.back >= 0.8 && fade.back <= 1.25 && fade.volumeKept, value: `${fade.back.toFixed(3)}, volume ${fade.volumeKept ? 'kept' : 'moved'}` },
      ...(echo ? echoBars(echo, a.found) : []),
    ];
    for (const bar of barList) log(`b${buffer} ${bar.ok ? 'PASS' : 'FAIL'} ${bar.name}: ${bar.value}`);
    return { device, a, b, c, d, e, f, g, h, fade, echo, bars: barList };
  } finally {
    unwatchInput();
    if (withEcho) engineInputSends.setOn('echo', false);
  }
}
