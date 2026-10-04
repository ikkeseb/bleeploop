// verify/guards/release-exe-rule.mjs — guard for scripts/release-exe-rule.mjs, the rule that tells
// `pnpm release:smoke` whether the exe names the checkout's HEAD as its build commit.
//
// Imports the REAL module. Covers: runner, local and full commit tokens that prefix HEAD pass;
// surrounding whitespace is trimmed; another commit or HEAD's tail fails; "-dirty" and "unknown"
// fail; missing or malformed labels, invalid commit tokens and malformed HEAD fail closed.
// Cannot see: the CDP read of Help's build line or the git call that feeds the rule; only a smoke
// run exercises those.
// Run: node verify/guards/release-exe-rule.mjs

import assert from 'node:assert';
import { exeBuildRule } from '../../scripts/release-exe-rule.mjs';

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

const HEAD = '123abcde' + '0123456789abcdef'.repeat(2);
const label = (built) => `BleepLoop 0.6.0 · ${built}`;

for (const [name, text, built] of [
  ['a runner build\'s 7-hex prefix passes', label(HEAD.slice(0, 7)), HEAD.slice(0, 7)],
  ['a local build\'s 8-hex prefix passes', label(HEAD.slice(0, 8)), HEAD.slice(0, 8)],
  ['the full 40-hex commit passes', label(HEAD), HEAD],
  ['surrounding whitespace passes', ` \t${label(HEAD.slice(0, 8))}\r\n`, HEAD.slice(0, 8)],
]) {
  check(name, () => {
    const rule = exeBuildRule({ label: text, head: HEAD });
    assert.strictEqual(rule.ok, true);
    assert.strictEqual(rule.built, built);
    assert.strictEqual(rule.why, `built from ${built}, the checkout's HEAD`);
  });
}

for (const [name, built] of [
  ['another commit fails and names both commits', 'abcdef01'],
  ['a token matching HEAD\'s tail but not its start fails', HEAD.slice(-8)],
]) {
  check(name, () => {
    const rule = exeBuildRule({ label: label(built), head: HEAD });
    assert.strictEqual(rule.ok, false);
    assert.strictEqual(rule.built, built);
    assert.match(rule.why, new RegExp(`built from ${built}, the checkout is at ${HEAD.slice(0, built.length)}:`));
    assert.match(rule.why, /check out the commit the exe was built from, or build this one/);
  });
}

for (const [name, built, why] of [
  ['a dirty build fails although its sha matches HEAD', `${HEAD.slice(0, 8)}-dirty`, /-dirty, a build of a tree with uncommitted changes.*commit, then build again/],
  ['unknown fails', 'unknown', /built without git, so it names no commit/],
  ['a 6-hex token fails', HEAD.slice(0, 6), /which is no commit/],
  ['an uppercase-hex token fails', HEAD.slice(0, 8).toUpperCase(), /which is no commit/],
  ['a 41-hex token fails', `${HEAD}a`, /which is no commit/],
]) {
  check(name, () => {
    const rule = exeBuildRule({ label: label(built), head: HEAD });
    assert.strictEqual(rule.ok, false);
    assert.strictEqual(rule.built, built);
    assert.match(rule.why, why);
  });
}

for (const [name, text] of [
  ['a null label fails', null],
  ['an empty label fails', ''],
  ['a label with no commit fails', 'BleepLoop 0.6.0'],
  ['another product name fails', `OtherLoop 0.6.0 · ${HEAD.slice(0, 8)}`],
  ['a hyphen instead of the middle dot fails', `BleepLoop 0.6.0 - ${HEAD.slice(0, 8)}`],
]) {
  check(name, () => {
    const rule = exeBuildRule({ label: text, head: HEAD });
    assert.strictEqual(rule.ok, false);
    assert.strictEqual(rule.built, null);
    assert.match(rule.why, /^Help's build line /);
    if (text === null) assert.match(rule.why, /could not be read/);
    else assert.ok(rule.why.includes(JSON.stringify(text)), 'the reason quotes the label');
  });
}

for (const [name, head] of [
  ['a null HEAD fails', null],
  ['a 39-hex HEAD fails', HEAD.slice(0, 39)],
  ['a short-sha HEAD fails', HEAD.slice(0, 8)],
  ['an uppercase-hex HEAD fails', HEAD.toUpperCase()],
  ['a HEAD with a trailing newline fails', `${HEAD}\n`],
]) {
  check(name, () => {
    const built = HEAD.slice(0, 8);
    const rule = exeBuildRule({ label: label(built), head });
    assert.strictEqual(rule.ok, false);
    assert.strictEqual(rule.built, built);
    assert.strictEqual(rule.why, "git gave no answer about the checkout's HEAD");
  });
}

console.log(`=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);
