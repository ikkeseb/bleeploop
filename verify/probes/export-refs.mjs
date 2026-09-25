/** Captures the v0.1.0 session files the native engine's Stage 5 must import
 * (`docs/plans/native-engine.md` § Stage 3 and § Stage 5): the export zip exactly as the user downloads
 * it, and the recovery archive exactly as autosave stores it in IndexedDB. A sibling of `tone-refs`
 * rather than part of it: this drives the app's UI, download and IndexedDB in a fresh profile, not Tone
 * OfflineContexts, and its compare has to see past timestamps. The two probes share one budget: their
 * fixtures together stay ≤ 10 MB, and each checks the sum.
 *
 * The session: 48 kHz (the probe pins the live AudioContext's rate), 240 BPM, one bar, two lanes of synthesized PCM loaded through `looper.loadSession` (the
 * import core, so the content is deterministic). Track 1 is muted at volume 0.6 and carries samples
 * past ±1 and a 1e-7 (the editable-headroom cases); track 2 plays at volume 1.2 through the filter and
 * the delay. The export is the Export button's real download; the recovery record is what
 * `autosave.flush()` wrote, read back from IndexedDB.
 *
 * Without `--write` it captures again and compares with the committed files, normalizing what a
 * capture cannot hold still: the timestamped base name, the zip's DOS time, session.json's `exported`
 * and the record's `savedAt`. Stems and session fields must match exactly; the PCM16 master may move
 * by 1 LSB (Blink sums a node's inputs in no fixed order). With `--write` it replaces the fixtures and
 * the manifest.
 * @no-ci capture tool for committed fixtures; CI's Chromium may differ from the capture's
 * Run: pnpm probe export-refs [--write]
 */
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { execSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { flag, probe } from '../harness/probe.ts';
import { parseZip } from '../../src/audio/export/unzip.ts';
import { decodeWav } from '../../src/audio/export/wav.ts';

const write = flag('write');
const FIXTURES = join(import.meta.dirname, '../../src-tauri/crates/lf-engine/tests/fixtures');
const DIR = join(FIXTURES, 'v0.1.0');
const TONE_DIR = join(FIXTURES, 'tone');
const BUDGET_BYTES = 10 * 1024 * 1024;
const BPM = 240;
const BARS = 1;

const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex');
const dirBytes = (dir) => existsSync(dir)
  ? readdirSync(dir).filter((n) => n.endsWith('.wav') || n.endsWith('.zip')).reduce((n, name) => n + statSync(join(dir, name)).size, 0)
  : 0;

// ── The page side ────────────────────────────────────────────────────────────────────────────────
async function buildSession({ bpm, bars }) {
  const lf = window.__lf;
  await lf.autosave.ready();
  await lf.looper.init();
  const { defaultFxStates } = await import('/src/audio/fx/fx.ts');
  const { framesPerBar } = await import('/src/audio/quantize.ts');
  const sr = lf.engine.ctx.sampleRate;
  const frames = bars * framesPerBar(bpm, sr);
  // Track 1: a quiet sine, then samples an editable stem must keep exactly (past ±1, and tiny).
  const one = Float32Array.from({ length: frames }, (_, n) => 0.5 * Math.sin((2 * Math.PI * 220 * n) / sr));
  one.set([1.5, -1.5, 1e-7], 1024);
  // Track 2: a decaying saw pluck on every beat (exact arithmetic).
  const beat = frames / (4 * bars);
  const two = Float32Array.from({ length: frames }, (_, n) => {
    const k = n % beat;
    const phase = ((k * 330) / sr) % 1;
    return 0.8 * (2 * phase - 1) * Math.max(0, 1 - k / (beat / 2));
  });
  const fx = defaultFxStates();
  fx[0] = { bypassed: false, params: { cutoff: 900, q: 3 } };
  fx[3] = { bypassed: false, params: { time: 2, feedback: 0.5, mix: 0.4 } };
  await lf.looper.loadSession({ bpm, bars, masterLengthFrames: frames, tracks: [
    { index: 0, pcm: one, volume: 0.6, muted: true, reversed: false, state: 'PLAYING', fx: defaultFxStates() },
    { index: 1, pcm: two, volume: 1.2, muted: false, reversed: false, state: 'PLAYING', fx },
  ] });
  return { sampleRate: sr, frames };
}

async function readRecovery() {
  const lf = window.__lf;
  await lf.autosave.flush();
  const db = await new Promise((resolve, reject) => {
    const request = indexedDB.open('bleeploop');
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
  const record = await new Promise((resolve, reject) => {
    const request = db.transaction('recovery', 'readonly').objectStore('recovery').get('latest');
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
  db.close();
  const bytes = new Uint8Array(record.bytes);
  let s = '';
  for (let k = 0; k < bytes.length; k += 0x8000) s += String.fromCharCode(...bytes.subarray(k, k + 0x8000));
  return { key: record.key, savedAt: record.savedAt, fields: Object.keys(record), bytes: btoa(s) };
}

// ── Normalized comparison ────────────────────────────────────────────────────────────────────────
/** Entries by name with the timestamped base cut off, so two captures line up, and session.json's
 * text with the base cut out of its file fields too. */
function entriesOf(zip) {
  const entries = parseZip(new Uint8Array(zip.buffer, zip.byteOffset, zip.length));
  const base = entries.find((e) => e.name.endsWith('-session.json')).name.slice(0, -'-session.json'.length);
  const byName = new Map(entries.map((e) => [e.name.slice(base.length), e.data]));
  return { byName, session: new TextDecoder().decode(byName.get('-session.json')).replaceAll(base, '') };
}

/** Differences between two archives, [] when they match up to the normalized fields. */
function compareArchives(label, committed, fresh) {
  const [a, b] = [committed, fresh].map(entriesOf);
  const out = [];
  if ([...a.byName.keys()].join() !== [...b.byName.keys()].join()) {
    return [`${label}: entries ${[...a.byName.keys()]} vs ${[...b.byName.keys()]}`];
  }
  for (const [name, old] of a.byName) {
    const now = b.byName.get(name);
    if (name === '-session.json') {
      const [x, y] = [a, b].map((m) => JSON.stringify({ ...JSON.parse(m.session), exported: null }));
      if (x !== y) out.push(`${label}${name}: session fields differ`);
    } else if (name === '-master.wav') {
      const [x, y] = [old, now].map((d) => decodeWav(d).channels);
      let lsb = 0;
      x.forEach((c, k) => c.forEach((v, i) => { lsb = Math.max(lsb, Math.round(Math.abs(v - y[k][i]) * 32767)); }));
      if (x[0].length !== y[0].length || lsb > 1) out.push(`${label}${name}: master moved ${lsb} LSB`);
    } else if (!Buffer.from(old).equals(Buffer.from(now))) {
      out.push(`${label}${name}: bytes differ`);
    }
  }
  return out;
}

// ── The Node side ────────────────────────────────────────────────────────────────────────────────
await probe(async ({ open, browser }) => {
  // A fresh profile: no earlier recovery record, and downloads land where the probe can read them.
  const context = await browser.newContext({ viewport: { width: 1280, height: 820 }, acceptDownloads: true });
  // The live context runs at the output device's rate; pin 48 k (the engine's rate) so the capture is
  // the same on every machine.
  const init = (page) => page.addInitScript(() => {
    const Native = window.AudioContext;
    window.AudioContext = class extends Native { constructor(options = {}) { super({ ...options, sampleRate: 48000 }); } };
  });
  const { page, consoleErrors } = await open({ context, init });
  const session = await page.evaluate(buildSession, { bpm: BPM, bars: BARS });
  const downloading = page.waitForEvent('download');
  await page.getByRole('button', { name: 'Export loops as a zip of WAV files' }).click();
  const download = await downloading;
  assert.equal(await download.failure(), null);
  const exportName = download.suggestedFilename();
  const exportBytes = readFileSync(await download.path());
  const record = await page.evaluate(readRecovery);
  const recoveryBytes = Buffer.from(record.bytes, 'base64');
  assert.deepEqual(consoleErrors, []);
  await context.close();

  const exported = entriesOf(exportBytes);
  const recovered = entriesOf(recoveryBytes);
  assert.deepEqual([...exported.byName.keys()], ['-track1.wav', '-track2.wav', '-master.wav', '-session.json']);
  assert.deepEqual([...recovered.byName.keys()], ['-track1.wav', '-track2.wav', '-session.json']);
  const master = JSON.parse(exported.session).master;
  assert.equal(master.kind, 'wet-v1', 'the wet master render fell back');

  const files = [
    { file: 'export.zip', bytes: exportBytes,
      what: 'The session export as downloaded: float32 mono stems, a PCM16 stereo wet master, session.json. Store-only zip.',
      how: `The Export button's download (buildExportBundle → makeZip), suggested name ${exportName}.` },
    { file: 'recovery.zip', bytes: recoveryBytes,
      what: 'The recovery archive: float32 mono stems and session.json, no master. Store-only zip.',
      how: `autosave.flush() (recovery-worker → prepareStemArchive float32 → makeZip); the \`bytes\` ArrayBuffer of IndexedDB bleeploop/recovery record { key: "${record.key}", savedAt, bytes }.` },
  ];
  const total = files.reduce((n, f) => n + f.bytes.length, 0);
  const combined = total + dirBytes(TONE_DIR);
  console.log(JSON.stringify({ ...session, export: exportBytes.length, recovery: recoveryBytes.length, record: record.fields }));
  console.log(`v0.1.0 ${(total / 1048576).toFixed(2)} MB, with tone ${(combined / 1048576).toFixed(2)} MB of ${BUDGET_BYTES / 1048576} MB`);
  assert.ok(combined <= BUDGET_BYTES, 'fixtures over budget (tone + v0.1.0)');

  if (write) {
    // Read before writing, so replacing the fixtures does not mark the capture dirty.
    const head = execSync('git rev-parse HEAD', { encoding: 'utf8' }).trim();
    const dirty = execSync('git status --porcelain', { encoding: 'utf8' }).trim() !== '';
    if (existsSync(DIR)) for (const name of readdirSync(DIR)) if (name.endsWith('.zip')) rmSync(join(DIR, name));
    mkdirSync(DIR, { recursive: true });
    for (const f of files) writeFileSync(join(DIR, f.file), f.bytes);
    const manifest = {
      note: 'Written by verify/probes/export-refs.mjs --write. Stage 5 import fixtures: the native engine must import both archives.',
      capture: { commit: head, dirty, chromium: browser.version(), savedAt: record.savedAt },
      session: { bpm: BPM, bars: BARS, ...session,
        tracks: 'track 1: 220 Hz sine at 0.5 with 1.5, -1.5, 1e-7 at frame 1024, muted, volume 0.6, FX default; '
          + 'track 2: saw pluck per beat, volume 1.2, filter (cutoff 900, q 3) and delay (1/8., feedback 0.5, mix 0.4) on' },
      files: files.map((f) => ({
        file: f.file, what: f.what, how: f.how, bytes: f.bytes.length, sha256: sha256(f.bytes),
        entries: parseZip(new Uint8Array(f.bytes.buffer, f.bytes.byteOffset, f.bytes.length))
          .map((e) => ({ name: e.name, bytes: e.data.length, sha256: sha256(e.data) })),
      })),
    };
    writeFileSync(join(DIR, 'manifest.json'), `${JSON.stringify(manifest, null, 1)}\n`);
    console.log(`wrote ${files.length} fixtures to ${DIR}`);
  } else {
    const manifest = JSON.parse(readFileSync(join(DIR, 'manifest.json'), 'utf8'));
    assert.equal(manifest.session.sampleRate, session.sampleRate, 'capture sample rate changed');
    const changed = [];
    for (const f of files) {
      const entry = manifest.files.find((m) => m.file === f.file);
      const committed = readFileSync(join(DIR, f.file));
      assert.equal(sha256(committed), entry.sha256, `${f.file} on disk differs from the manifest`);
      changed.push(...compareArchives(f.file, committed, f.bytes));
    }
    console.log(JSON.stringify({ chromium: browser.version(), captured: manifest.capture.chromium, changed }));
    assert.deepEqual(changed, [], 'captures differ from the committed fixtures');
  }
});
