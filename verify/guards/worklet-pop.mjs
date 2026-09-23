// verify/guards/worklet-pop.mjs — executes the REAL plugin-pcm-source.ts process() in Node with a worklet-global
// shim and real ringbuf.js SABs (the capture-processor idiom of verify/guards/capture-packets.mjs).
//
// What this PROVES: for every pop count from an empty ring (underrun) through every partial pop to a full
// quantum, process() writes exactly the popped frames to its output, zero-fills the [got, 128) shortfall,
// counts consumed frames and underruns in the stats SAB, never creates a typed-array view (invariant 5:
// the partial branch copies by index), and keeps the ring's FIFO order across calls.

import assert from 'node:assert/strict';
import { RingBuffer } from 'ringbuf.js';

const WANT = 128;
let Processor;
globalThis.AudioWorkletProcessor = class {};
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

function fixture() {
  const ringSab = RingBuffer.getStorageForCapacity(4 * WANT, Float32Array);
  const statsSab = new SharedArrayBuffer(8);
  return {
    processor: new Processor({ processorOptions: { ringSab, statsSab } }),
    ring: new RingBuffer(ringSab, Float32Array),
    stats: new Int32Array(statsSab),
  };
}
/** Distinct, non-zero, signed samples so a wrong index shows up. */
const sample = (i) => ((i % 2 === 0 ? 1 : -1) * (i + 1)) / 1024;

/** Run process() with Float32Array view creation trapped; returns the output and whether a view was made. */
function run(processor) {
  const out = new Float32Array(WANT).fill(7); // pre-dirty to catch un-written slots
  const subarray = Float32Array.prototype.subarray;
  let views = 0;
  Float32Array.prototype.subarray = function (...args) { views++; return subarray.apply(this, args); };
  try { processor.process([], [[out]]); } finally { Float32Array.prototype.subarray = subarray; }
  return { out, views };
}

for (let got = 0; got <= WANT; got++) {
  const { processor, ring, stats } = fixture();
  if (got > 0) ring.push(Float32Array.from({ length: got }, (_, i) => sample(i)));
  const { out, views } = run(processor);
  check(`got=${got}: the popped frames are copied in order`, () => {
    for (let i = 0; i < got; i++) assert.equal(out[i], sample(i), `index ${i}`);
  });
  check(`got=${got}: the shortfall is zero-filled`, () => {
    for (let i = got; i < WANT; i++) assert.equal(out[i], 0, `index ${i}`);
  });
  check(`got=${got}: consumed and underrun counters`, () => {
    assert.equal(stats[0], got);
    assert.equal(stats[1], got < WANT ? 1 : 0);
  });
  check(`got=${got}: no typed-array view is created`, () => assert.equal(views, 0));
  check(`got=${got}: the ring is drained`, () => assert.equal(ring.available_read(), 0));
}

// FIFO across calls: 2.5 quanta arrive at once, then three quanta render.
{
  const { processor, ring, stats } = fixture();
  const total = 2 * WANT + WANT / 2;
  ring.push(Float32Array.from({ length: total }, (_, i) => sample(i)));
  const outs = [run(processor).out, run(processor).out, run(processor).out];
  check('FIFO: three quanta play the pushed frames in order, then silence', () => {
    for (let k = 0; k < 3 * WANT; k++) {
      assert.equal(outs[Math.floor(k / WANT)][k % WANT], k < total ? sample(k) : 0, `frame ${k}`);
    }
  });
  check('FIFO: counters after the burst', () => {
    assert.equal(stats[0], total);
    assert.equal(stats[1], 1);
  });
}

// A missing output (the node disconnected) leaves the ring and counters alone.
{
  const { processor, ring, stats } = fixture();
  ring.push(new Float32Array(WANT).fill(0.5));
  check('no output: keeps running without popping', () => {
    assert.equal(processor.process([], [[]]), true);
    assert.equal(ring.available_read(), WANT);
    assert.equal(stats[0], 0);
    assert.equal(stats[1], 0);
  });
}

console.log(`\n=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);
