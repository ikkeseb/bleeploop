// verify/guards/share-target.mjs — deterministic guard for src/ui/settings/share-target.ts.
//
// Imports the REAL source (Node TS type-stripping), NOT a port. `sharesInterface` decides whether the
// Audio Settings popover warns that Share output targets the interface ASIO plays on (tester report
// F22: the master heard twice). Covers the driver-vs-endpoint name pairs the warning must catch
// (Focusrite, a "2-" Windows suffix, Steinberg/Yamaha, Realtek, Behringer UMC), the pairs it must stay
// quiet on (a virtual cable, ASIO4ALL wrapping anything, a virtual ASIO driver, a different brand,
// empty names) and that matching is by word, not substring. Run: node verify/guards/share-target.mjs

import assert from 'node:assert';

const { sharesInterface } = await import('../../src/ui/settings/share-target.ts');

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

const CASES = [
  ['Focusrite USB ASIO', 'Speakers (Focusrite USB Audio)', true],
  ['Focusrite USB ASIO', 'Speakers (2- Focusrite USB Audio)', true],
  ['Yamaha Steinberg USB ASIO', 'Line (Steinberg UR22C)', true],
  ['Realtek ASIO', 'Speakers (Realtek(R) Audio)', true],
  ['UMC ASIO Driver', 'Speakers (Behringer UMC 204HD)', true],
  ['Focusrite USB ASIO', 'CABLE Input (VB-Audio Virtual Cable)', false],
  ['ASIO4ALL v2', 'Speakers (Realtek(R) Audio)', false],
  ['Voicemeeter Insert Virtual ASIO', 'Voicemeeter Input (VB-Audio Voicemeeter VAIO)', false],
  ['Focusrite USB ASIO', 'Speakers (Realtek(R) Audio)', false],
  ['', 'Speakers (Realtek(R) Audio)', false],
  ['Focusrite USB ASIO', '', false],
  ['', '', false],
];
for (const [asio, share, expected] of CASES) {
  check(`"${asio}" vs "${share}" -> ${expected}`, () => assert.strictEqual(sharesInterface(asio, share), expected));
}

// Words, not substrings: "ESI" (a brand) inside "Genesis" must not count.
check('a driver word inside a longer endpoint word does not match', () =>
  assert.strictEqual(sharesInterface('ESI ASIO', 'Speakers (Genesis USB Audio)'), false));
// Generic words alone never match: two unrelated USB audio devices share "usb" and "audio".
check('generic words alone do not match', () =>
  assert.strictEqual(sharesInterface('Focusrite USB ASIO', 'Speakers (Realtek USB Audio)'), false));
// Case and punctuation are not what distinguishes a device.
check('case and punctuation are ignored', () =>
  assert.strictEqual(sharesInterface('FOCUSRITE usb asio', 'Speakers (focusrite-usb-audio)'), true));

console.log(`\n=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);
