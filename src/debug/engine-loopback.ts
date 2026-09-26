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
import { engineDevice, onEngineEvent, openEngineDevice, setEngineInputChannel } from '../ui/state/engine-store';

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

/** Every beat's click in a committed lane, against its beat frame. */
function analyse(name: string, laneIndex: number, pcm: Float32Array, rate: number, ref: Reference, channel: number): TakeStats {
  const beat = (rate * 60) / BPM;
  const beats = Math.round(pcm.length / beat);
  const half = Math.round(beat / 2);
  const n = pcm.length;
  // Noise floor from every 64th sample: sorting the whole take would stall the main thread.
  const floor = median(Array.from({ length: Math.floor(n / 64) }, (_, i) => Math.abs(pcm[i * 64])));
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

interface BufferResult {
  device: DeviceStatus;
  a: TakeStats;
  b: TakeStats;
  c: TakeStats;
  d: TakeStats;
  bars: { name: string; ok: boolean; value: string }[];
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
  for (const buffer of buffers) results.push(await runBuffer(buffer as BufferFrames, bars, channel, rejected));

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

async function runBuffer(buffer: BufferFrames, bars: number, channel: number, rejected: string[]): Promise<BufferResult> {
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
    const msA = await take(0, 'A', 2 + masterFrames / rate + 15);
    check(looper.masterLengthFrames() === masterFrames, `the master is ${looper.masterLengthFrames()} frames, expected ${masterFrames}`);
    const a = analyse('A', 0, await committedPcm(0, masterFrames), rate, ref, channel);
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
    const inputPeak = guard.max;
    unwatchInput();

    looper.clearAll();
    await until('an empty looper after the takes', () => allEmpty() && looper.masterLengthFrames() === 0, 5);
    check(rejected.length === rejectedBefore, `take rejected: ${rejected.slice(rejectedBefore).join(', ')}`);
    log(`  b${buffer} takes: A ${msA} ms, B ${msB} ms, C ${msC} ms, D ${msD} ms; input peak ${inputPeakA.toFixed(3)} (A), ${inputPeak.toFixed(3)} (all), ${rejected.length - rejectedBefore} rejected`);

    // ── The bars ─────────────────────────────────────────────────────────────────────────────────────
    const spreads = [a, b, c, d].map((s) => s.max - s.min);
    const drifts = [a, b, c, d].map((s) => s.drift);
    const barList = [
      { name: '|clickX_A| <= 2 ms', ok: Math.abs(a.x) <= 2, value: signed(a.x, 3) },
      { name: 'accent on beat 1 A,B,C,D', ok: [a, b, c, d].every((s) => s.offBar.length === 0), value: [a, b, c, d].map((s) => s.offBar.length).join(',') },
      { name: 'spread A,B,C,D <= 1 ms', ok: spreads.every((s) => s <= 1), value: spreads.map((s) => s.toFixed(3)).join(',') },
      { name: '|drift| A,B,C,D <= 0.1 ms/min', ok: drifts.every((s) => Math.abs(s) <= 0.1), value: drifts.map((s) => signed(s, 3)).join(',') },
      // The same path measured twice: only the detector differs, so 0.1 ms (4 frames at 44.1 kHz) is room.
      { name: '|loopX_B - 2*clickX_A| <= 0.1 ms', ok: Math.abs(b.x - 2 * a.x) <= 0.1, value: `${signed(b.x, 3)} - 2*${signed(a.x, 3)} = ${signed(b.x - 2 * a.x, 3)}` },
      { name: '|clickX_C - clickX_A| <= 0.1 ms', ok: Math.abs(c.x - a.x) <= 0.1, value: signed(c.x - a.x, 3) },
      { name: '|loopX_D - loopX_B| <= 0.1 ms', ok: Math.abs(d.x - b.x) <= 0.1, value: signed(d.x - b.x, 3) },
    ];
    for (const bar of barList) log(`b${buffer} ${bar.ok ? 'PASS' : 'FAIL'} ${bar.name}: ${bar.value}`);
    return { device, a, b, c, d, bars: barList };
  } finally {
    unwatchInput();
  }
}
