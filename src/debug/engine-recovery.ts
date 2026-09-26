/**
 * DEV probe: engine mode's session paths in the real app — export, import and local recovery through
 * the engine's snapshot and session load (`src/ui/state/engine-store.ts` `engineSession`).
 * `pnpm native:engine-recovery` runs the ASIO dev app in engine-smoke's own profile, twice:
 *
 *   save     two lanes recorded from the input (MIC: an empty slot live) with the click on, so the loopback
 *            cable (docs/VERIFY.md) gives them audio; lane 1 at volume 0.7, lane 2 muted; exported to a zip in memory
 *            (stems + wet master + session.json), CLEAR ALL, the zip imported back: both lanes PLAYING
 *            again with the same PCM and mix; then, once autosave has the jam, `saved: …` and the
 *            runner kills app.exe with the loops playing
 *   restore  the relaunch restores the jam from recovery: the same PCM and mix; CLEAR ALL, which
 *            deletes the recovery, so the next run starts clean; the runner closes the window
 *
 * A lane's PCM is compared by its length and the sum of its absolute samples (float32 all the way, so
 * exact); a silent lane fails the run, since silence would round-trip trivially. `[engine-recovery]` lines go through `console.error`; the runner fails the run on any other.
 *
 * Trigger: `VITE_LF_PROBE=engine-recovery` at Vite start (DEV only). Knobs:
 *   `VITE_LF_PROBE_PHASE`   `save` | `restore` (set by the runner)
 *   `VITE_LF_PROBE_EXPECT`  what `save` measured, handed over by the runner (JSON)
 */
import { autosave } from '../audio/autosave';
import { buildExportBundle } from '../audio/export/export';
import { importSession } from '../audio/export/import';
import { parseZip } from '../audio/export/unzip';
import { engineMode } from '../platform';
import { clock, looper, session } from '../ui/state/audio';
import { engineDevice } from '../ui/state/engine-store';

const TAG = '[engine-recovery]';
const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
const log = (...args: unknown[]) => console.error(TAG, ...args);

async function until(label: string, predicate: () => boolean | Promise<boolean>, seconds: number): Promise<void> {
  for (let i = 0; i < seconds * 10; i++) {
    if (await predicate()) return;
    await sleep(100);
  }
  throw Error(`timed out after ${seconds} s waiting for ${label}`);
}

function check(ok: boolean, what: string): void {
  if (!ok) throw Error(what);
}

const lane = (i: number) => looper.track(i)();
const allEmpty = () => Array.from({ length: looper.trackCount }, (_, i) => lane(i).state).every((s) => s === 'EMPTY');

/** What survives a round trip: each lane's length, PCM sum and mix. */
async function measure(): Promise<string> {
  const snap = await session.exportSnapshot();
  return JSON.stringify(
    snap.tracks.map((t) => ({
      lane: t.index + 1,
      frames: t.pcm.length,
      sum: t.pcm.reduce((a, x) => a + Math.abs(x), 0),
      volume: t.volume,
      muted: t.muted,
      reversed: t.reversed,
    })),
  );
}

export async function runEngineRecovery(): Promise<void> {
  try {
    check(engineMode(), 'engine mode is off in this profile (the runner writes its toggle file)');
    await until('the engine device', () => engineDevice() !== null, 90);
    if (import.meta.env.VITE_LF_PROBE_PHASE === 'restore') await restore();
    else await save();
  } catch (e) {
    looper.clearAll();
    await sleep(1000);
    log(`FAIL ${String(e instanceof Error ? e.message : e)}`);
  }
}

async function save(): Promise<void> {
  looper.clearAll();
  await until('an empty looper', () => allEmpty() && !clock.bpmLocked(), 5);
  await autosave.ready();
  clock.setBpm(120);
  await until('120 BPM', () => clock.bpm() === 120, 5);
  clock.setMetronome(true);
  looper.setFixedLengthEnabled(true);
  looper.setFixedLengthBars(1);
  if (!looper.inputArmed()) check(await looper.toggleInput(), 'MIC could not take an empty slot live');
  void looper.recDub(0);
  await until('lane 1 PLAYING', () => lane(0).state === 'PLAYING', 12);
  void looper.recDub(1);
  await until('lane 2 PLAYING', () => lane(1).state === 'PLAYING', 8);
  clock.setMetronome(false);
  if (looper.inputArmed()) await looper.toggleInput();
  looper.setVolume(0, 0.7);
  looper.setMute(1, true);
  const recorded = await measure();
  log(`recorded: ${recorded}`);
  check((JSON.parse(recorded) as { sum: number }[]).every((t) => t.sum > 0), 'a recorded lane is silent: nothing reached the input');

  // Export → CLEAR ALL → import the same zip.
  const bundle = await buildExportBundle({ bpm: 120, bars: 1 }, {}, session);
  check(bundle !== null, 'the export had nothing to export');
  const entries = parseZip(bundle!.zipBytes);
  const sessionJson = JSON.parse(new TextDecoder().decode(entries.find((e) => e.name.endsWith('-session.json'))!.data));
  log(`exported: ${entries.map((e) => e.name.replace(/^bleeploop-[\d-]+-/, '')).join(', ')}; master ${sessionJson.master.kind}`);
  check(sessionJson.tracks.length === 2 && sessionJson.master.kind === 'wet-v1', 'the export lacks a stem or the wet master');
  looper.clearAll();
  await until('an empty looper after CLEAR ALL', () => allEmpty() && looper.masterLengthFrames() === 0, 5);
  await importSession(bundle!.zipBytes, session);
  await until('both lanes PLAYING after the import', () => lane(0).state === 'PLAYING' && lane(1).state === 'PLAYING', 5);
  const imported = await measure();
  log(`imported: ${imported}`);
  check(imported === recorded, 'the imported lanes differ from the recorded ones');

  // Recovery: wait until autosave holds this jam, then the runner kills the app with the loops playing.
  await sleep(3000);
  await until('the recovery save', () => autosave.hasSaved(), 20);
  log(`saved: ${recorded}`);
}

async function restore(): Promise<void> {
  const expected = String(import.meta.env.VITE_LF_PROBE_EXPECT ?? '');
  check(expected !== '', 'no measurement handed over from the save phase');
  await until('the recovered lanes', () => lane(0).state === 'PLAYING' && lane(1).state === 'PLAYING', 30);
  const restored = await measure();
  log(`restored lanes: ${restored}`);
  check(restored === expected, 'the recovered lanes differ from the saved ones');
  looper.clearAll();
  await until('an empty looper', () => allEmpty(), 5);
  await until('the recovery deleted', async () => !(await autosave.hasSaved()), 20);
  log(`restored: 2 lanes, the same PCM and mix after the kill`);
}
