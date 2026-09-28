/** Checks that the export's offline wet render still produces the Tone references the engine's FX ports
 * are tested against (`src-tauri/crates/lf-engine/tests/fixtures/tone`, `docs/plans/native-engine.md`
 * § Stage 3). It re-renders the manifest's fx, limiter and IR scenarios in Tone OfflineContexts through
 * the production modules the export uses (`FxChain`, `makeReverbBus` and `makeMasterLimiter` from
 * `src/session/offline-fx.ts`), with `Math.random` replaced by the seeded mulberry32 each scenario was
 * captured with (seed 1000 + its index in the manifest), encodes each render as float32 WAV through the
 * real `encodeWav`, and compares every sample with the committed file (1e-6: two renders differ by up to
 * ~1.2e-7, Blink sums a node's inputs in no fixed order). It also regenerates the noise tables from their
 * seed and asserts Tone's own buffers equal that regeneration and the manifest's hashes (the reverb IR
 * draws on the white table), that the fixtures stay within their 10 MB budget with export-refs's v0.1.0
 * files, and that the files on disk are the manifest's.
 *
 * The fixtures are frozen: the synth scenarios' Tone voices went with `src/audio/synths` (the engine's
 * ports replay them in lf-engine `tests/synth.rs` and `tests/voices.rs`), so the set can no longer be
 * captured whole and this probe only compares. It sees Chromium's offline rendering, not WebView2 or the
 * engine; control changes land on 128-frame boundaries because that is where Tone's offline clock ticks.
 * Nothing is audible.
 * @no-ci compares committed fixtures; CI's Chromium may differ from the capture's
 * Run: pnpm probe tone-refs
 */
