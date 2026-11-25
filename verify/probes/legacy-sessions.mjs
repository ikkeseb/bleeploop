/**
 * Older session files, unchanged, into today's web engine fake (`src/platform/host.web.ts`): each must reach
 * the engine as one load whose header carries its grid, mix and one channel per track (the engine plays a
 * mono stem on both sides: `docs/unit-contract.md` §8), and whose PCM is each stem's float32 samples
 * exactly, read here from the archive's bytes without the product's zip or WAV code. Today's export and
 * import changing together cannot hide a break with these files.
 *
 * - v0.1.0 (`src-tauri/crates/lf-engine/tests/fixtures/v0.1.0`, their bytes pinned by
 *   `src-tauri/crates/lf-engine/tests/v0_1_0_exports.rs`): the downloaded export (stems, a PCM16 master,
 *   session.json) and the recovery archive through the IMPORT button's file input, at 48 kHz. The grid is
 *   240 BPM, one bar, 48000 frames, both lanes playing forwards, the mix as saved (track 1 muted at 0.6,
 *   track 2 at 1.2 with its filter and delay on; DUB FEEDBACK, which the format did not hold yet, at 100 %,
 *   and no pan).
 * - v0.8.9 (`verify/fixtures/v0.8.9`, captured from that release's own recovery path, see below): a v1
 *   recovery (4/4) and a v2 one (6/8, the meter in its header) at 48 and at 44.1 kHz, each seeded into
 *   IndexedDB as v0.8.9 stored it (`bleeploop`/`recovery`, record `latest` with its rate) and restored by
 *   the boot itself (`autosave.start`'s one restore, unit contract §6 "the autosaved jam") into a fake
 *   device at the jam's rate. Then the feed shows the lane playing and the engine's snapshot holds the
 *   stem on both channels, as the native engine plays a mono stem; the next save (`autosave.flush`) writes
 *   `formatVersion: 3` with a two-channel stem whose channels are both the v0.8.9 stem's samples exactly,
 *   the grid and the meter kept.
 *
 * Capture (`--write`, against a dev server of a v0.8.9 checkout, never this tree): each v0.8.9 case's jam
 * is loaded through that build's `session.loadSession` (one lane, a 0.5 sine with 1.5, -1.5 and 1e-7
 * planted at frame 1024, a non-default mix), its fake's snapshot scripted to the same PCM, the lane shown
 * playing, then `autosave.flush()` (its recovery worker and zip) and the record read back from IndexedDB;
 * the manifest names the checkout's commit. Run: `git worktree add <dir> v0.8.9` outside this repo,
 * `pnpm install` and `pnpm exec vite --port <p> --strictPort` there, then from here
 * `node verify/probes/legacy-sessions.mjs --url=http://localhost:<p> --write --checkout=<dir>`. Without
 * `--write` the fixtures are read only, and their bytes must match the manifest.
 *
 * Cannot see the native engine's load of those bytes (`src-tauri/crates/lf-engine/tests/session.rs`),
 * Tauri's IPC, WebView2 or a real v0.8.9 profile's IndexedDB (rehearsed at the release).
 * Run: pnpm probe legacy-sessions
 */
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { execSync } from 'node:child_process';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { arg, flag, probe } from '../harness/probe.ts';

const V010 = 'src-tauri/crates/lf-engine/tests/fixtures/v0.1.0';
const V089 = join(import.meta.dirname, '../fixtures/v0.8.9');
/** v0.8.9's release commit (`git rev-parse v0.8.9^{commit}`): a capture from any other tree is refused. */
const V089_COMMIT = '457447d';
const write = flag('write');
const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex');

// ── The archive's bytes, read without the product's code ────────────────────────────────────────────

