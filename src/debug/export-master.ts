/**
 * DEV probe: the export's wet master on the engine, in the real app (`docs/VERIFY.md`,
 * native:export-master). `pnpm native:export-master` launches the WASAPI dev app once, in a profile of
 * its own (`scripts/export-probe.tauri.json`):
 *
 *   output  the WASAPI output moves to the endpoint named like "CABLE Input" (VB-Audio Virtual Cable)
 *           when there is one, so the room hears nothing; without one, the master volume goes to 0.1
 *           (the render keeps it: the master stays audible in the file, quietly in the room)
 *   take    the built-in Lead on slot A, a FIXED 1-bar first take at 120 BPM on lane 1, the click off;
 *           one note (MIDI velocity 100) played through the input router while the take records, so the
 *           record tap takes the synth
 *   export  `buildExportBundle` (the Export button's bundle): session.json's `master.kind` is
 *           'wet-engine', the master WAV is stereo and one loop long, not silent, and its onset (the
 *           first sample past a tenth of its peak) lies within 2 frames of the stem's
 *
 * It ends with every lane empty (CLEAR ALL), so the runner's window close meets no jam question. It
 * cannot hear the master: how it sounds against the live mix is the owner's ear (`STATUS.md`).
 * `[export-master]` lines go through `console.error`; the runner fails the run on any other.
 *
 * Trigger: `VITE_LF_PROBE=export-master` at Vite start (DEV only). No knobs.
 */
import { buildExportBundle } from '../session/export';
import { parseZip } from '../session/unzip';
import { decodeWav } from '../session/wav';
import { framesPerBar } from '../ui/state/quantize';
import { openEngineDevice, engineDevice } from '../ui/state/engine-store';
import { outputDevices, refreshOutputDevices } from '../ui/state/audio-devices';
import { writeAudioDeviceSettings } from '../ui/state/audio-settings';
import { selectSynth } from '../ui/state/instrument';
import { inputRouter } from '../ui/state/input-router';
import { clock, looper, master, session } from '../ui/state/audio';

const TAG = '[export-master]';
const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
const log = (...args: unknown[]) => console.error(TAG, ...args);
/** The onset is the first sample past this share of the signal's peak. */
const ONSET = 0.1;
/** How far apart the master's and the stem's onsets may lie, in frames. */
const ALIGN_FRAMES = 2;

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
const peak = (x: Float32Array) => x.reduce((m, v) => Math.max(m, Math.abs(v)), 0);
const onset = (x: Float32Array) => {
  const level = ONSET * peak(x);
  return x.findIndex((v) => Math.abs(v) > level);
};

export async function runExportMaster(): Promise<void> {
  try {
    await run();
  } catch (e) {
    // Empty the looper first, so the runner's window close meets no jam question.
    looper.clearAll();
    log(`FAIL ${e instanceof Error ? e.message : String(e)}`);
  }
}

async function run(): Promise<void> {
  // ── Output: the virtual cable when there is one ─────────────────────────────────────────────────
  await until('the engine device', () => engineDevice() !== null, 90);
  check(engineDevice()!.backend === 'Wasapi', `the device is ${engineDevice()!.backend}: this probe runs on WASAPI`);
  check(await refreshOutputDevices(), 'the outputs could not be listed');
  const cable = outputDevices().find((d) => /^CABLE Input\b/i.test(d.name));
  if (cable) {
    writeAudioDeviceSettings({ outputDeviceId: cable.id });
    const status = await openEngineDevice();
    check(status !== null && status.outputName === cable.name, `the output did not move to ${cable.name}`);
  } else {
    master.setVolume(0.1);
  }
  master.setMuted(false);
  const device = engineDevice()!;
  const rate = device.sampleRate;
  log(`device: ${device.backend} ${device.inputName} → ${device.outputName}, ${rate} Hz, ${device.block} frames; ${cable ? 'the virtual cable' : 'no virtual cable: master volume 0.1'}`);

  // ── The take: one Lead note into a FIXED 1-bar first take ───────────────────────────────────────
  // A profile of its own, but an earlier run may have left loops in the engine if it died.
  looper.clearAll();
  await until('an empty looper', () => allEmpty() && looper.masterLengthFrames() === 0 && !clock.bpmLocked(), 5);
  selectSynth(0, 'lead');
  clock.setMetronome(false);
  clock.setBpm(120);
  await until('120 BPM on the feed', () => clock.bpm() === 120, 5);
  looper.setFixedLengthEnabled(true);
  looper.setFixedLengthBars(1);
  const bar = framesPerBar(120, rate);
  void looper.recDub(0);
  await until('lane 1 recording', () => lane(0).state === 'RECORDING' && !lane(0).armed, 10);
  await sleep(400);
  inputRouter.handle({ type: 'on', note: 57, velocity: 100, source: 'computer' });
  await sleep(250);
  inputRouter.handle({ type: 'off', note: 57, velocity: 0, source: 'computer' });
  await until('lane 1 PLAYING', () => lane(0).state === 'PLAYING', 10);
  check(lane(0).lengthFrames === bar, `the loop is ${lane(0).lengthFrames} frames, one bar is ${bar}`);

  // ── The export: the engine's master beside the stem ─────────────────────────────────────────────
  const t0 = performance.now();
  const bundle = await buildExportBundle({ bpm: 120, bars: 1 }, {}, session);
  const ms = Math.round(performance.now() - t0);
  check(bundle !== null, 'the export had nothing to export');
  const entries = parseZip(bundle!.zipBytes);
  const json = JSON.parse(new TextDecoder().decode(entries.find((e) => e.name.endsWith('-session.json'))!.data)) as {
    masterLengthFrames: number;
    master: { file: string; kind: string; level: number };
    tracks: { file: string }[];
  };
  const stem = decodeWav(entries.find((e) => e.name === json.tracks[0].file)!.data).channels[0];
  const wav = decodeWav(entries.find((e) => e.name === json.master.file)!.data);
  const [left, right] = wav.channels;
  const at = { stem: onset(stem), left: onset(left), right: onset(right) };
  const peaks = { stem: peak(stem), left: peak(left), right: peak(right) };
  log(
    `export: ${ms} ms; master ${json.master.kind}, level ${json.master.level}, ${wav.channels.length} channels × ${left.length} frames at ${wav.sampleRate} Hz; ` +
      `peaks stem ${peaks.stem.toFixed(4)}, master ${peaks.left.toFixed(4)}/${peaks.right.toFixed(4)}; onsets stem ${at.stem}, master ${at.left}/${at.right}`,
  );
  check(json.master.kind === 'wet-engine', `the master is ${json.master.kind}, not the engine's`);
  check(wav.channels.length === 2 && left.length === json.masterLengthFrames && right.length === json.masterLengthFrames, 'the master is not stereo and one loop long');
  check(json.masterLengthFrames === bar && wav.sampleRate === rate, `the master is ${json.masterLengthFrames} frames at ${wav.sampleRate} Hz`);
  check(peaks.stem > 0.01, 'the stem is silent: the note never reached the record tap');
  check(peaks.left > 1e-3 && peaks.right > 1e-3, 'the master is silent');
  check(at.stem > 0, `the stem's onset is at ${at.stem}`);
  const off = Math.max(Math.abs(at.left - at.stem), Math.abs(at.right - at.stem));
  check(off <= ALIGN_FRAMES, `the master's onset lies ${off} frames from the stem's`);

  looper.clearAll();
  await until('an empty looper at the end', () => allEmpty() && looper.masterLengthFrames() === 0, 5);
  log(`complete: wet-engine, ${left.length} frames, onset ${at.stem} within ${off} frame(s), ${ms} ms`);
}