import assert from 'node:assert/strict';
import { existsSync, readFileSync, readdirSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { probe } from '../harness/probe.ts';

const DIR = join(import.meta.dirname, '../../src-tauri/crates/lf-engine/tests/fixtures/tone');
const BUDGET_BYTES = 10 * 1024 * 1024;
/** The groups the export's render still runs; `synth` went with the Tone voices. */
const RENDERED = new Set(['fx', 'limiter', 'ir']);

// ── The page side: everything below runs in the app's page ───────────────────────────────────────
async function inPage({ list, tableSeed }) {
  const transformed = await (await fetch('/src/session/offline-fx.ts')).text();
  const tonePath = transformed.match(/from\s+["']([^"']*\/tone[^"']*)["']/)?.[1];
  if (!tonePath) throw new Error('Could not resolve the production Tone module');
  const Tone = await import(tonePath);
  const { FxChain, makeReverbBus, makeMasterLimiter } = await import('/src/session/offline-fx.ts');
  const { encodeWav } = await import('/src/session/wav.ts');

  const mulberry32 = (seed) => {
    let a = seed | 0;
    return () => {
      a = (a + 0x6d2b79f5) | 0;
      let t = Math.imul(a ^ (a >>> 15), 1 | a);
      t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
      return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
    };
  };
  let draws = [];
  const seedRandom = (seed) => {
    const next = mulberry32(seed);
    draws = [];
    Math.random = () => { const v = next(); draws.push(v); return v; };
  };
  const sha = async (arrays) => {
    const bytes = new Uint8Array(arrays.reduce((n, a) => n + a.byteLength, 0));
    let at = 0;
    for (const a of arrays) { bytes.set(new Uint8Array(a.buffer, a.byteOffset, a.byteLength), at); at += a.byteLength; }
    return [...new Uint8Array(await crypto.subtle.digest('SHA-256', bytes))].map((b) => b.toString(16).padStart(2, '0')).join('');
  };
  const b64 = (bytes) => {
    let s = '';
    for (let k = 0; k < bytes.length; k += 0x8000) s += String.fromCharCode(...bytes.subarray(k, k + 0x8000));
    return btoa(s);
  };

  // ── Noise tables: Tone generates them lazily, on a Noise's first start, from Math.random ──
  const TABLE_RATE = 48000;
  seedRandom(tableSeed);
  let captured = {};
  await Tone.Offline((offline) => {
    for (const type of ['white', 'pink']) {
      const noise = new Tone.Noise({ type, context: offline });
      noise.start(0);
      const buffer = noise._source.buffer.get();
      captured[type] = { rate: buffer.sampleRate, channels: [0, 1].map((c) => buffer.getChannelData(c).slice()) };
    }
  }, 128 / TABLE_RATE, 1, TABLE_RATE);
  // Regenerate independently (Tone's algorithm, source/Noise.js) and require equality.
  const regen = mulberry32(tableSeed);
  const LENGTH = 44100 * 5;
  const expected = {};
  expected.white = [0, 1].map(() => Float32Array.from({ length: LENGTH }, () => regen() * 2 - 1));
  regen(); // the white Noise's start offset
  expected.pink = [0, 1].map(() => {
    const out = new Float32Array(LENGTH);
    let b0 = 0, b1 = 0, b2 = 0, b3 = 0, b4 = 0, b5 = 0, b6 = 0;
    for (let i = 0; i < LENGTH; i++) {
      const white = regen() * 2 - 1;
      b0 = 0.99886 * b0 + white * 0.0555179;
      b1 = 0.99332 * b1 + white * 0.0750759;
      b2 = 0.969 * b2 + white * 0.153852;
      b3 = 0.8665 * b3 + white * 0.3104856;
      b4 = 0.55 * b4 + white * 0.5329522;
      b5 = -0.7616 * b5 - white * 0.016898;
      out[i] = b0 + b1 + b2 + b3 + b4 + b5 + b6 + white * 0.5362;
      out[i] *= 0.11;
      b6 = white * 0.115926;
    }
    return out;
  });
  const tables = { sha256: {} };
  for (const type of ['white', 'pink']) {
    const got = await sha(captured[type].channels);
    const want = await sha(expected[type]);
    if (got !== want) throw new Error(`${type} noise table is not the seeded regeneration (cache filled earlier?)`);
    tables.sha256[type] = got;
  }
  captured = null;

  // ── Seeded inputs (exact arithmetic only, so Rust regenerates them bit for bit) ──
  const fxInput = (rate, frames, active) => {
    const x = new Float32Array(frames);
    const noise = mulberry32(7);
    const quarter = Math.round(rate / 4);
    const on = Math.round(active * rate);
    let phase = 0;
    for (let n = 0; n < frames; n++) {
      phase += (110 * (1 + (3 * n) / frames)) / rate;
      phase -= Math.floor(phase);
      const w = noise() * 2 - 1;
      const burst = n % quarter < quarter / 10 ? 1 : 0;
      x[n] = n < on ? 0.4 * (2 * phase - 1) + 0.3 * w * burst : 0;
    }
    return x;
  };
  const limiterInput = (rate, frames, ramp) => {
    const l = new Float32Array(frames);
    const r = new Float32Array(frames);
    const rampFrames = Math.round(ramp * rate);
    const burst = Math.round(0.05 * rate);
    const gap = Math.round(0.15 * rate);
    let pl = 0, pr = 0;
    for (let n = 0; n < frames; n++) {
      pl += 220 / rate; pl -= Math.floor(pl);
      pr += 330 / rate; pr -= Math.floor(pr);
      let a;
      if (n < rampFrames) a = 0.1 + (3.9 * n) / rampFrames;
      else a = (n - rampFrames) % (burst + gap) < burst ? 3 : 0.05;
      l[n] = a * (1 - 4 * Math.abs(pl - 0.5));
      r[n] = 0.5 * a * (1 - 4 * Math.abs(pr - 0.5));
    }
    return [l, r];
  };
  const bufferOf = (raw, channels) => {
    const buffer = raw.createBuffer(channels.length, channels[0].length, raw.sampleRate);
    channels.forEach((c, k) => buffer.copyToChannel(c, k));
    return buffer;
  };
  const input = (node) => node.input ? input(node.input) : node;

  const out = [];
  for (const { index, scenario: s } of list) {
    seedRandom(1000 + index);
    let channels;
    if (s.group === 'ir') {
      let reverb;
      await Tone.Offline(async (offline) => {
        const bus = makeReverbBus(offline.rawContext.destination, offline);
        reverb = bus.reverb;
        await bus.ready;
      }, 128 / s.rate, 1, s.rate);
      const ir = reverb._convolver.buffer; // Reverb holds a native ConvolverNode
      channels = [0, 1].map((c) => ir.getChannelData(c).slice());
    } else {
      const frames = Math.round(s.seconds * s.rate);
      const rendered = await Tone.Offline(async (offline) => {
        const raw = offline.rawContext;
        const ticks = [];
        offline.on('tick', () => {
          const frame = Math.round(offline.currentTime * s.rate);
          while (ticks.length && ticks[0].frame <= frame) ticks.shift().run();
        });
        if (s.group === 'fx') {
          const reverbBus = s.setup.reverb ? makeReverbBus(raw.destination, offline) : null;
          const chain = new FxChain(s.setup.states, { context: offline, dest: raw.destination, reverbBus: reverbBus?.bus ?? new Tone.Gain({ gain: 0, context: offline }) });
          chain.setTiming(s.setup.timing);
          for (const e of s.events) {
            ticks.push({ frame: e.frame, run: () => e.kind === 'bypass' ? chain.nodes[e.fx].setBypass(e.value) : chain.nodes[e.fx].setParam(e.key, e.value) });
          }
          const source = raw.createBufferSource();
          source.buffer = bufferOf(raw, [fxInput(s.rate, frames, s.setup.input.active)]);
          source.connect(input(chain.input));
          source.start(0);
          if (reverbBus) await reverbBus.ready;
        } else if (s.group === 'limiter') {
          const source = raw.createBufferSource();
          source.buffer = bufferOf(raw, limiterInput(s.rate, frames, s.setup.input.ramp));
          source.connect(makeMasterLimiter(raw)).connect(raw.destination);
          source.start(0);
        }
        ticks.sort((a, b) => a.frame - b.frame);
      }, s.seconds, 2, s.rate);
      channels = [0, 1].map((c) => rendered.getChannelData(c).slice());
    }
    const mono = channels[0].every((v, k) => Object.is(v, channels[1][k]));
    const stored = mono ? [channels[0]] : channels;
    const wav = encodeWav(stored, s.rate, 'float32');
    let peak = 0;
    for (const c of stored) for (const v of c) peak = Math.max(peak, Math.abs(v));
    out.push({ id: s.id, channels: stored.length, peak, draws: draws.length, wav: b64(wav) });
  }
  return { tables, scenarios: out };
}

// ── The Node side ────────────────────────────────────────────────────────────────────────────────
await probe(async ({ open, browser }) => {
  const manifest = JSON.parse(readFileSync(join(DIR, 'manifest.json'), 'utf8'));
  const list = manifest.scenarios.map((scenario, index) => ({ index, scenario })).filter(({ scenario }) => RENDERED.has(scenario.group));
  const { page } = await open();
  const { tables, scenarios: rendered } = await page.evaluate(inPage, { list, tableSeed: manifest.tables.seed });

  const files = rendered.map((s) => ({ id: s.id, bytes: Buffer.from(s.wav, 'base64'), channels: s.channels, peak: s.peak, draws: s.draws }));
  console.log(JSON.stringify(files.map((f) => ({ id: f.id, ch: f.channels, kb: Math.round(f.bytes.length / 1024), peak: +f.peak.toFixed(3), draws: f.draws }))));
  assert.deepEqual(files.map((f) => f.id), list.map(({ scenario }) => scenario.id), 'every fx, limiter and IR scenario rendered');
  for (const f of files) assert.ok(f.peak > 1e-3, `${f.id} rendered silence`);

  // One budget with export-refs's v0.1.0 fixtures (the two probes check the same sum).
  const dirBytes = (dir, ext) => existsSync(dir)
    ? readdirSync(dir).filter((n) => n.endsWith(ext)).reduce((n, name) => n + statSync(join(dir, name)).size, 0)
    : 0;
  const onDisk = dirBytes(DIR, '.wav');
  const exportBytes = dirBytes(join(DIR, '../v0.1.0'), '.zip');
  console.log(`tone ${(onDisk / 1048576).toFixed(2)} MB, with v0.1.0 ${((onDisk + exportBytes) / 1048576).toFixed(2)} MB of ${BUDGET_BYTES / 1048576} MB`);
  assert.ok(onDisk + exportBytes <= BUDGET_BYTES, 'fixtures over budget (tone + v0.1.0)');

  assert.equal(manifest.tables.sha256.white, tables.sha256.white, 'white noise table changed');
  assert.equal(manifest.tables.sha256.pink, tables.sha256.pink, 'pink noise table changed');
  const byId = new Map(manifest.scenarios.map((s) => [s.id, s]));
  const samples = (bytes) => new Float32Array(bytes.buffer.slice(bytes.byteOffset + 58, bytes.byteOffset + bytes.length));
  let worst = 0;
  const changed = [];
  for (const f of files) {
    const committed = byId.get(f.id);
    const old = committed && existsSync(join(DIR, committed.file)) ? samples(readFileSync(join(DIR, committed.file))) : null;
    const now = samples(f.bytes);
    if (!old || old.length !== now.length) { changed.push(f.id); continue; }
    let diff = 0;
    for (let k = 0; k < now.length; k++) diff = Math.max(diff, Math.abs(now[k] - old[k]));
    worst = Math.max(worst, diff);
    if (diff > 1e-6) changed.push(f.id);
  }
  console.log(JSON.stringify({ chromium: browser.version(), captured: manifest.capture.chromium, compared: files.length, worst, changed }));
  assert.deepEqual(changed, [], 'renders differ from the committed references');
  const listed = manifest.scenarios.reduce((n, s) => n + s.bytes, 0);
  assert.equal(onDisk, listed, 'fixture files on disk differ from the manifest');
});
