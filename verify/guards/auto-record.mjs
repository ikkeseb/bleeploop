#!/usr/bin/env node

import {
  AUTO_RECORD_DEFAULT_SENSITIVITY,
  AutoRecordDetector,
  autoRecordThreshold,
} from '../../src/audio/looper/auto-record.ts';

let passed = 0;
let failed = 0;

function check(name, condition, detail = '') {
  if (condition) {
    passed++;
    console.log(`  ok   ${name}${detail ? `  ${detail}` : ''}`);
  } else {
    failed++;
    console.log(`  FAIL ${name}${detail ? `  ${detail}` : ''}`);
  }
}

function approx(a, b, epsilon = 1e-7) {
  return Math.abs(a - b) <= epsilon;
}

function exactPrefix(actual, expected) {
  if (actual.length !== expected.length) return false;
  for (let i = 0; i < actual.length; i++) {
    if (actual[i] !== expected[i]) return false;
  }
  return true;
}

console.log('=== A. sensitivity maps monotonically onto a useful dBFS range ===');
{
  const least = autoRecordThreshold(1);
  const middle = autoRecordThreshold(AUTO_RECORD_DEFAULT_SENSITIVITY);
  const most = autoRecordThreshold(100);
  check('A least-sensitive endpoint is -12 dBFS', approx(least, 10 ** (-12 / 20)));
  check('A most-sensitive endpoint is -60 dBFS', approx(most, 10 ** (-60 / 20)));
  check('A default sits strictly between the endpoints', most < middle && middle < least);
  check('A sensitivity clamps below 1', autoRecordThreshold(-20) === least);
  check('A sensitivity clamps above 100', autoRecordThreshold(140) === most);
}

console.log('\n=== B. silence and sub-threshold sound do not trigger ===');
{
  const detector = new AutoRecordDetector(1000); // 4-frame RMS window, 40-frame history
  const out = new Float32Array(100);
  const quiet = new Float32Array(30).fill(0.01);
  check('B quiet batch stays armed', detector.scan(quiet, quiet.length, 0.2, out) === -1);
  check('B genuinely quiet history can advance integrity baselines', detector.historyIsQuiet(0.2));

  const soft = new Float32Array(20).fill(0.05);
  check('B sub-trigger sound stays armed', detector.scan(soft, soft.length, 0.2, out) === -1);
  check('B possible onset holds the integrity baseline', !detector.historyIsQuiet(0.2));
  check('B no frames were copied before a trigger', detector.copiedFrames() === 0);
}

console.log('\n=== C. trigger recovers the soft onset and appends the batch exactly once ===');
{
  const detector = new AutoRecordDetector(1000);
  const out = new Float32Array(100);
  const data = new Float32Array([
    ...new Array(20).fill(0),
    ...new Array(8).fill(0.05),
    ...new Array(8).fill(0.2),
  ]);
  const offset = detector.scan(data, data.length, 0.2, out);
  const copied = detector.copiedFrames();
  const combined = new Float32Array(copied + data.length - offset);
  combined.set(out.subarray(0, copied), 0);
  combined.set(data.subarray(offset), copied);
  check('C four loud frames satisfy the 4 ms RMS window', offset === 32, `offset=${offset}`);
  check('C look-back starts one analysis block before the soft onset', copied === 16, `copied=${copied}`);
  check('C kept audio is one contiguous source slice with no duplicate trigger sample', exactPrefix(combined, data.slice(16)));
}

console.log('\n=== D. a batch boundary cannot hide or duplicate the onset ===');
{
  const detector = new AutoRecordDetector(1000);
  const out = new Float32Array(100);
  const before = new Float32Array([
    ...new Array(20).fill(0),
    ...new Array(8).fill(0.05),
  ]);
  const crossing = new Float32Array(8).fill(0.2);
  check('D first batch stays armed', detector.scan(before, before.length, 0.2, out) === -1);
  const offset = detector.scan(crossing, crossing.length, 0.2, out);
  const copied = detector.copiedFrames();
  const combined = new Float32Array(copied + crossing.length - offset);
  combined.set(out.subarray(0, copied), 0);
  combined.set(crossing.subarray(offset), copied);
  const stream = new Float32Array(before.length + crossing.length);
  stream.set(before);
  stream.set(crossing, before.length);
  check('D trigger offset is relative to the crossing batch', offset === 4, `offset=${offset}`);
  check('D retained audio is still the exact contiguous stream suffix', exactPrefix(combined, stream.slice(16)));
}

console.log('\n=== E. reset and history wrap cannot leak an older arm into a new take ===');
{
  const detector = new AutoRecordDetector(1000);
  const out = new Float32Array(100);
  detector.scan(new Float32Array(35).fill(0.04), 35, 0.2, out);
  detector.reset();
  const fresh = new Float32Array(4).fill(0.2);
  const freshOffset = detector.scan(fresh, fresh.length, 0.2, out);
  check('E fresh arm triggers after its own RMS window', freshOffset === 4);
  check('E reset removed every older sample', detector.copiedFrames() === 4 && exactPrefix(out.slice(0, 4), fresh));

  detector.reset();
  detector.scan(new Float32Array(50), 50, 0.2, out); // wrap the 40-frame history
  const tail = new Float32Array([...new Array(8).fill(0.05), ...new Array(4).fill(0.2)]);
  const wrappedOffset = detector.scan(tail, tail.length, 0.2, out);
  check('E wrapped history still triggers', wrappedOffset === 12);
  check('E wrapped history retains only the relevant 16-frame onset', detector.copiedFrames() === 16);
}

console.log(`\n=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);