/** The entries of a store-only zip, from its local headers: name → bytes. */
function zipEntries(zip) {
  const out = new Map();
  for (let at = 0; zip.readUInt32LE(at) === 0x04034b50; ) {
    assert.equal(zip.readUInt16LE(at + 8), 0, 'a store-only entry');
    const size = zip.readUInt32LE(at + 18);
    const nameLen = zip.readUInt16LE(at + 26);
    const data = at + 30 + nameLen + zip.readUInt16LE(at + 28);
    out.set(zip.toString('utf8', at + 30, at + 30 + nameLen), zip.subarray(data, data + size));
    at = data + size;
  }
  return out;
}

/** A float32 WAV's channels, from its `fmt ` and `data` chunks (interleaved frames). */
function wavChannels(wav) {
  let fmt = null;
  for (let c = 12; c + 8 <= wav.length; c += 8 + wav.readUInt32LE(c + 4) + (wav.readUInt32LE(c + 4) & 1)) {
    const id = wav.toString('latin1', c, c + 4);
    if (id === 'fmt ') fmt = { format: wav.readUInt16LE(c + 8), channels: wav.readUInt16LE(c + 10), rate: wav.readUInt32LE(c + 12), bits: wav.readUInt16LE(c + 22) };
    if (id !== 'data') continue;
    assert.ok(fmt && fmt.bits === 32 && (fmt.format === 3 || fmt.format === 0xfffe), `a float32 WAV: ${JSON.stringify(fmt)}`);
    const n = wav.readUInt32LE(c + 4) / 4 / fmt.channels;
    const channels = Array.from({ length: fmt.channels }, (_, ch) => Array.from({ length: n }, (_, i) => wav.readFloatLE(c + 8 + 4 * (i * fmt.channels + ch))));
    return { rate: fmt.rate, channels };
  }
  throw new Error('no data chunk');
}

/** The session.json and each track's stem of a session archive. */
function readArchive(zip) {
  const entries = zipEntries(zip);
  const json = [...entries.keys()].find((n) => n.endsWith('-session.json'));
  assert.ok(json, `a session.json among ${[...entries.keys()]}`);
  const session = JSON.parse(entries.get(json).toString('utf8'));
  return { names: [...entries.keys()], session, stems: session.tracks.map((t) => wavChannels(entries.get(t.file))) };
}

const firstDiff = (got, want) => (got.length !== want.length ? `length ${got.length} vs ${want.length}` : got.findIndex((x, i) => !Object.is(x, want[i])));

// ── v0.8.9's recoveries ─────────────────────────────────────────────────────────────────────────────

const FOUR_FOUR = { num: 4, den: 4, beats: 4 };
const SIX_EIGHT = { num: 6, den: 8, beats: 2 };
/** One bar at 300 BPM keeps the fixtures small (0.8 s in 4/4, 0.4 s in 6/8); a lane other than 1 in
 * three of them, so the track's index travels too. */
const CASES = [
  { file: 'recovery-v1-48000.zip', formatVersion: 1, rate: 48000, bpm: 300, meter: FOUR_FOUR, lane: 0, freq: 220 },
  { file: 'recovery-v1-44100.zip', formatVersion: 1, rate: 44100, bpm: 300, meter: FOUR_FOUR, lane: 1, freq: 330 },
  { file: 'recovery-v2-48000.zip', formatVersion: 2, rate: 48000, bpm: 300, meter: SIX_EIGHT, lane: 2, freq: 440 },
  { file: 'recovery-v2-44100.zip', formatVersion: 2, rate: 44100, bpm: 300, meter: SIX_EIGHT, lane: 4, freq: 550 },
];
/** The mix each capture saves: none of it a default. */
const MIX = { volume: 0.7, muted: false, dubFeedback: 0.6, pan: -0.25, filter: { cutoff: 900, q: 3 } };

