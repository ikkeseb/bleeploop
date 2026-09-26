/**
 * DEV probe: the UI on the native engine, in the real app (`docs/plans/native-engine.md` § Stage 5).
 * `pnpm native:engine-smoke` launches the ASIO dev app in its own profile with engine mode on (the
 * runner writes the toggle file there), and this page drives the same facades the lanes and the command
 * bar use, reading back through the feed and the DOM:
 *
 *   device    boot opened the saved ASIO device; the probe switches it to its buffer (default 128)
 *   take A    lane 1, a FIXED 1-bar first take at 120 BPM with the click: the count-in's beats (4-3-2-1,
 *             clicked) on the feed and on screen (the lane's numeral, the beat LEDs), then PLAYING with
 *             a loop of exactly one bar at the device's rate
 *   take B    lane 2, a later take over that master: ARMED, recording, PLAYING at the master's length
 *   dub       an overdub on lane 1 commits (UNDO offered), and UNDO is taken without a refusal
 *   transport STOP ALL stops both lanes, PLAY ALL restarts them, CLEAR and CLEAR ALL empty them
 *   plugin    (`VITE_LF_PROBE_PLUGIN` set) that plugin loads into slot 1, GO LIVE takes it live and the
 *             input meter moves
 *
 * It also reports the state of the AudioContext Tone builds when it loads. It ends with every lane empty,
 * so the runner's window close meets no jam question. `[engine-smoke]` lines go through `console.error`
 * (→ the same log as the Rust host); the runner fails the run on any other `console.error` line.
 *
 * Trigger: `VITE_LF_PROBE=engine-smoke` at Vite start (DEV only). Knobs:
 *   `VITE_LF_PROBE_BUFFER`  the ASIO buffer in frames (default 128)
 *   `VITE_LF_PROBE_PLUGIN`  a plugin name substring to load into slot 1 and go live on (default: none)
 */
import { getContext } from 'tone';
import { framesPerBar } from '../audio/quantize';
import { setBufferSize, usingAsio } from '../audio/audio-devices';
import type { BufferFrames } from '../audio/audio-settings';
import { availablePlugins, nativeHostReady, selectPlugin, slotPlugins } from '../audio/instrument';
import { goLive, inputArmed } from '../audio/native-io';
import { engineMode, type EngineEvent } from '../platform';
import { clock, looper } from '../ui/state/audio';
import { engineDevice, onEngineEvent, openEngineDevice } from '../ui/state/engine-store';

const TAG = '[engine-smoke]';
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

/** The Tone context's state: Tone builds its default AudioContext when its module loads. */
function toneContext(): string {
  const raw = getContext().rawContext as { state?: unknown; baseLatency?: unknown };
  return typeof raw.state === 'string' ? raw.state : 'none (Tone holds no AudioContext)';
}

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

export async function runEngineSmoke(): Promise<void> {
  try {
    await run();
  } catch (e) {
    // Empty the looper first, so the runner's window close meets no jam question.
    looper.clearAll();
    await sleep(1000);
    log(`FAIL ${String(e instanceof Error ? e.message : e)}`);
  }
}

async function run(): Promise<void> {
  check(engineMode(), 'engine mode is off in this profile (the runner writes its toggle file)');
  log(`tone context at start: ${toneContext()}`);
  const events: EngineEvent[] = [];
  onEngineEvent((ev) => events.push(ev));

  // ── Device ──────────────────────────────────────────────────────────────────────────────────────
  await until('the engine device', () => engineDevice() !== null, 90);
  check(usingAsio(), 'ASIO is not in use: this probe runs on the ASIO driver');
  const buffer = Number(import.meta.env.VITE_LF_PROBE_BUFFER ?? 128) as BufferFrames;
  if (engineDevice()?.block !== buffer) {
    await setBufferSize(buffer);
    check((await openEngineDevice())?.block === buffer, `the device did not reopen at ${buffer} frames`);
  }
  const device = engineDevice();
  check(device !== null && device.backend === 'Asio', 'no ASIO device runs');
  const rate = device!.sampleRate;
  log(`device: ${device!.backend} ${device!.inputName} → ${device!.outputName}, ${rate} Hz, ${device!.block} frames, align ${device!.alignFrames} (input ${device!.inputFrames})`);

  // A profile of its own, but an earlier run may have left loops in the engine if it died.
  looper.clearAll();
  await until('an empty looper', () => allEmpty() && looper.masterLengthFrames() === 0 && !clock.bpmLocked(), 5);
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

  // ── STOP ALL, PLAY ALL, CLEAR, CLEAR ALL ────────────────────────────────────────────────────────
  looper.stopAll();
  await until('both lanes STOPPED', () => lane(0).state === 'STOPPED' && lane(1).state === 'STOPPED', 5);
  looper.playAll();
  await until('both lanes PLAYING', () => lane(0).state === 'PLAYING' && lane(1).state === 'PLAYING', 5);
  looper.clear(1);
  await until('lane 2 EMPTY', () => lane(1).state === 'EMPTY', 5);
  log('transport: STOP ALL, PLAY ALL and CLEAR changed the lanes');

  // ── A plugin live (once the engine routes plugins) ──────────────────────────────────────────────
  const want = String(import.meta.env.VITE_LF_PROBE_PLUGIN ?? '').toLowerCase();
  if (want) {
    await until('the plugin scan', () => nativeHostReady() && availablePlugins().length > 0, 300);
    const desc = availablePlugins().find((d) => d.name.toLowerCase().includes(want));
    check(desc !== undefined, `no scanned plugin matches "${want}"`);
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
    log(`plugin: ${desc!.name} live in slot 1, input peak ${Math.min(...levels).toExponential(2)}..${Math.max(...levels).toExponential(2)}`);
    check(moved, 'the input meter did not move');
  }

  looper.clearAll();
  await until('an empty looper at the end', () => allEmpty() && looper.masterLengthFrames() === 0, 5);
  clock.setMetronome(false);
  log(`tone context at the end: ${toneContext()}`);
  log(`complete: ${rate} Hz, ${device!.block} frames, one bar = ${bar} frames`);
}
