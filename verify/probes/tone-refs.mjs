/** Captures the Tone reference renders the native engine's Stage 3 ports are tested against
 * (`docs/plans/native-engine.md` § Stage 3). It renders the production modules in Tone OfflineContexts
 * (the six synths from `src/audio/synths`, `FxChain` from `src/audio/fx/fx.ts`, `makeMasterLimiter` and
 * the reverb IR) with `Math.random` replaced by a seeded mulberry32, and encodes each render as float32
 * WAV through the real `encodeWav`. Without `--write` it re-renders and compares every sample with the
 * committed files (does this Chromium and Tone still produce the references?); with `--write` it
 * replaces the fixtures and the manifest. Two renders of one scenario differ by up to ~1.2e-7 (Blink
 * sums a node's inputs in no fixed order), so the comparison allows 1e-6, far inside the ports' −60 dB
 * class. Regenerate only with a stated reason: the ports' tolerance classes are recorded against
 * these files.
 *
 * What the Rust tests replay comes from the manifest: each scenario's setup, its frame-stamped events,
 * the seeded inputs (their sha256, so a port proves it regenerated them bit-exactly) and every
 * `Math.random` draw the scenario made. The noise tables are not stored: they are regenerated from
 * their seed (`tables`), and the probe asserts Tone's own buffers equal that regeneration.
 *
 * It sees Chromium's offline rendering, not WebView2 or a live AudioContext; control changes (pitch
 * bend, mod wheel, FX params) land on 128-frame boundaries because that is where Tone's offline clock
 * ticks. Nothing is audible.
 * @no-ci capture tool for committed fixtures; CI's Chromium may differ from the capture's
 * Run: pnpm probe tone-refs [--write]
 */
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { execSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { flag, probe } from '../harness/probe.ts';

const write = flag('write');
const DIR = join(import.meta.dirname, '../../src-tauri/crates/lf-engine/tests/fixtures/tone');
const BUDGET_BYTES = 10 * 1024 * 1024;

// ── Scenario scripts (plain data; the manifest carries them to the Rust tests) ────────────────────
const sec = (rate, s) => Math.round(s * rate);
/** A control change lands on Tone's offline tick, a 128-frame boundary. */
const tick = (rate, s) => Math.round(s * rate / 128) * 128;

function synthNotes(rate) {
  // Range and velocity: low to high, loud to soft.
  const e = [];
  [[36, 1.0, 0.0], [60, 0.5, 0.25], [84, 0.8, 0.5], [96, 0.25, 0.75]].forEach(([note, velocity, at]) => {
    e.push({ frame: sec(rate, at), kind: 'on', note, velocity });
    e.push({ frame: sec(rate, at + 0.2), kind: 'off', note });
  });
  return e;
}

function synthPoly(rate, polyphony) {
  // A full-polyphony chord, one more note (a voice steal), release; legato; bend and vibrato.
  const e = [];
  const chord = Array.from({ length: polyphony }, (_, k) => 48 + k * 4);
  for (const note of chord) e.push({ frame: 0, kind: 'on', note, velocity: 0.7 });
  e.push({ frame: sec(rate, 0.3), kind: 'on', note: 91, velocity: 0.9 });
  for (const note of [...chord, 91]) e.push({ frame: sec(rate, 0.6), kind: 'off', note });
  e.push({ frame: sec(rate, 0.8), kind: 'on', note: 64, velocity: 0.8 });
  e.push({ frame: sec(rate, 0.9), kind: 'on', note: 67, velocity: 0.6 });
  e.push({ frame: sec(rate, 1.0), kind: 'off', note: 64 });
  e.push({ frame: sec(rate, 1.1), kind: 'off', note: 67 });
  e.push({ frame: sec(rate, 1.15), kind: 'on', note: 57, velocity: 1.0 });
  e.push({ frame: tick(rate, 1.2), kind: 'bend', value: 2 });
  e.push({ frame: tick(rate, 1.25), kind: 'mod', value: 1 });
  e.push({ frame: tick(rate, 1.45), kind: 'bend', value: -1.5 });
  e.push({ frame: tick(rate, 1.55), kind: 'mod', value: 0 });
  e.push({ frame: sec(rate, 1.6), kind: 'off', note: 57 });
  return e;
}

function drumKit(rate) {
  const notes = [49, 51, 56, 54, 39, 40, 44, 37, 36, 38, 42, 46, 35, 45, 47, 50];
  return notes.map((note, k) => ({ frame: sec(rate, k * 0.1), kind: 'on', note, velocity: k % 2 ? 0.5 : 1.0 }));
}

const POLYPHONY = { lead: 8, bass: 3, pad: 12, piano: 12, organ: 8 };

/** FX state helpers: five entries in chain order [filter, pitch, stutter, delay, reverb]. */
const fxOff = () => [
  { bypassed: true, params: { cutoff: 1200, q: 2 } },
  { bypassed: true, params: { semitones: 0 } },
  { bypassed: true, params: { rate: 1 } },
  { bypassed: true, params: { time: 1, feedback: 0.4, mix: 0.3 } },
  { bypassed: true, params: { amount: 0.3 } },
];
function fxWith(index, params) {
  const states = fxOff();
  states[index] = { bypassed: false, params: { ...states[index].params, ...params } };
  return states;
}

function scenarios() {
  const list = [];
  const add = (s) => list.push({ ...s, id: `${s.id}-${s.rate}` });
  for (const rate of [48000]) {
    for (const synth of ['lead', 'bass', 'pad', 'piano', 'organ']) {
      add({ id: `synth-${synth}-notes`, group: 'synth', rate, seconds: 1.0, setup: { synth }, events: synthNotes(rate) });
      add({ id: `synth-${synth}-poly`, group: 'synth', rate, seconds: 1.7, setup: { synth }, events: synthPoly(rate, POLYPHONY[synth]) });
    }
    add({ id: 'synth-drum-kit', group: 'synth', rate, seconds: 2.4, setup: { synth: 'drum' }, events: drumKit(rate) });
    const timing120 = { anchor: 0, beatPeriod: Math.round(240 / 120 * rate) / rate / 4 };
    const timing137 = { anchor: -0.087, beatPeriod: Math.round(240 / 137 * rate) / rate / 4 };
    const fx = (id, seconds, active, states, extra = {}) =>
      add({ id: `fx-${id}`, group: 'fx', rate, seconds, setup: { states, timing: timing120, input: { active }, ...extra }, events: [] });
    fx('filter-dark', 1.0, 1.0, fxWith(0, { cutoff: 300, q: 0.7 }));
    fx('filter-reso', 1.0, 1.0, fxWith(0, { cutoff: 6000, q: 10 }));
    add({ id: 'fx-filter-bypass', group: 'fx', rate, seconds: 1.0, setup: { states: fxWith(0, { cutoff: 800, q: 4 }), timing: timing120, input: { active: 1.0 } },
      events: [{ frame: tick(rate, 0.3), kind: 'param', fx: 0, key: 'cutoff', value: 2400 },
        { frame: tick(rate, 0.5), kind: 'bypass', fx: 0, value: true },
        { frame: tick(rate, 0.75), kind: 'bypass', fx: 0, value: false }] });
    fx('pitch-down12', 1.0, 0.7, fxWith(1, { semitones: -12 }));
    fx('pitch-up7', 1.0, 0.7, fxWith(1, { semitones: 7 }));
    fx('stutter-8th', 1.0, 1.0, fxWith(2, { rate: 1 }));
    add({ id: 'fx-stutter-137', group: 'fx', rate, seconds: 1.0, setup: { states: fxWith(2, { rate: 3 }), timing: timing137, input: { active: 1.0 } },
      events: [{ frame: tick(rate, 0.5), kind: 'param', fx: 2, key: 'rate', value: 2 }] });
    fx('delay-default', 1.5, 0.4, fxWith(3, {}));
    fx('delay-hot', 1.5, 0.4, fxWith(3, { time: 0, feedback: 0.95, mix: 1 }), { timing: timing137 });
    fx('reverb-full', 2.2, 0.3, fxWith(4, { amount: 1 }), { reverb: true });
    add({ id: 'limiter-sweep', group: 'limiter', rate, seconds: 1.5, setup: { input: { ramp: 1.0 } }, events: [] });
    add({ id: 'ir-reverb', group: 'ir', rate, seconds: 0, setup: {}, events: [] });
  }
  // The 44.1 k spot set: no noise voices (the tables are generated once, at 48 k).
  const rate = 44100;
  add({ id: 'synth-lead-poly', group: 'synth', rate, seconds: 1.7, setup: { synth: 'lead' }, events: synthPoly(rate, POLYPHONY.lead) });
  add({ id: 'fx-filter-reso', group: 'fx', rate, seconds: 1.0,
    setup: { states: fxWith(0, { cutoff: 6000, q: 10 }), timing: { anchor: 0, beatPeriod: Math.round(240 / 120 * rate) / rate / 4 }, input: { active: 1.0 } }, events: [] });
  add({ id: 'limiter-sweep', group: 'limiter', rate, seconds: 1.0, setup: { input: { ramp: 0.7 } }, events: [] });
  for (const s of list) s.frames = Math.round(s.seconds * s.rate);
  return list;
}

// ── The page side: everything below runs in the app's page ───────────────────────────────────────
async function inPage({ list, tableSeed }) {
  const lf = window.__lf;
  await lf.engine.start();
  const transformed = await (await fetch('/src/audio/fx/fx.ts')).text();
  const tonePath = transformed.match(/from\s+["']([^"']*\/tone[^"']*)["']/)?.[1];
  if (!tonePath) throw new Error('Could not resolve the production Tone module');
  const Tone = await import(tonePath);
  const { SYNTHS } = await import('/src/audio/synths/index.ts');
  const { FxChain, makeReverbBus } = await import('/src/audio/fx/fx.ts');
  const { engine, makeMasterLimiter } = await import('/src/audio/engine.ts');
  const { encodeWav } = await import('/src/audio/export/wav.ts');

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
  const tables = { prng: 'mulberry32', seed: tableSeed, order: ['white', 'pink'], length: LENGTH, channels: 2, sha256: {} };
  for (const type of ['white', 'pink']) {
    const got = await sha(captured[type].channels);
    const want = await sha(expected[type]);
    if (got !== want) throw new Error(`${type} noise table is not the seeded regeneration (cache filled earlier?)`);
    tables.sha256[type] = got;
    tables.rate = captured[type].rate;
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
  for (const [index, s] of list.entries()) {
    seedRandom(1000 + index);
    let inputSha = null;
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
      s.frames = ir.length;
      s.setup = { decay: reverb.decay, preDelay: reverb.preDelay, normalize: reverb._convolver.normalize };
    } else {
      const rendered = await Tone.Offline(async (offline) => {
        const raw = offline.rawContext;
        const at = (frame) => frame / s.rate;
        const ticks = [];
        offline.on('tick', () => {
          const frame = Math.round(offline.currentTime * s.rate);
          while (ticks.length && ticks[0].frame <= frame) ticks.shift().run();
        });
        if (s.group === 'synth') {
          const bus = raw.createGain();
          bus.connect(raw.destination);
          Object.defineProperty(engine, 'instrumentBus', { configurable: true, get: () => bus });
          let synth;
          try { synth = SYNTHS.find((f) => f.id === s.setup.synth).create(); }
          finally { delete engine.instrumentBus; }
          for (const e of s.events) {
            if (e.kind === 'on') synth.noteOn(e.note, e.velocity, at(e.frame));
            else if (e.kind === 'off') synth.noteOff(e.note, at(e.frame));
            else if (e.kind === 'bend') ticks.push({ frame: e.frame, run: () => synth.setPitchBend(e.value) });
            else if (e.kind === 'mod') ticks.push({ frame: e.frame, run: () => synth.setModulation(e.value) });
          }
        } else if (s.group === 'fx') {
          const reverbBus = s.setup.reverb ? makeReverbBus(raw.destination, offline) : null;
          const chain = new FxChain(s.setup.states, { context: offline, dest: raw.destination, reverbBus: reverbBus?.bus ?? new Tone.Gain({ gain: 0, context: offline }) });
          chain.setTiming(s.setup.timing);
          for (const e of s.events) {
            ticks.push({ frame: e.frame, run: () => e.kind === 'bypass' ? chain.nodes[e.fx].setBypass(e.value) : chain.nodes[e.fx].setParam(e.key, e.value) });
          }
          const x = fxInput(s.rate, s.frames, s.setup.input.active);
          inputSha = await sha([x]);
          const source = raw.createBufferSource();
          source.buffer = bufferOf(raw, [x]);
          source.connect(input(chain.input));
          source.start(0);
          if (reverbBus) await reverbBus.ready;
        } else if (s.group === 'limiter') {
          const x = limiterInput(s.rate, s.frames, s.setup.input.ramp);
          inputSha = await sha(x);
          const source = raw.createBufferSource();
          source.buffer = bufferOf(raw, x);
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
    out.push({ ...s, channels: stored.length, peak, inputSha256: inputSha, random: draws.slice(), wav: b64(wav) });
  }
  return { tables, scenarios: out };
}

// ── The Node side ────────────────────────────────────────────────────────────────────────────────
await probe(async ({ open, browser }) => {
  const { page } = await open();
  const list = scenarios();
  const { tables, scenarios: rendered } = await page.evaluate(inPage, { list, tableSeed: 1 });
  const tone = JSON.parse(readFileSync(join(import.meta.dirname, '../../node_modules/tone/package.json'), 'utf8')).version;
  const head = execSync('git rev-parse HEAD', { encoding: 'utf8' }).trim();
  const dirty = execSync('git status --porcelain', { encoding: 'utf8' }).trim() !== '';

  const files = rendered.map((s) => {
    const bytes = Buffer.from(s.wav, 'base64');
    const { wav: _wav, ...rest } = s;
    return { bytes, entry: { ...rest, file: `${s.id}.wav`, sha256: createHash('sha256').update(bytes).digest('hex'), bytes: bytes.length } };
  });
  const total = files.reduce((n, f) => n + f.bytes.length, 0);
  console.log(JSON.stringify(files.map((f) => ({ id: f.entry.id, ch: f.entry.channels, kb: Math.round(f.entry.bytes / 1024), peak: +f.entry.peak.toFixed(3), draws: f.entry.random.length }))));
  // One budget with export-refs's v0.1.0 fixtures (the two probes check the same sum).
  const exportsDir = join(DIR, '../v0.1.0');
  const exportBytes = existsSync(exportsDir)
    ? readdirSync(exportsDir).filter((n) => n.endsWith('.zip')).reduce((n, name) => n + statSync(join(exportsDir, name)).size, 0)
    : 0;
  console.log(`total ${(total / 1048576).toFixed(2)} MB, with v0.1.0 ${((total + exportBytes) / 1048576).toFixed(2)} MB of ${BUDGET_BYTES / 1048576} MB`);
  assert.ok(total + exportBytes <= BUDGET_BYTES, 'fixtures over budget (tone + v0.1.0)');
  for (const f of files) assert.ok(f.entry.peak > 1e-3, `${f.entry.id} rendered silence`);

  if (write) {
    if (existsSync(DIR)) for (const name of readdirSync(DIR)) if (name.endsWith('.wav')) rmSync(join(DIR, name));
    mkdirSync(DIR, { recursive: true });
    for (const f of files) writeFileSync(join(DIR, f.entry.file), f.bytes);
    const manifest = {
      note: 'Written by verify/probes/tone-refs.mjs --write. Regenerate only with a stated reason; each port records its tolerance class against these bytes.',
      capture: { commit: head, dirty, tone, chromium: browser.version(), rates: [...new Set(rendered.map((s) => s.rate))] },
      tables,
      scenarios: files.map((f) => f.entry),
    };
    writeFileSync(join(DIR, 'manifest.json'), `${JSON.stringify(manifest, null, 1)}\n`);
    console.log(`wrote ${files.length} fixtures to ${DIR}`);
  } else {
    const manifest = JSON.parse(readFileSync(join(DIR, 'manifest.json'), 'utf8'));
    assert.equal(manifest.tables.sha256.white, tables.sha256.white, 'white noise table changed');
    assert.equal(manifest.tables.sha256.pink, tables.sha256.pink, 'pink noise table changed');
    const byId = new Map(manifest.scenarios.map((s) => [s.id, s]));
    const samples = (bytes) => new Float32Array(bytes.buffer.slice(bytes.byteOffset + 58, bytes.byteOffset + bytes.length));
    let worst = 0;
    const changed = [];
    for (const f of files) {
      const committed = byId.get(f.entry.id);
      const old = committed && existsSync(join(DIR, committed.file)) ? samples(readFileSync(join(DIR, committed.file))) : null;
      const now = samples(f.bytes);
      if (!old || old.length !== now.length) { changed.push(f.entry.id); continue; }
      let diff = 0;
      for (let k = 0; k < now.length; k++) diff = Math.max(diff, Math.abs(now[k] - old[k]));
      worst = Math.max(worst, diff);
      if (diff > 1e-6) changed.push(f.entry.id);
    }
    console.log(JSON.stringify({ chromium: browser.version(), captured: manifest.capture.chromium, worst, changed }));
    assert.deepEqual(changed, [], 'renders differ from the committed references');
    const onDisk = readdirSync(DIR).filter((n) => n.endsWith('.wav')).reduce((n, name) => n + statSync(join(DIR, name)).size, 0);
    assert.equal(onDisk, total, 'fixture files on disk differ from the manifest');
  }
});