/** In a v0.8.9 page: load the case's jam, script the snapshot, return its frame count. */
async function captureJam({ rate, bpm, meter, lane, freq, mix }) {
  const lf = window.__lf;
  await lf.autosave.ready();
  const { session } = await import('/src/ui/state/audio.ts');
  const { defaultFxStates } = await import('/src/ui/state/fx-metadata.ts');
  const { framesPerBar } = await import('/src/ui/state/quantize.ts');
  const { encodeSessionBytes, TIME_SIGNATURES } = await import('/src/platform/engine-wire.ts');
  const timeSig = TIME_SIGNATURES.find((m) => m.num === meter.num && m.den === meter.den);
  const frames = framesPerBar(bpm, rate, timeSig.beats);
  const pcm = Float32Array.from({ length: frames }, (_, n) => 0.5 * Math.sin((2 * Math.PI * freq * n) / rate));
  pcm.set([1.5, -1.5, 1e-7], 1024);
  const fx = defaultFxStates();
  fx[0] = { bypassed: false, params: mix.filter };
  await session.loadSession({ bpm, meter: timeSig, bars: 1, masterLengthFrames: frames, tracks: [
    { index: lane, pcm, volume: mix.volume, muted: mix.muted, reversed: false, state: 'PLAYING', fx, dubFeedback: mix.dubFeedback, pan: mix.pan },
  ] });
  lf.native.snapshotBytes = encodeSessionBytes({ rate, masterLengthFrames: frames, bpm, meter: timeSig,
    tracks: [{ index: lane, frames, reversed: false, state: 'Playing' }] }, [pcm]).buffer;
  return frames;
}

/** The IndexedDB record `latest`, after a forced save when `flush`; its bytes base64. */
async function readLatest(flush) {
  if (flush) await window.__lf.autosave.flush();
  const db = await new Promise((resolve, reject) => {
    const r = indexedDB.open('bleeploop');
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
  });
  const record = await new Promise((resolve, reject) => {
    const r = db.transaction('recovery', 'readonly').objectStore('recovery').get('latest');
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
  });
  db.close();
  if (!record) return null;
  const bytes = new Uint8Array(record.bytes);
  let s = '';
  for (let k = 0; k < bytes.length; k += 0x8000) s += String.fromCharCode(...bytes.subarray(k, k + 0x8000));
  const { bytes: _, ...fields } = record;
  return { fields, bytes: btoa(s) };
}

/** Store `record` (its `bytes` base64) as IndexedDB `bleeploop`/`recovery` holds it, store and all. */
async function seedRecord(record) {
  const db = await new Promise((resolve, reject) => {
    const r = indexedDB.open('bleeploop', 1);
    r.onupgradeneeded = () => {
      if (!r.result.objectStoreNames.contains('recovery')) r.result.createObjectStore('recovery', { keyPath: 'key' });
    };
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
  });
  const bin = atob(record.bytes);
  const bytes = Uint8Array.from({ length: bin.length }, (_, i) => bin.charCodeAt(i));
  await new Promise((resolve, reject) => {
    const tx = db.transaction('recovery', 'readwrite');
    tx.objectStore('recovery').put({ ...record.fields, bytes: bytes.buffer });
    tx.oncomplete = resolve;
    tx.onerror = () => reject(tx.error);
  });
  db.close();
}

