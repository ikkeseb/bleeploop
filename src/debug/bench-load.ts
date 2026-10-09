/**
 * DEV probe: the MIDI latency benchmark's jam load, set up unattended (`docs/plans/native-midi.md`
 * § Measurement; the benchmark: `src-tauri/src/engine_io/midi_bench.rs`). `node scripts/native-probe.mjs
 * bench-load --asio` with `LF_MIDI_BENCH` set launches the ASIO dev app in engine-smoke's profile, and
 * this page sets the load, then holds it while the benchmark sends:
 *
 *   device    the saved ASIO device at buffer 128 (the driver's own rate), as engine-smoke picks it
 *   master    volume 0: the engine still renders and multiplies every sample; only the output is quiet
 *   plugin    the amp-sim `pnpm native:engine` loads (Archetype Petrucci X; Pro-Q if absent) loaded into
 *             slot 1 and live
 *   lanes     three FIXED 1-bar takes at 120 BPM, the click on, all three looping
 *   stage     the stage view open
 *
 * It plays no note and touches no MIDI path: the benchmark's notes reach the app on its default path.
 * Once set it prints `ready at <ISO>`; then it watches the page's long tasks (the benchmark's UI stalls,
 * `src/platform/host.tauri.ts`) and calls the benchmark over once no stall came for `QUIET_MS` after
 * some had, or after `VITE_LF_PROBE_HOLD` seconds (default 900). It ends with the lanes empty, the
 * stage closed and the master level restored, so the runner's window close meets no jam question.
 */
import { setStageOpen } from '../ui/stage/stage-store';
import { availablePlugins, nativeHostReady, selectPlugin, slotPlugins } from '../ui/state/instrument';
import { goLive, inputArmed } from '../ui/state/native-io';
import { platform } from '../platform';
import { clock, looper, master } from '../ui/state/audio';
import { engineDevice } from '../ui/state/engine-store';
import { usingAsio } from '../ui/state/audio-devices';
import type { BufferFrames } from '../ui/state/audio-settings';
import { pickDevice } from './engine-smoke';

const TAG = '[bench-load]';
const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
const log = (...args: unknown[]) => console.error(TAG, ...args);
const AMPS = ['archetype petrucci x', 'pro-q 3'];
/** A main-thread task this long is one of the benchmark's stalls (250 ms by default). */
const STALL_MIN_MS = 150;
/** No stall for this long, after some: the benchmark has ended. */
const QUIET_MS = 15_000;

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

export async function runBenchLoad(): Promise<void> {
  const volume = master.volume();
  let verdict: string;
  try {
    verdict = `complete: ${await run()}`;
  } catch (e) {
    verdict = `FAIL ${String(e instanceof Error ? e.message : e)}`;
  }
  // Empty the looper first, so the runner's window close meets no jam question.
  setStageOpen(false);
  looper.clearAll();
  clock.setMetronome(false);
  master.setVolume(volume);
  await sleep(1000);
  log(verdict);
}

async function run(): Promise<string> {
  check(platform.engine.available, 'this platform has no engine');
  // Count long tasks from the start; only those after `ready` decide the end.
  const stalls: number[] = [];
  new PerformanceObserver((list) => {
    for (const entry of list.getEntries()) if (entry.duration >= STALL_MIN_MS) stalls.push(entry.startTime + entry.duration);
  }).observe({ type: 'longtask', buffered: true });

  // ── Device, master ──────────────────────────────────────────────────────────────────────────────
  await until('the engine device', () => engineDevice() !== null, 90);
  check(usingAsio(), 'ASIO is not in use: this probe runs on the ASIO driver');
  looper.clearAll();
  await until('an empty looper', () => allEmpty() && looper.masterLengthFrames() === 0 && !clock.bpmLocked(), 5);
  await pickDevice(128 as BufferFrames, null);
  const device = engineDevice();
  check(device !== null && device.backend === 'Asio' && device.block === 128, 'no ASIO device runs at 128 frames');
  master.setVolume(0);
  log(`device: ${device!.backend} ${device!.inputName} → ${device!.outputName}, ${device!.sampleRate} Hz, ${device!.block} frames; master volume 0`);

  // ── The amp-sim live in slot 1 ──────────────────────────────────────────────────────────────────
  await until('the plugin scan', () => nativeHostReady() && availablePlugins().length > 0, 300);
  let desc;
  for (const want of AMPS) {
    desc = availablePlugins().find((d) => d.name.toLowerCase().includes(want) && d.format === 'vst3');
    if (desc) break;
  }
  check(desc !== undefined, `no scanned plugin matches ${AMPS.join(' or ')}`);
  await selectPlugin(0, desc!);
  check(slotPlugins()[0]?.id === desc!.id, `could not load ${desc!.name}`);
  await goLive(0);
  check(inputArmed()[0], 'GO LIVE did not take slot 1 live');
  log(`plugin: ${desc!.name} (${desc!.format}) live in slot 1`);

  // ── Three lanes looping ─────────────────────────────────────────────────────────────────────────
  clock.setBpm(120);
  await until('120 BPM on the feed', () => clock.bpm() === 120, 5);
  clock.setMetronome(true);
  looper.setFixedLengthEnabled(true);
  looper.setFixedLengthBars(1);
  for (let i = 0; i < 3; i++) {
    void looper.recDub(i);
    await until(`lane ${i + 1} recording`, () => lane(i).state === 'RECORDING', 5);
    await until(`lane ${i + 1} PLAYING`, () => lane(i).state === 'PLAYING', 15);
  }
  check([0, 1, 2].every((i) => lane(i).state === 'PLAYING' && lane(i).lengthFrames > 0), 'three lanes are not looping');

  // ── Stage view ──────────────────────────────────────────────────────────────────────────────────
  setStageOpen(true);
  await sleep(500);
  check(document.querySelector('canvas') !== null, 'the stage view drew no canvas');
  const readyAt = performance.now();
  log(`ready at ${new Date().toISOString()} (unix ms ${Date.now()}): 3 lanes PLAYING, ${looper.masterLengthFrames()} frames each`);

  // ── Hold until the benchmark's stalls stop ──────────────────────────────────────────────────────
  const hold = Number(import.meta.env.VITE_LF_PROBE_HOLD ?? 900) * 1000;
  let beat = performance.now();
  for (;;) {
    await sleep(500);
    const now = performance.now();
    const after = stalls.filter((t) => t > readyAt);
    const last = after.at(-1);
    check([0, 1, 2].every((i) => lane(i).state === 'PLAYING'), 'a lane stopped looping during the hold');
    if (last !== undefined && now - last > QUIET_MS) {
      return `held ${Math.round((now - readyAt) / 1000)} s after ready, ${after.length} stalls seen, none in the last ${QUIET_MS / 1000} s`;
    }
    if (now - readyAt > hold) throw Error(`held ${hold / 1000} s and saw ${after.length} stalls end`);
    if (now - beat > 30_000) {
      beat = now;
      log(`holding: ${Math.round((now - readyAt) / 1000)} s, ${after.length} stalls since ready`);
    }
  }
}
