#!/usr/bin/env node
// verify/guards/auto-record.mjs — deterministic guard for the AUTO REC sensitivity scale in
// src/ui/state/auto-record.ts (the threshold the command-bar meter marks). The onset detector itself
// is the engine's (lf-engine `autorec.rs`, with its cargo tests).
// Run: node verify/guards/auto-record.mjs

import {
  AUTO_RECORD_DEFAULT_SENSITIVITY,
  autoRecordThreshold,
} from '../../src/ui/state/auto-record.ts';

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

console.log(`\n=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);
