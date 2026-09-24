// verify/guards/worklet-pop.mjs — executes the REAL plugin-pcm-source.ts process() in Node against a simulated
// hop-1 ring (a plain ArrayBuffer laid out the way the native host writes it) and a worklet-global shim.
//
// What this PROVES about the render-thread end of the plugin bridge:
//   start    it waits for a setpoint's worth, then plays from the newest setpoint's worth, holding the
//            controller's level (header[3]) on the setpoint
//   steady   a producer at the render rate plays gapless across the ring's wrap and the u32 cursor wrap,
//            with the read cursor published in header[1] and nothing counted as loss
//   settle   after a new producer epoch the queue's mean moves the read cursor ONCE: a queue long by X
//            drops X (counted), a short one inserts X of silence (counted as an underrun), a step inside
//            the tolerance moves nothing and counts nothing
//   underrun a starved quantum zero-fills its shortfall and counts, and the settle puts the queue back
//   lag cap  a backlog past maxLag drops to the setpoint at once (counted)
//   level    without a step the level follows the queue (the drift controller's PV)
// and process() never creates a typed-array view (invariant 5); 'close' ends the processor.

import assert from 'node:assert/strict';

const WANT = 128;
const SR = 48000;
const CAP = 16384;
const HEADER = 32;
const TARGET = 1440;
const MAX_LAG = 2880;
const SETTLE = SR; // 1 s
const SETTLE_QUANTA = Math.ceil(SETTLE / WANT);
const TOLERANCE = SR / 1000;

globalThis.sampleRate = SR;
globalThis.AudioWorkletProcessor = class {
  constructor() {
    this.port = { onmessage: null, postMessage() {} };
  }
};
let Processor;
globalThis.registerProcessor = (name, ctor) => {
  assert.equal(name, 'plugin-pcm-source');
  Processor = ctor;
};
await import('../../src/audio/worklets/plugin-pcm-source.ts');

let passed = 0;
let failed = 0;
function check(name, fn) {
  try { fn(); passed++; } catch (e) { failed++; console.error(`FAIL: ${name}\n  ${e.message}`); }
}

/** Distinct, non-zero, signed samples: the k-th produced frame is identifiable in the output. */
const PERIOD = 9973;
const sample = (k) => Math.fround(((k % 2 === 0 ? 1 : -1) * ((k % PERIOD) + 1)) / 16384);

let views = 0;
const subarray = Float32Array.prototype.subarray;
Float32Array.prototype.subarray = function (...args) { views++; return subarray.apply(this, args); };

/** A processor on a fresh ring whose cursors start at `base` (u32). */
function rig(base = 0) {
  const ab = new ArrayBuffer(HEADER + CAP * 4);
  const h = new Uint32Array(ab, 0, 8);
  const d = new Float32Array(ab, HEADER, CAP);
  h[0] = base;
  h[1] = base;
  h[2] = CAP;
  const stats = new Int32Array(new SharedArrayBuffer(6 * 4));
  const processor = new Processor({
    processorOptions: {
      statsSab: stats.buffer, headerBytes: HEADER, capacityFrames: CAP,
      targetFrames: TARGET, maxLagFrames: MAX_LAG, settleFrames: SETTLE,
    },
  });
  let produced = 0;
  let expect = 0; // the produced frame the next played sample should be
  const events = []; // discontinuities in what was played
  const r = {
    h, stats, processor,
    deliver: () => processor.port.onmessage({ data: ab }),
    produce(n) {
      for (let i = 0; i < n; i++) {
        d[(base + produced) & (CAP - 1)] = sample(produced);
        produced++;
      }
      h[0] = (base + produced) >>> 0;
    },
    queue: () => (h[0] - h[1]) >>> 0,
    /** One quantum; returns what process() returned and the output. */
    render() {
      const out = new Float32Array(WANT).fill(7); // pre-dirty to catch un-written slots
      const alive = processor.process([], [[out]]);
      return { out, alive };
    },
    /** One quantum, following the played frames: `events` gets every jump and every silence run. */
    play() {
      const { out } = r.render();
      for (const v of out) {
        if (v === sample(expect)) { expect++; continue; }
        if (v === 0) {
          const last = events[events.length - 1];
          if (last && last.kind === 'silence' && last.open) last.frames++;
          else events.push({ kind: 'silence', frames: 1, open: true });
          continue;
        }
        let k = expect + 1;
        while (k < expect + CAP && sample(k) !== v) k++;
        events.push({ kind: 'jump', by: k - expect, bad: k === expect + CAP });
        expect = k + 1;
      }
      const last = events[events.length - 1];
      if (last && last.kind === 'silence' && out[WANT - 1] !== 0) last.open = false;
      return out;
    },
    /** `quanta` quanta with `perQuantum` produced before each. */
    run(quanta, perQuantum = WANT) {
      for (let i = 0; i < quanta; i++) { r.produce(perQuantum); r.play(); }
    },
    setExpect: (k) => { expect = k; },
    events,
    closeEvents: () => events.forEach((e) => { e.open = false; }),
    loss: () => ({ underruns: stats[1], dropped: stats[2] }),
  };
  return r;
}
const clean = (events) => events.map(({ open: _open, bad, ...e }) => (bad ? { ...e, bad } : e));

