/**
 * DEV probe: the UI on the native engine, in the real app (`docs/VERIFY.md`, native:engine-smoke).
 * `pnpm native:engine-smoke` launches the ASIO dev app in its own profile, and this page drives the same facades the lanes and the command
 * bar use, reading back through the feed and the DOM:
 *
 *   device    boot opened the saved ASIO device; the probe switches it to its buffer (default 128) and
 *             its rate pick (default none: the driver's own) through the Audio Settings path, and checks
 *             that the device then runs that rate
 *   take A    lane 1, a FIXED 1-bar first take at 120 BPM with the click: the count-in's beats (4-3-2-1,
 *             clicked) on the feed and on screen (the lane's numeral, the beat LEDs), then PLAYING with
 *             a loop of exactly one bar at the rate that runs
 *   take B    lane 2, a later take over that master: ARMED, recording, PLAYING at the master's length
 *   dub       an overdub on lane 1 commits (UNDO offered), and UNDO is taken without a refusal
 *   dropout   mid-overdub on lane 1, the DEV `holdLock` holds the engine lock 10 ms, so the callbacks miss
 *             their blocks: a real dropout. The layer is kept with one `Dropout` event; the store's marks
 *             are one short run of bins, and the lane's canvas draws the rec-red notch there and nowhere
 *             else. UNDO takes the marks away, REDO brings them back, REVERSE mirrors them through the
 *             loop's frames, and after a WebView reload (sessionStorage carries the run across) the
 *             feed's reset brings them back from the engine
 *   transport STOP ALL stops both lanes, PLAY ALL restarts them, CLEAR and CLEAR ALL empty them
 *   plugin    (`VITE_LF_PROBE_PLUGIN` set) that plugin loads into slot 1, GO LIVE takes it live and the
 *             input meter moves
 *
 * It ends with every lane empty, so the runner's window close meets no jam question. `[engine-smoke]` lines go through `console.error`
 * (→ the same log as the Rust host); the runner fails the run on any other `console.error` line but the
 * one the planted dropout's `Dropout` event logs (`scripts/native-probe.mjs`, `expectedErrors`).
 *
 * Trigger: `VITE_LF_PROBE=engine-smoke` at Vite start (DEV only). Knobs:
 *   `VITE_LF_PROBE_BUFFER`  the ASIO buffer in frames (default 128)
 *   `VITE_LF_PROBE_RATE`    the rate pick, 44100 or 48000 (default: none, the driver's own rate)
 *   `VITE_LF_PROBE_PLUGIN`  `<name substring>[:<format>]` to load into slot 1 and go live on, e.g.
 *                           `Pro-Q:vst3` (default: none)
 */
import { framesPerBar } from '../ui/state/quantize';
import { sampleRatePick, setBufferSize, setSampleRatePick, usingAsio } from '../ui/state/audio-devices';
import { SAMPLE_RATE_OPTIONS, type BufferFrames, type SampleRate } from '../ui/state/audio-settings';
import { availablePlugins, nativeHostReady, selectPlugin, slotPlugins } from '../ui/state/instrument';
import { goLive, inputArmed } from '../ui/state/native-io';
import { platform, type EngineEvent } from '../platform';
import { PEAK_FRAMES, clock, looper, type PeakView } from '../ui/state/audio';
import { engineDevice, onEngineEvent, openEngineDevice } from '../ui/state/engine-store';

const TAG = '[engine-smoke]';
/** What the first document hands the reloaded one (sessionStorage, this launch only). */
const RELOAD_KEY = 'lf.engineSmoke.reload';
interface Reloaded {
  rate: number;
  block: number;
  bar: number;
  /** Lane 1's marks before the reload, reversed. */
  marks: number[];
}
const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
const log = (...args: unknown[]) => console.error(TAG, ...args);

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

/** What the lanes and the command bar show while a take runs, sampled every 20 ms. */
function watchDom(laneIndex: number) {
  const seen = { states: new Set<string>(), counts: new Set<string>(), leds: new Set<number>() };
  const timer = setInterval(() => {
    const el = document.querySelectorAll<HTMLElement>('.lp-lane')[laneIndex];
    if (el?.dataset.state) seen.states.add(el.dataset.state);
    const count = el?.querySelector('.lp-lane__count')?.textContent;
    if (count) seen.counts.add(count);
    document.querySelectorAll('.transport__beat').forEach((led, k) => {
      if (led.classList.contains('on')) seen.leds.add(k);
    });
  }, 20);
  return { seen, stop: () => clearInterval(timer) };
}

