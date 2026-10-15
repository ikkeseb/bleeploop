// verify/guards/fixed-bars.mjs — FIXED's bar math, the UI's half of a parity check with the engine.
//
// The UI computes the engine's FIXED rule itself, to bound the bar selector (`engineLooper.nextTakeMaxBars`,
// the engine's `Looper::next_take_max_bars`) and to draw a later take's window (`laterTakeBars`, the
// engine's `grid::later_take_bars`; the span the record head sweeps, `engineLooper.recSpanFrames`).
// `verify/fixtures/fixed-bars.json` holds rows of (rate, BPM, master, FIXED request) and what both must
// answer; `src-tauri/crates/lf-engine/tests/fixed_bars.rs` loads each row's master into the engine and
// holds the engine to it. Here the REAL `src/ui/state/engine-store.ts`, under the verify hooks, reads each
// row from the feed as the engine sends it (the device's rate, the transport, FIXED on, a take that
// starts) through the browser build's engine fake, and must answer the same. A row neither side can
// reach is not here: the engine loads no master of a fractional bar count, so the UI's branch for one
// has no engine twin to hold it to.
// Run: node verify/guards/fixed-bars.mjs

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import '../harness/hooks.ts';

const fixture = JSON.parse(readFileSync(new URL('../fixtures/fixed-bars.json', import.meta.url), 'utf8'));

const stored = new Map();
globalThis.localStorage = {
  getItem: (k) => (stored.has(k) ? stored.get(k) : null),
  setItem: (k, v) => stored.set(k, String(v)),
};

let passed = 0;
let failed = 0;
async function check(name, fn) {
  try {
    await fn();
    passed++;
  } catch (e) {
    failed++;
    console.error(`FAIL: ${name}\n  ${e.message}`);
  }
}

let generation = 0;
/** A fresh store, subscribed to a fresh engine fake. */
async function freshStore() {
  const g = ++generation;
  const store = await import(`../../src/ui/state/engine-store.ts?g=${g}`);
  const { webEngineFake } = await import(`../../src/platform/host.web.ts?g=${g}`);
  store.startEngineStore();
  let seq = 0;
  const emit = (events, status = null) =>
    webEngineFake.emit({ seq: ++seq, reset: false, events, device: [], status, anchor: null, meter: null, peaks: [] });
  return { store, emit };
}

const status = (rate) => ({
  backend: 'Asio',
  sampleRate: rate,
  block: 128,
  inputName: 'In',
  outputName: 'Out',
  inputOpen: true,
  inputChannels: [1, 1],
  alignFrames: 0,
  inputFrames: 0,
});

const recording = {
  state: 'Recording',
  length: 0,
  armed: false,
  autoArmed: false,
  canUndo: false,
  canReverse: false,
  reversed: false,
  stopAt: null,
  fading: false,
  retakePass: 0,
};

await check('the fixture has its rows', () => {
  assert.ok(fixture.rows.length >= 8);
  assert.equal(fixture.laneSeconds, 60, 'the host lane buffer the UI bounds a multiply by');
});

for (const row of fixture.rows) {
  const tag = `${row.rate} Hz, ${row.bpm} BPM, a ${row.bars}-bar master, FIXED ${row.requested}`;
  const { store, emit } = await freshStore();
  const { engineLooper, laterTakeBars } = store;
  emit(
    [
      { Transport: { frame: 0, master: row.master, bpm: row.bpm, locked: true } },
      { Toggled: { frame: 0, toggle: 'Fixed', on: true } },
    ],
    status(row.rate),
  );
  engineLooper.setFixedLengthBars(row.requested);
  await check(`${tag}: the selector's bound`, () => assert.equal(engineLooper.nextTakeMaxBars(), row.maxBars));
  await check(`${tag}: the bars a later take records`, () =>
    assert.equal(laterTakeBars(row.requested, row.bars, row.maxBars), row.takeBars),
  );
  // A take that starts over the master: its record head sweeps the window FIXED armed, never less than
  // the loop (a shorter take repeats across it).
  emit([{ Lane: { frame: row.master, lane: 1, info: recording } }]);
  await check(`${tag}: the window the head sweeps`, () =>
    assert.equal(engineLooper.recSpanFrames(1), Math.max(row.master, row.takeFrames)),
  );
}

console.log(`\n=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);
