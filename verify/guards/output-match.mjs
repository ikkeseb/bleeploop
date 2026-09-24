// verify/guards/output-match.mjs — deterministic guard for src/audio/output-match.ts.
//
// Imports the REAL source (Node TS type-stripping), not a port, so it cannot drift.
// Run: node verify/guards/output-match.mjs
//
// The Audio Settings output pick is a cpal name; the WebView sink is found by matching it against
// Chromium's labels. Covers the label shapes WebView2 produces on Windows: a vendor-driver endpoint as is
// (the dev rig's Focusrite), a USB-class endpoint with ` (vid:pid)` (a tester's SteelSeries headset), a
// Bluetooth one, the default/communications aliases, a longer label winning over its prefix, and the
// label-less list a session without a mic grant sees.

import assert from 'node:assert';
import { matchWebOutput, outputLabelsHidden } from '../../src/audio/output-match.ts';

let passed = 0;
let failed = 0;

function check(name, fn) {
  try {
    fn();
    passed++;
  } catch (e) {
    failed++;
    console.error(`FAIL: ${name}\n  ${e.message}`);
  }
}

const out = (deviceId, label) => ({ kind: 'audiooutput', deviceId, label });
const LIST = [
  { kind: 'audioinput', deviceId: 'mic1', label: 'Headset Microphone (SteelSeries Arctis Nova 5) (1038:2232)' },
  out('default', 'Default - Headphones (SteelSeries Arctis Nova 5) (1038:2232)'),
  out('communications', 'Communications - Headphones (SteelSeries Arctis Nova 5) (1038:2232)'),
  out('arctis', 'Headphones (SteelSeries Arctis Nova 5) (1038:2232)'),
  out('focusrite', 'Speakers (2- Focusrite USB Audio)'),
  out('bt', 'Headphones (WH-1000XM4) (Bluetooth)'),
  out('mon', 'Speakers (Monitor)'),
  out('mon2', 'Speakers (Monitor) 2'),
];

check('USB-class label: the (vid:pid) suffix is not part of the name', () =>
  assert.strictEqual(matchWebOutput('Headphones (SteelSeries Arctis Nova 5) [Headphones] via USB', LIST), 'arctis'));
check('vendor-driver label matches as is', () =>
  assert.strictEqual(matchWebOutput('Speakers (2- Focusrite USB Audio) [Speakers] via USB', LIST), 'focusrite'));
check('Bluetooth label: the (Bluetooth) suffix is not part of the name', () =>
  assert.strictEqual(matchWebOutput('Headphones (WH-1000XM4) [Headphones] via Bluetooth', LIST), 'bt'));
check('exact name matches', () => assert.strictEqual(matchWebOutput('Speakers (Monitor)', LIST), 'mon'));
check('the longer label wins over its prefix', () =>
  assert.strictEqual(matchWebOutput('Speakers (Monitor) 2 [Speakers] via HDMI', LIST), 'mon2'));
check('default/communications aliases and inputs never match', () =>
  assert.strictEqual(
    matchWebOutput('Headphones (SteelSeries Arctis Nova 5) [Headphones] via USB', LIST.filter((d) => d.deviceId !== 'arctis')),
    undefined,
  ));
check('a label that only shares a prefix without a space does not match', () =>
  assert.strictEqual(matchWebOutput('Speakers (Monitor)X', LIST), undefined));
const HIDDEN = [{ kind: 'audioinput', deviceId: '', label: '' }, out('', '')];
check('no labels: nothing matches', () => assert.strictEqual(matchWebOutput('Speakers (Monitor)', HIDDEN), undefined));
check('no labels: hidden', () => assert.strictEqual(outputLabelsHidden(HIDDEN), true));
check('labelled list: not hidden', () => assert.strictEqual(outputLabelsHidden(LIST), false));
check('only input labels: still hidden', () =>
  assert.strictEqual(outputLabelsHidden([LIST[0], out('', '')]), true));

console.log(`\n=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);