const view: PeakView = { min: null, max: null, marks: null, count: 0, version: 0 };

/** Lane `i`'s marked bins in the store, in play order. */
function marked(i: number): number[] {
  looper.peaksInto(i, view);
  const out: number[] = [];
  for (let b = 0; b < view.count; b++) if (view.marks?.[b]) out.push(b);
  return out;
}

const same = (a: number[], b: number[]) => a.length === b.length && a.every((x, k) => x === b[k]);

/** A dropout of 10 ms (a few blocks) marks one short run of bins: at most 4, the loop's end wrapping to its start. */
function shortRun(bins: number[], count: number): boolean {
  if (bins.length === 0 || bins.length > 4) return false;
  const gaps = bins.filter((b, k) => k > 0 && b !== bins[k - 1] + 1).length;
  return gaps === 0 || (gaps === 1 && bins[0] === 0 && bins[bins.length - 1] === count - 1);
}

/** The heard bins a reversal puts buffer bins `bins` of a `frames`-long loop at: through its frames, as
 * the feed maps them (heard frame `frames - 1 - f`). */
function mirrored(bins: number[], frames: number): number[] {
  const out = new Set<number>();
  for (const b of bins) {
    const last = Math.min((b + 1) * PEAK_FRAMES, frames) - 1;
    for (let h = Math.floor((frames - 1 - last) / PEAK_FRAMES); h <= Math.floor((frames - 1 - b * PEAK_FRAMES) / PEAK_FRAMES); h++) out.add(h);
  }
  return [...out].sort((a, b) => a - b);
}

/**
 * Lane `i`'s canvas over five drawn frames: the columns of its top row in the mark colour (the rec-red
 * notch, nearly opaque, as `verify/probes/lane-marks.mjs` reads it) must all sit at a marked bin's
 * frames, `bins` of a `frames`-long loop; with `bins` empty there must be none. A playhead crossing the
 * notch can hide it in one frame, so the columns are the frames' union.
 */
async function canvasMarks(i: number, bins: number[], frames: number): Promise<string> {
  const red = new Set<number>();
  let width = 0;
  for (let n = 0; n < 5; n++) {
    await new Promise((r) => requestAnimationFrame(r));
    const c = document.querySelectorAll('.lp-lane')[i]?.querySelector('canvas');
    check(c instanceof HTMLCanvasElement, `lane ${i + 1} has no canvas`);
    const top = c!.getContext('2d')!.getImageData(0, 0, c!.width, 1).data;
    width = c!.width;
    for (let x = 0; x < width; x++) {
      const [r, g, , a] = top.slice(4 * x, 4 * x + 4);
      if (a > 160 && r > 150 && r - g > 80) red.add(x);
    }
  }
  const at = (b: number) => Math.floor(((b * PEAK_FRAMES) / frames) * width);
  // The notch is 2 device pixels wide (`waveform.ts` `drawMarks`); a column either side for rounding.
  const notch = Math.max(1, Math.round(2 * devicePixelRatio)) + 1;
  const stray = [...red].filter((x) => !bins.some((b) => x >= at(b) - 1 && x <= at(b) + notch));
  const shown = red.size ? `${Math.min(...red)}..${Math.max(...red)}` : 'none';
  const want = bins.map((b) => at(b)).join(',');
  check(stray.length === 0, `lane ${i + 1}'s canvas draws the mark colour at columns ${stray.join(',')}, away from the marked bins (at ${want || 'none'} of ${width})`);
  check(bins.length === 0 || red.size > 0, `lane ${i + 1}'s canvas draws no notch for bins ${bins.join(',')} (at ${want} of ${width})`);
  return `notch columns ${shown} of ${width}`;
}

/** The rate pick `VITE_LF_PROBE_RATE` names (null: none, the driver's own rate). */
export function probeRate(): SampleRate | null {
  const knob = import.meta.env.VITE_LF_PROBE_RATE;
  if (!knob) return null;
  const rate = Number(knob) as SampleRate;
  check(SAMPLE_RATE_OPTIONS.includes(rate), `VITE_LF_PROBE_RATE=${knob}: pick one of ${SAMPLE_RATE_OPTIONS.join(', ')}`);
  return rate;
}