await probe(async ({ browser, open }) => {
  const lane = (state, length) => ({ state, length, armed: false, autoArmed: false, canUndo: false,
    canReverse: state === 'Playing', reversed: false, stopAt: null, fading: false, retakePass: 0 });
  /** The feed's frame for a jam of `frames` (0: empty) on `playing`'s lane. */
  const jamFrame = (seq, { rate, bpm, meter }, frames, playing, reset = false) => ({
    seq, reset, catchUp: false, device: [], peaks: [], scope: null, tuner: null,
    ...(reset ? { settings: [], anchor: { frame: 0, atMs: Date.now(), rate, grid: 0 } } : { anchor: null }),
    meter: { peak: [0, 0], clip: [false, false] },
    events: [
      { Transport: { frame: 0, master: frames, bpm, meter, locked: frames > 0 } },
      ...[0, 1, 2, 3, 4].map((i) => ({ Lane: { frame: 0, lane: i, info: i === playing && frames > 0 ? lane('Playing', frames) : lane('Empty', 0) } })),
      ...(reset ? [{ Selected: { frame: 0, lane: 0 } }] : []),
    ],
  });

  if (write) {
    // ── Capture from a v0.8.9 dev server ──
    const checkout = arg('checkout');
    assert.ok(checkout, '--write needs --checkout=<the v0.8.9 checkout the server at --url runs>');
    const head = execSync('git rev-parse HEAD', { cwd: checkout, encoding: 'utf8' }).trim();
    assert.ok(head.startsWith(V089_COMMIT), `the checkout is at ${head}, not v0.8.9 (${V089_COMMIT})`);
    const dirty = execSync('git status --porcelain --untracked-files=no', { cwd: checkout, encoding: 'utf8' }).trim() !== '';
    mkdirSync(V089, { recursive: true });
    const files = [];
    for (const c of CASES) {
      const context = await browser.newContext({ viewport: { width: 1280, height: 820 } });
      const { page } = await open({ context, init: (p) => p.addInitScript((rate) => Object.assign(window, { __lfEngineFake: true, __lfEngineFakeRate: rate }), c.rate) });
      await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 10000 });
      await page.evaluate((f) => window.__lf.native.emit(f), jamFrame(1, c, 0, -1, true));
      const frames = await page.evaluate(captureJam, { ...c, mix: MIX });
      await page.evaluate((f) => window.__lf.native.emit(f), jamFrame(2, c, frames, c.lane));
      const record = await page.evaluate(readLatest, true);
      await context.close();
      const bytes = Buffer.from(record.bytes, 'base64');
      const { session } = readArchive(bytes);
      assert.equal(session.formatVersion, c.formatVersion, `${c.file}: v0.8.9 wrote formatVersion ${session.formatVersion}`);
      assert.equal(record.fields.rate, c.rate, `${c.file}: the record's rate`);
      writeFileSync(join(V089, c.file), bytes);
      files.push({ file: c.file, record: record.fields, frames, bytes: bytes.length, sha256: sha256(bytes) });
      console.log(c.file, JSON.stringify(record.fields), frames, bytes.length);
    }
    const manifest = {
      note: 'Written by verify/probes/legacy-sessions.mjs --write against a v0.8.9 checkout\'s dev server. The recovery archives v0.8.9 stores in IndexedDB (bleeploop/recovery, record `latest`); `record` is that record\'s other fields, `bytes` its ArrayBuffer.',
      capture: { commit: head, dirty, chromium: browser.version() },
      how: 'session.loadSession (one lane, 1 bar), the fake\'s snapshot scripted to the same PCM, the lane shown playing, then autosave.flush(): its recovery worker, prepareStemArchive float32, makeZip.',
      session: { bars: 1, pcm: '0.5 sine at `freq` Hz, with 1.5, -1.5 and 1e-7 at frame 1024', mix: MIX },
      files: files.map((f) => ({ ...CASES.find((c) => c.file === f.file), ...f })),
    };
    writeFileSync(join(V089, 'manifest.json'), `${JSON.stringify(manifest, null, 1)}\n`);
    console.log(`wrote ${files.length} fixtures to ${V089}`);
    return;
  }

  // ── v0.1.0: the export and the recovery through IMPORT ──
  const V010_FRAMES = 48000;
  const fx = (filter, delay) => [
    { bypassed: !filter, params: filter ? { cutoff: 900, q: 3 } : { cutoff: 1200, q: 2 } },
    { bypassed: true, params: { semitones: 0 } },
    { bypassed: true, params: { rate: 1 } },
    { bypassed: !delay, params: delay ? { time: 2, feedback: 0.5, mix: 0.4 } : { time: 1, feedback: 0.4, mix: 0.3 } },
    { bypassed: true, params: { amount: 0.3 } },
  ];
  const V010_HEADER = {
    bpm: 240,
    bars: 1,
    masterLengthFrames: V010_FRAMES,
    tracks: [
      { index: 0, frames: V010_FRAMES, channels: 1, reversed: false, state: 'Playing', mix: { volume: 0.6, muted: true, dubFeedback: 1, fx: fx(false, false) } },
      { index: 1, frames: V010_FRAMES, channels: 1, reversed: false, state: 'Playing', mix: { volume: 1.2, muted: false, dubFeedback: 1, fx: fx(true, true) } },
    ],
  };
  const loadedSession = (page) => page.evaluate(async () => {
    const { splitSessionBytes } = await import('/src/platform/engine-wire.ts');
    const { header, pcm } = splitSessionBytes(window.__lf.native.loadedSessions[0].slice().buffer);
    return { header, pcm: pcm.map((block) => Array.from(block)) };
  });
  for (const file of ['export.zip', 'recovery.zip']) {
    const zip = readFileSync(`${V010}/${file}`);
    const { page } = await open({ init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)) });
    await page.waitForFunction(() => window.__lf.native.opened.length >= 1, undefined, { timeout: 10000 });
    await page.locator('input[type="file"]').setInputFiles({ name: file, mimeType: 'application/zip', buffer: zip });
    await page.waitForFunction(() => window.__lf.native.loadedSessions.length === 1, undefined, { timeout: 10000 });
    const loaded = await loadedSession(page);
    console.log(file, JSON.stringify(loaded.header));
    assert.deepEqual(loaded.header, V010_HEADER, `${file}: the load's header`);
    assert.equal(loaded.pcm.length, 2, `${file}: two lanes of PCM`);
    const { stems } = readArchive(zip);
    for (const i of [0, 1]) {
      const want = stems[i].channels[0];
      assert.equal(want.length, V010_FRAMES);
      const off = firstDiff(loaded.pcm[i], want);
      assert.equal(off, -1, `${file}: lane ${i + 1}'s PCM is its stem's samples (first difference: ${off})`);
    }
    // Track 1 as captured: a 220 Hz sine at 0.5, with 1.5, -1.5 and 1e-7 written at frame 1024.
    assert.deepEqual(loaded.pcm[0].slice(1024, 1027), [1.5, -1.5, Math.fround(1e-7)], `${file}: track 1's planted samples`);
    const sineOff = loaded.pcm[0].reduce((m, x, f) => (f >= 1024 && f < 1027 ? m : Math.max(m, Math.abs(x - 0.5 * Math.sin((2 * Math.PI * 220 * f) / V010_FRAMES)))), 0);
    assert.ok(sineOff < 1e-6, `${file}: track 1 is the 220 Hz sine (off by ${sineOff})`);
    await page.close();
  }

  // ── v0.8.9: v1 and v2 recoveries restored at boot, then saved as v3 ──
  const manifest = JSON.parse(readFileSync(join(V089, 'manifest.json'), 'utf8'));
  assert.ok(manifest.capture.commit.startsWith(V089_COMMIT) && !manifest.capture.dirty, `the fixtures were captured from v0.8.9: ${JSON.stringify(manifest.capture)}`);
  for (const c of CASES) {
    const entry = manifest.files.find((f) => f.file === c.file);
    const zip = readFileSync(join(V089, c.file));
    assert.equal(sha256(zip), entry.sha256, `${c.file} on disk differs from the manifest`);
    const old = readArchive(zip);
    const t = old.session.tracks[0];
    assert.deepEqual([old.session.formatVersion, old.session.sampleRate, old.session.tracks.length, old.stems[0].channels.length, old.stems[0].rate], [c.formatVersion, c.rate, 1, 1, c.rate],
      `${c.file}: a v${c.formatVersion} recovery at ${c.rate} Hz with one mono stem`);
    const pcm = old.stems[0].channels[0];
    const frames = entry.frames;
    assert.equal(pcm.length, frames);

    // A first load with no engine (nothing boots) seeds the record; the launch after it boots the engine
    // at the jam's rate, and its recovery restores the record.
    const context = await browser.newContext({ viewport: { width: 1280, height: 820 } });
    const { page } = await open({
      context,
      init: (p) => p.addInitScript((rate) => {
        if (sessionStorage.getItem('probe.boot') === '1') Object.assign(window, { __lfEngineFake: true, __lfEngineFakeRate: rate });
      }, c.rate),
    });
    await page.evaluate(seedRecord, { fields: entry.record, bytes: zip.toString('base64') });
    await page.evaluate(() => sessionStorage.setItem('probe.boot', '1'));
    await page.reload();
    await page.waitForFunction(() => '__lf' in window && window.__lf.native.opened.length === 1, undefined, { timeout: 10000 });
    await page.evaluate(() => window.__lf.autosave.ready());
    await page.waitForFunction(() => window.__lf.native.loadedSessions.length === 1, undefined, { timeout: 10000 });
    const loaded = await loadedSession(page);
    console.log(c.file, 'restored:', JSON.stringify(loaded.header));
    const mix = loaded.header.tracks[0]?.mix;
    assert.deepEqual(
      { ...loaded.header, tracks: loaded.header.tracks.map(({ mix: _, ...rest }) => rest) },
      { bpm: c.bpm, ...(c.formatVersion === 2 ? { meter: c.meter } : {}), bars: 1, masterLengthFrames: frames,
        tracks: [{ index: c.lane, frames, channels: 1, reversed: false, state: 'Playing' }] },
      `${c.file}: the restore's header (grid, meter, one channel)`,
    );
    assert.deepEqual({ volume: mix.volume, muted: mix.muted, dubFeedback: mix.dubFeedback, pan: mix.pan ?? 0, fx: mix.fx },
      { volume: t.volume, muted: t.muted, dubFeedback: t.dubFeedback, pan: t.pan, fx: t.fx }, `${c.file}: the restore's mix is the saved one`);
    assert.equal(loaded.pcm.length, 1, `${c.file}: one block of PCM`);
    assert.equal(firstDiff(loaded.pcm[0], pcm), -1, `${c.file}: the restored PCM is the stem's samples`);

    // The engine plays the mono stem on both sides: its feed and its snapshot say so. The next save is v3.
    await page.evaluate(async ({ rate, bpm, meter, lane: index, frames: n }) => {
      const { encodeSessionBytes, splitSessionBytes } = await import('/src/platform/engine-wire.ts');
      const [mono] = splitSessionBytes(window.__lf.native.loadedSessions[0].slice().buffer).pcm;
      window.__lf.native.snapshotBytes = encodeSessionBytes({ rate, masterLengthFrames: n, bpm, meter,
        tracks: [{ index, frames: n, channels: 2, reversed: false, state: 'Playing' }] }, [mono.slice(), mono.slice()]).buffer;
    }, { ...c, frames });
    await page.evaluate((f) => window.__lf.native.emit(f), jamFrame(1, c, frames, c.lane));
    const resaved = await page.evaluate(readLatest, true);
    await context.close();
    const now = readArchive(Buffer.from(resaved.bytes, 'base64'));
    console.log(c.file, 'saved again:', JSON.stringify({ record: resaved.fields, session: { ...now.session, tracks: now.session.tracks.map((x) => ({ ...x, fx: undefined })) } }));
    assert.deepEqual(
      [now.session.formatVersion, now.session.sampleRate, now.session.bpm, now.session.bars, now.session.meter ?? FOUR_FOUR, resaved.fields.rate],
      [3, c.rate, c.bpm, 1, c.meter, c.rate],
      `${c.file}: the next save is v3 at the jam's rate, grid and meter`,
    );
    assert.deepEqual(now.session.tracks.map((x) => [x.track, x.frames]), [[c.lane + 1, frames]], `${c.file}: the v3 save's track`);
    assert.equal(now.stems[0].channels.length, 2, `${c.file}: the v3 stem has two channels`);
    now.stems[0].channels.forEach((ch, k) => assert.equal(firstDiff(ch, pcm), -1, `${c.file}: the v3 stem's channel ${k + 1} is the v0.8.9 stem's samples`));
  }
});