// ── start ──────────────────────────────────────────────────────────────────────────────────────
{
  const r = rig();
  check('no buffer yet: silence, keeps running', () => {
    const { out, alive } = r.render();
    assert.equal(alive, true);
    assert.ok(out.every((v) => v === 0));
  });
  r.deliver();
  r.produce(TARGET - 1);
  check('start: under a setpoint queued, silence and nothing read', () => {
    const { out } = r.render();
    assert.ok(out.every((v) => v === 0));
    assert.equal(r.h[1], 0);
    assert.equal(r.h[3], TARGET);
    assert.equal(r.stats[5], 0);
  });
  r.produce(1 + 500);
  r.setExpect(500); // the newest setpoint's worth starts at produced frame 500
  r.play();
  check('start: plays from the newest setpoint, no loss counted', () => {
    assert.deepEqual(clean(r.events), []);
    assert.equal(r.stats[5], 1);
    assert.equal(r.h[1], 500 + WANT);
    assert.deepEqual(r.loss(), { underruns: 0, dropped: 0 });
  });
  r.run(3 * SETTLE_QUANTA);
  check('steady: gapless across many ring wraps, no loss, queue on the setpoint', () => {
    assert.deepEqual(clean(r.events), []);
    assert.deepEqual(r.loss(), { underruns: 0, dropped: 0 });
    assert.equal(r.queue(), TARGET - WANT);
    assert.equal(r.h[3], TARGET);
    assert.equal(r.h[4], r.stats[0]);
    assert.equal(r.h[1], (r.h[0] - (TARGET - WANT)) >>> 0);
  });
}

// ── settle ─────────────────────────────────────────────────────────────────────────────────────
function steady(base = 0) {
  const r = rig(base);
  r.deliver();
  r.produce(TARGET);
  r.play();
  r.run(SETTLE_QUANTA + 10);
  return r;
}
{
  const r = steady();
  r.h[7]++;
  r.produce(480);
  r.run(SETTLE_QUANTA / 2);
  check('settle: the level holds the setpoint while a long queue is measured', () => {
    assert.equal(r.h[3], TARGET);
    assert.deepEqual(r.loss(), { underruns: 0, dropped: 0 });
  });
  r.run(SETTLE_QUANTA);
  check('settle: a queue long by 480 drops 480 once, then sits on the setpoint', () => {
    assert.deepEqual(clean(r.events), [{ kind: 'jump', by: 480 }]);
    assert.deepEqual(r.loss(), { underruns: 0, dropped: 480 });
    assert.equal(r.queue(), TARGET - WANT);
  });
}
{
  const r = steady();
  r.run(3, 0); // the producer pauses three quanta, then marks the step as it resumes
  r.h[7]++;
  r.run(2 * SETTLE_QUANTA);
  r.closeEvents();
  check('settle: a queue short by 384 inserts 384 of silence once, counted as an underrun', () => {
    assert.deepEqual(clean(r.events), [{ kind: 'silence', frames: 384 }]);
    assert.deepEqual(r.loss(), { underruns: 1, dropped: 0 });
    assert.equal(r.queue(), TARGET - WANT);
  });
}
{
  const r = steady();
  r.h[7]++;
  r.produce(TOLERANCE - 1);
  r.run(2 * SETTLE_QUANTA);
  check('settle: a step inside the tolerance moves and counts nothing', () => {
    assert.deepEqual(clean(r.events), []);
    assert.deepEqual(r.loss(), { underruns: 0, dropped: 0 });
  });
}