/**
 * Reopen the device at `buffer` and the rate pick `rate` as Audio Settings does (save the picks, open
 * them), unless it runs them already. Resolves whether it reopened.
 */
export async function pickDevice(buffer: BufferFrames, rate: SampleRate | null): Promise<boolean> {
  if (engineDevice()?.block === buffer && sampleRatePick() === rate) return false;
  await setBufferSize(buffer);
  await setSampleRatePick(rate);
  check((await openEngineDevice())?.block === buffer, `the device did not reopen at ${buffer} frames${rate ? ` and ${rate} Hz` : ''}`);
  return true;
}

export async function runEngineSmoke(): Promise<void> {
  let reloaded: Reloaded | null = null;
  try {
    const raw = sessionStorage.getItem(RELOAD_KEY);
    sessionStorage.removeItem(RELOAD_KEY);
    reloaded = raw === null ? null : (JSON.parse(raw) as Reloaded);
  } catch {
    reloaded = null;
  }
  try {
    if (reloaded) await afterReload(reloaded);
    else await run();
  } catch (e) {
    // Empty the looper first, so the runner's window close meets no jam question.
    looper.clearAll();
    await sleep(1000);
    log(`FAIL ${String(e instanceof Error ? e.message : e)}`);
  }
}

async function run(): Promise<void> {
  check(platform.engine.available, 'this platform has no engine');
  const events: EngineEvent[] = [];
  onEngineEvent((ev) => events.push(ev));

  // ── Device ──────────────────────────────────────────────────────────────────────────────────────
  await until('the engine device', () => engineDevice() !== null, 90);
  check(usingAsio(), 'ASIO is not in use: this probe runs on the ASIO driver');
  // A profile of its own, but an earlier run may have left loops in the engine if it died; they would
  // make a rate pick ask first.
  looper.clearAll();
  await until('an empty looper', () => allEmpty() && looper.masterLengthFrames() === 0 && !clock.bpmLocked(), 5);
  const buffer = Number(import.meta.env.VITE_LF_PROBE_BUFFER ?? 128) as BufferFrames;
  const pick = probeRate();
  await pickDevice(buffer, pick);
  const device = engineDevice();
  check(device !== null && device.backend === 'Asio', 'no ASIO device runs');
  const rate = device!.sampleRate;
  check(pick === null || rate === pick, `the device runs at ${rate} Hz, not the ${pick} Hz picked`);
  log(`device: ${device!.backend} ${device!.inputName} → ${device!.outputName}, ${rate} Hz (pick: ${pick ?? "the driver's own"}), ${device!.block} frames, the driver's latency in ${device!.inputFrames} + out ${device!.alignFrames - device!.inputFrames} = align ${device!.alignFrames} frames`);
  clock.setBpm(120);
  await until('120 BPM on the feed', () => clock.bpm() === 120, 5);
  clock.setMetronome(true);
  looper.setFixedLengthEnabled(true);
  looper.setFixedLengthBars(1);
  const bar = framesPerBar(120, rate);

  // ── Take A: the counted first take ──────────────────────────────────────────────────────────────
  events.length = 0;
  const domA = watchDom(0);
  const t0 = performance.now();
  void looper.recDub(0);
  await until('lane 1 ARMED', () => lane(0).state === 'RECORDING' && lane(0).armed, 5);
  await until('lane 1 recording', () => lane(0).state === 'RECORDING' && !lane(0).armed, 5);
  await until('lane 1 PLAYING', () => lane(0).state === 'PLAYING', 10);
  await sleep(300);
  domA.stop();
  const beats = events.filter((e) => e.type === 'Beat');
  const countIn = beats.filter((b) => b.countLeft > 0);
  const counts = countIn.map((b) => b.countLeft);
  log(`take A: ${Math.round(performance.now() - t0)} ms; count-in beats ${counts.join('-')}, clicked ${beats.filter((b) => b.clicked).length}/${beats.length} beats; DOM states ${[...domA.seen.states].join(',')}, numerals ${[...domA.seen.counts].join(',')}, LEDs lit ${[...domA.seen.leds].sort().join(',')}`);
  check(counts.join() === '4,3,2,1', `the count-in counted ${counts.join('-')}`);
  check(countIn.every((b) => b.clicked), 'a count-in beat did not click');
  check(['armed', 'rec', 'play'].every((s) => domA.seen.states.has(s)), `lane 1 showed ${[...domA.seen.states].join(',')}`);
  check(['4', '3', '2', '1'].every((n) => domA.seen.counts.has(n)), `the lane showed numerals ${[...domA.seen.counts].join(',')}`);
  check(domA.seen.leds.size >= 3, `only LEDs ${[...domA.seen.leds].join(',')} lit`);
  check(lane(0).lengthFrames === bar && looper.masterLengthFrames() === bar, `the loop is ${lane(0).lengthFrames} frames, master ${looper.masterLengthFrames()}, one bar is ${bar}`);
  check(clock.bpmLocked(), 'the tempo did not lock');

  // ── Take B: a later take over the master ────────────────────────────────────────────────────────
  const domB = watchDom(1);
  void looper.recDub(1);
  await until('lane 2 ARMED', () => lane(1).state === 'RECORDING' && lane(1).armed, 5);
  await until('lane 2 recording', () => lane(1).state === 'RECORDING' && !lane(1).armed, 5);
  await until('lane 2 PLAYING', () => lane(1).state === 'PLAYING', 6);
  await sleep(300);
  domB.stop();
  check(lane(1).lengthFrames === bar, `lane 2's loop is ${lane(1).lengthFrames} frames`);
  check(domB.seen.states.has('rec') && domB.seen.states.has('play'), `lane 2 showed ${[...domB.seen.states].join(',')}`);
  log(`take B: lane 2 PLAYING, ${lane(1).lengthFrames} frames; DOM states ${[...domB.seen.states].join(',')}`);

  // ── Overdub, then UNDO ──────────────────────────────────────────────────────────────────────────
  void looper.recDub(0);
  await until('lane 1 OVERDUBBING', () => lane(0).state === 'OVERDUBBING', 5);
  await sleep(1000);
  void looper.recDub(0);
  await until('lane 1 back to PLAYING with UNDO', () => lane(0).state === 'PLAYING' && lane(0).canUndo, 5);
  events.length = 0;
  looper.undoLastOverdub(0);
  await sleep(2500); // the swap lands on the next loop boundary
  const refused = events.filter((e) => e.type === 'Refused');
  check(refused.length === 0, `UNDO was refused: ${JSON.stringify(refused)}`);
  check(lane(0).state === 'PLAYING' && lane(0).canUndo, 'lane 1 left PLAYING or lost its REDO after UNDO');
  log('dub: overdub committed, UNDO taken');

  // ── A dropout mid-overdub: kept and marked, through UNDO, REDO, REVERSE and a reload ─────────────
  const hold = platform.engine.holdLock;
  check(hold !== undefined, 'this build has no holdLock (DEV, Tauri only)');
  check(marked(0).length === 0, `lane 1 shows marks before the dropout: ${marked(0).join(',')}`);
  events.length = 0;
  void looper.recDub(0);
  await until('lane 1 OVERDUBBING', () => lane(0).state === 'OVERDUBBING', 5);
  await sleep(500);
  check(await hold!(10), 'holdLock: no device runs');
  await sleep(500);
  void looper.recDub(0);
  await until('lane 1 back to PLAYING with UNDO after the dropout', () => lane(0).state === 'PLAYING' && lane(0).canUndo, 5);
  await until("the layer's Dropout event", () => events.some((e) => e.type === 'Dropout'), 5);
  const dropouts = events.filter((e) => e.type === 'Dropout');
  check(dropouts.length === 1 && dropouts[0].type === 'Dropout' && dropouts[0].lane === 0 && dropouts[0].overdub, `the dropout's events: ${JSON.stringify(dropouts)}`);
  const frames = lane(0).lengthFrames;
  const count = Math.ceil(frames / PEAK_FRAMES);
  await until("the layer's marks in the store", () => marked(0).length > 0, 5);
  const marks = marked(0);
  check(shortRun(marks, count), `lane 1's marks are not one short run: ${marks.join(',')} of ${count} bins`);
  log(`dropout: the layer kept with one Dropout; marks at bins ${marks.join(',')} of ${count}; ${await canvasMarks(0, marks, frames)}`);

  events.length = 0;
  looper.undoLastOverdub(0);
  await until('the marks gone with the layer (UNDO, on the next loop boundary)', () => marked(0).length === 0, 6);
  log(`dropout: UNDO took the marks away; ${await canvasMarks(0, [], frames)}`);
  looper.undoLastOverdub(0);
  await until('the marks back with the layer (REDO)', () => same(marked(0), marks), 6);
  log(`dropout: REDO brought them back; ${await canvasMarks(0, marks, frames)}`);
  const back = mirrored(marks, frames);
  looper.reverse(0);
  await until('lane 1 reversed with its marks mirrored', () => lane(0).reversed && same(marked(0), back), 6);
  log(`dropout: REVERSE put them at bins ${back.join(',')}; ${await canvasMarks(0, back, frames)}`);
  const refusals = events.filter((e) => e.type === 'Refused');
  check(refusals.length === 0, `UNDO, REDO or REVERSE was refused: ${JSON.stringify(refusals)}`);
  const handover: Reloaded = { rate, block: device!.block, bar, marks: back };
  sessionStorage.setItem(RELOAD_KEY, JSON.stringify(handover));
  log('dropout: reloading the WebView; the reset must bring the marks back from the engine');
  location.reload();
}

