/**
 * The v0.1.0 session files, unchanged, through the product's import (`src/session/import.ts` via the
 * IMPORT button's file input) into an empty web engine fake (`src/platform/host.web.ts`): the downloaded
 * export (stems, a PCM16 master, session.json) and the recovery archive (stems, session.json), both in
 * `src-tauri/crates/lf-engine/tests/fixtures/v0.1.0` (their bytes pinned by
 * `src-tauri/crates/lf-engine/tests/v0_1_0_exports.rs`). Each must reach the engine as one session whose
 * header carries the grid (240 BPM, one bar, 48000 frames), both lanes playing forwards, and the mix as
 * saved (track 1 muted at 0.6, track 2 at 1.2 with its filter and delay on; DUB FEEDBACK, which the
 * format did not hold yet, at 100 %, and no pan), and whose PCM is each stem's float32 samples
 * exactly, read here from the archive's bytes without the product's zip or WAV code. Today's export
 * and import changing together cannot hide a break with these files.
 *
 * Cannot see the native engine's load of those bytes (`src-tauri/crates/lf-engine/tests/session.rs`),
 * Tauri's IPC or WebView2.
 * Run: pnpm probe legacy-sessions
 */
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { probe } from '../harness/probe.ts';

const DIR = 'src-tauri/crates/lf-engine/tests/fixtures/v0.1.0';
const FRAMES = 48000;

/** The float32 samples of `name` in a store-only zip, read from its local header and the WAV's data chunk. */
function stem(zip, name) {
  for (let at = 0; zip.readUInt32LE(at) === 0x04034b50; ) {
    const size = zip.readUInt32LE(at + 18);
    const nameLen = zip.readUInt16LE(at + 26);
    const data = at + 30 + nameLen + zip.readUInt16LE(at + 28);
    if (zip.toString('utf8', at + 30, at + 30 + nameLen) === name) {
      const wav = zip.subarray(data, data + size);
      for (let c = 12; c + 8 <= wav.length; c += 8 + wav.readUInt32LE(c + 4) + (wav.readUInt32LE(c + 4) & 1)) {
        if (wav.toString('latin1', c, c + 4) !== 'data') continue;
        return Array.from({ length: wav.readUInt32LE(c + 4) / 4 }, (_, i) => wav.readFloatLE(c + 8 + 4 * i));
      }
    }
    at = data + size;
  }
  throw new Error(`no ${name} in the archive`);
}

const fx = (filter, delay) => [
  { bypassed: !filter, params: filter ? { cutoff: 900, q: 3 } : { cutoff: 1200, q: 2 } },
  { bypassed: true, params: { semitones: 0 } },
  { bypassed: true, params: { rate: 1 } },
  { bypassed: !delay, params: delay ? { time: 2, feedback: 0.5, mix: 0.4 } : { time: 1, feedback: 0.4, mix: 0.3 } },
  { bypassed: true, params: { amount: 0.3 } },
];
const HEADER = {
  bpm: 240,
  bars: 1,
  masterLengthFrames: FRAMES,
  tracks: [
    { index: 0, frames: FRAMES, reversed: false, state: 'Playing', mix: { volume: 0.6, muted: true, dubFeedback: 1, fx: fx(false, false) } },
    { index: 1, frames: FRAMES, reversed: false, state: 'Playing', mix: { volume: 1.2, muted: false, dubFeedback: 1, fx: fx(true, true) } },
  ],
};

await probe(async ({ open }) => {
  for (const file of ['export.zip', 'recovery.zip']) {
    const zip = readFileSync(`${DIR}/${file}`);
    const { page } = await open({ init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)) });
    await page.waitForFunction(() => window.__lf.native.opened.length >= 1, undefined, { timeout: 10000 });
    await page.locator('input[type="file"]').setInputFiles({ name: file, mimeType: 'application/zip', buffer: zip });
    await page.waitForFunction(() => window.__lf.native.loadedSessions.length === 1, undefined, { timeout: 10000 });
    const loaded = await page.evaluate(async () => {
      const { splitSessionBytes } = await import('/src/platform/engine-wire.ts');
      const { header, pcm } = splitSessionBytes(window.__lf.native.loadedSessions[0].slice().buffer);
      return { header, pcm: pcm.map((block) => Array.from(block)) };
    });
    console.log(file, JSON.stringify(loaded.header));
    assert.deepEqual(loaded.header, HEADER, `${file}: the load's header`);
    assert.equal(loaded.pcm.length, 2, `${file}: two lanes of PCM`);
    for (const [i, name] of ['bleeploop-2026-09-25-0204-track1.wav', 'bleeploop-2026-09-25-0204-track2.wav'].entries()) {
      const want = stem(zip, name);
      assert.equal(want.length, FRAMES);
      const off = loaded.pcm[i].findIndex((x, f) => x !== want[f]);
      assert.ok(loaded.pcm[i].length === FRAMES && off === -1, `${file}: lane ${i + 1}'s PCM is ${name}'s samples (first difference at frame ${off})`);
    }
    // Track 1 as captured: a 220 Hz sine at 0.5, with 1.5, -1.5 and 1e-7 written at frame 1024.
    assert.deepEqual(loaded.pcm[0].slice(1024, 1027), [1.5, -1.5, Math.fround(1e-7)], `${file}: track 1's planted samples`);
    const sineOff = loaded.pcm[0].reduce((m, x, f) => (f >= 1024 && f < 1027 ? m : Math.max(m, Math.abs(x - 0.5 * Math.sin((2 * Math.PI * 220 * f) / FRAMES)))), 0);
    assert.ok(sineOff < 1e-6, `${file}: track 1 is the 220 Hz sine (off by ${sineOff})`);
    await page.close();
  }
});