// ── underrun ───────────────────────────────────────────────────────────────────────────────────
{
  const r = steady();
  r.run(11, 0); // 1312 queued after a quantum: ten full quanta, then 32 frames and 96 of silence
  r.run(5, 0);
  check('underrun: a starved quantum zero-fills and counts once, then waits in silence', () => {
    assert.deepEqual(r.loss(), { underruns: 1, dropped: 0 });
    assert.equal(r.queue(), 0);
  });
  r.run(12); // 1536 produced: the queue is back past the setpoint, playing resumes from its newest
  r.closeEvents();
  check('underrun: playing resumes on the setpoint as soon as it refills', () => {
    const events = clean(r.events);
    assert.equal(events.length, 2);
    assert.equal(events[0].kind, 'silence');
    assert.deepEqual(events[1], { kind: 'jump', by: 12 * WANT - TARGET });
    assert.equal(r.queue(), TARGET - WANT);
  });
  r.run(2 * SETTLE_QUANTA);
  check('underrun: its settle finds the setpoint and moves nothing more', () => {
    assert.equal(clean(r.events).length, 2);
    assert.deepEqual(r.loss(), { underruns: 1, dropped: 0 });
  });
}

// A producer too jittery to keep a quantum ahead: the queue saws between ~0 and a block past the setpoint.
{
  const r = steady();
  r.run(11, 0);
  let starved = 0;
  for (let i = 0; i < 2 * SETTLE_QUANTA; i++) {
    const before = r.stats[1];
    r.run(1, i % 4 === 0 ? 4 * WANT : 0);
    if (r.stats[5] === 1 && r.stats[1] !== before && r.queue() === 0) starved++;
  }
  check('underrun: a bursty producer after an underrun never starves again', () => {
    assert.equal(starved, 0);
    assert.equal(r.stats[2], 0);
  });
}

// ── lag cap ────────────────────────────────────────────────────────────────────────────────────
{
  const r = steady();
  r.produce(MAX_LAG);
  r.run(1);
  const expected = MAX_LAG + WANT - TARGET + (TARGET - WANT); // everything past the setpoint
  check('lag cap: a backlog past maxLag drops to the setpoint at once, counted', () => {
    assert.deepEqual(clean(r.events), [{ kind: 'jump', by: expected }]);
    assert.deepEqual(r.loss(), { underruns: 0, dropped: expected });
    assert.equal(r.queue(), TARGET - WANT);
  });
  r.run(2 * SETTLE_QUANTA);
  check('lag cap: its settle finds the setpoint and moves nothing more', () => {
    assert.deepEqual(r.loss(), { underruns: 0, dropped: expected });
  });
}

// ── level ──────────────────────────────────────────────────────────────────────────────────────
{
  const r = steady();
  r.produce(100); // a drift-like offset, no epoch
  r.run(SETTLE_QUANTA);
  check('level: without a step it follows the queue and nothing moves', () => {
    assert.ok(r.h[3] >= TARGET + 90 && r.h[3] <= TARGET + 100, `level ${r.h[3]}`);
    assert.deepEqual(clean(r.events), []);
    assert.deepEqual(r.loss(), { underruns: 0, dropped: 0 });
  });
}

// ── u32 cursor wrap ────────────────────────────────────────────────────────────────────────────
{
  const r = steady(2 ** 32 - 3 * SR);
  r.run(4 * SETTLE_QUANTA);
  check('u32 wrap: the cursors wrap with the audio gapless', () => {
    assert.ok(r.h[0] < 2 ** 31, 'the write cursor wrapped');
    assert.deepEqual(clean(r.events), []);
    assert.deepEqual(r.loss(), { underruns: 0, dropped: 0 });
    assert.equal(r.queue(), TARGET - WANT);
  });
}

// ── close ──────────────────────────────────────────────────────────────────────────────────────
{
  const r = steady();
  r.processor.port.onmessage({ data: 'close' });
  check('close: the processor ends', () => assert.equal(r.render().alive, false));
}

Float32Array.prototype.subarray = subarray;
check('process() never created a typed-array view', () => assert.equal(views, 0));

console.log(`\n=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);