/** The reloaded document: the marks come back from the engine, then the rest of the run. */
async function afterReload(saved: Reloaded): Promise<void> {
  check(platform.engine.available, 'this platform has no engine');
  await until('the engine device after the reload', () => engineDevice() !== null, 60);
  await until('lane 1 PLAYING reversed after the reload', () => lane(0).state === 'PLAYING' && lane(0).reversed, 30);
  await until('the marks back from the engine after the reload', () => same(marked(0), saved.marks), 10);
  log(`dropout: after the reload the marks are back at bins ${saved.marks.join(',')}; ${await canvasMarks(0, saved.marks, lane(0).lengthFrames)}`);
  await rest(saved.rate, saved.block, saved.bar);
}

/** STOP ALL to the end. */
async function rest(rate: number, block: number, bar: number): Promise<void> {
  // ── STOP ALL, PLAY ALL, CLEAR, CLEAR ALL ────────────────────────────────────────────────────────
  looper.stopAll();
  await until('both lanes STOPPED', () => lane(0).state === 'STOPPED' && lane(1).state === 'STOPPED', 5);
  looper.playAll();
  await until('both lanes PLAYING', () => lane(0).state === 'PLAYING' && lane(1).state === 'PLAYING', 5);
  looper.clear(1);
  await until('lane 2 EMPTY', () => lane(1).state === 'EMPTY', 5);
  log('transport: STOP ALL, PLAY ALL and CLEAR changed the lanes');

  // ── A plugin live (once the engine routes plugins) ──────────────────────────────────────────────
  const [want, format] = String(import.meta.env.VITE_LF_PROBE_PLUGIN ?? '').toLowerCase().split(':');
  if (want) {
    await until('the plugin scan', () => nativeHostReady() && availablePlugins().length > 0, 300);
    const desc = availablePlugins().find((d) => d.name.toLowerCase().includes(want) && (!format || d.format === format));
    check(desc !== undefined, `no scanned plugin matches "${want}${format ? `:${format}` : ''}"`);
    await selectPlugin(0, desc!);
    check(slotPlugins()[0]?.id === desc!.id, `could not load ${desc!.name}`);
    await goLive(0);
    check(inputArmed()[0], 'GO LIVE did not take slot 1 live');
    const levels: number[] = [];
    for (let i = 0; i < 50; i++) {
      levels.push(looper.levelValue());
      await sleep(40);
    }
    const moved = new Set(levels.map((l) => l.toFixed(6))).size > 1;
    log(`plugin: ${desc!.name} (${desc!.format}) live in slot 1, input peak ${Math.min(...levels).toExponential(2)}..${Math.max(...levels).toExponential(2)}`);
    check(moved, 'the input meter did not move');
  }

  looper.clearAll();
  await until('an empty looper at the end', () => allEmpty() && looper.masterLengthFrames() === 0, 5);
  clock.setMetronome(false);
  log(`complete: ${rate} Hz, ${block} frames, one bar = ${bar} frames`);
}
