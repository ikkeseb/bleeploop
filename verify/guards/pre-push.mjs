// verify/guards/pre-push.mjs: the hook checks every pushed ref against a clean HEAD and runs one
// gate. Imports the real decision and runner with injected subprocess results; no git or pnpm needed.
// Checks Windows spawn options, but cannot prove a Windows .cmd shim launches here. One check starts
// the script through a linked directory, as git may: its main block must still run.
// Run: node verify/guards/pre-push.mjs

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, symlinkSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { decidePush, runPrePush } from '../../scripts/pre-push.mjs';

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

const HEAD = 'a'.repeat(40);
const OTHER = 'b'.repeat(40);
const TAG = 'c'.repeat(40);
const ZERO = '0'.repeat(40);
const update = (sha = HEAD, ref = 'refs/heads/main') => `${ref} ${sha} ${ref} ${OTHER}\n`;
const deletion = update(ZERO, '(delete)');
const peel = (sha) => sha === TAG ? HEAD : sha;
const decide = (stdin, status = '', resolve = peel) => decidePush(stdin, HEAD, resolve, status);
function refused(result, detail) {
  assert.equal(result.action, 'refuse');
  assert.ok(result.message.includes(detail), result.message);
  assert.match(result.message, /clean checkout of its commit/);
}

check('configured hook runs the script and forwards remote arguments', () => {
  const pkg = JSON.parse(readFileSync(new URL('../../package.json', import.meta.url), 'utf8'));
  assert.equal(pkg['simple-git-hooks']['pre-push'], 'node scripts/pre-push.mjs "$@"');
});
check('the script run through a linked directory still runs its main block', () => {
  const dir = mkdtempSync(join(tmpdir(), 'pre-push-'));
  try {
    // A junction on Windows (no privilege needed); the type is ignored elsewhere.
    symlinkSync(fileURLToPath(new URL('../../scripts', import.meta.url)), join(dir, 'scripts'), 'junction');
    const run = spawnSync(process.execPath, [join(dir, 'scripts', 'pre-push.mjs')], { input: '', encoding: 'utf8' });
    assert.equal(run.status, 0, run.stderr);
    assert.match(run.stdout, /nothing is pushed that needs the gate/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
check('one ref at clean HEAD needs the gate', () => assert.equal(decide(update()).action, 'gate'));
check('another commit is refused with the ref and commit', () => {
  const result = decide(update(OTHER, 'refs/heads/other'));
  refused(result, 'refs/heads/other');
  assert.ok(result.message.includes(OTHER));
});
check('HEAD plus another commit is refused', () =>
  refused(decide(update() + update(OTHER, 'refs/heads/other')), OTHER));
for (const status of [' M tracked.txt\n', 'M  staged.txt\n', '?? untracked.txt\n']) {
  check(`dirty status ${status.trim()} is refused`, () => refused(decide(update(), status), status.trim()));
}
check('dirty status output is capped at five lines', () => {
  const status = Array.from({ length: 8 }, (_, i) => `?? file-${i}`).join('\n');
  const result = decide(update(), status);
  refused(result, '?? file-4');
  assert.ok(!result.message.includes('file-5'));
  assert.match(result.message, /3 more status lines/);
});
check('delete only needs nothing, even with a dirty tree', () =>
  assert.equal(decide(deletion, '?? untracked.txt\n', () => assert.fail('must not peel deletion')).action, 'nothing'));
check('delete plus HEAD needs one gate decision', () => {
  const peeled = [];
  assert.equal(decide(deletion + update(), '', (sha) => { peeled.push(sha); return sha; }).action, 'gate');
  assert.deepEqual(peeled, [HEAD]);
});
check('annotated tag peeling to HEAD needs the gate', () =>
  assert.equal(decide(update(TAG, 'refs/tags/v1')).action, 'gate'));
check('tag on a tree or blob is refused when commit peeling fails', () =>
  refused(decide(update(TAG, 'refs/tags/tree'), '', () => { throw new Error('not a commit'); }), 'refs/tags/tree'));
check('empty peel result is refused', () => refused(decide(update(), '', () => ''), HEAD));
check('empty stdin needs nothing', () => assert.equal(decide('').action, 'nothing'));
check('CRLF stdin parses the same as LF', () => {
  const stdin = deletion + update() + update(TAG, 'refs/tags/v1');
  assert.deepEqual(decide(stdin.replaceAll('\n', '\r\n')), decide(stdin));
});
check('malformed ref update is refused', () => refused(decide('refs/heads/main invalid\n'), 'invalid ref update'));

function exercise(stdin, { status = '', gitFailure, gateResult = { status: 0 } } = {}) {
  const calls = [];
  const result = runPrePush(stdin, (command, args, options) => {
    calls.push({ command, args, options });
    if (command === 'pnpm check') return gateResult;
    assert.equal(command, 'git');
    assert.deepEqual(options.stdio, ['ignore', 'pipe', 'inherit']);
    assert.equal(options.encoding, 'utf8');
    if (args[0] === gitFailure) return { status: 1, stdout: '' };
    if (args[0] === 'status') {
      assert.deepEqual(args, ['status', '--porcelain', '--untracked-files=all']);
      return { status: 0, stdout: status };
    }
    if (args.length === 2) {
      assert.deepEqual(args, ['rev-parse', 'HEAD']);
      return { status: 0, stdout: `${HEAD}\n` };
    }
    assert.deepEqual(args.slice(0, 2), ['rev-parse', '--verify']);
    assert.match(args[2], /^[a-f0-9]{40}\^\{commit\}$/);
    return { status: 0, stdout: `${peel(args[2].slice(0, 40))}\n` };
  });
  return { result, calls, gates: calls.filter((call) => call.command === 'pnpm check') };
}

check('delete plus multiple HEAD refs run pnpm check exactly once', () => {
  const { result, gates, calls } = exercise(deletion + update() + update(TAG, 'refs/tags/v1'));
  assert.equal(result.code, 0);
  assert.deepEqual(gates, [{ command: 'pnpm check', args: [], options: { stdio: 'inherit', shell: true } }]);
  assert.equal(calls.filter((call) => call.args[1] === '--verify').length, 2);
});
check('empty and delete-only pushes run no subprocess', () => {
  for (const stdin of ['', deletion]) {
    const { result, calls } = exercise(stdin);
    assert.equal(result.code, 0);
    assert.match(result.message, /nothing is pushed that needs the gate/);
    assert.deepEqual(calls, []);
  }
});
check('ref mismatch, dirty tree and git failures run no gate', () => {
  for (const [stdin, options] of [
    [update() + update(OTHER), {}],
    [update(), { status: '?? new.txt\n' }],
    [update(), { gitFailure: 'rev-parse' }],
    [update(), { gitFailure: 'status' }],
  ]) {
    const { result, gates } = exercise(stdin, options);
    assert.equal(result.code, 1);
    assert.match(result.message, /clean checkout/);
    assert.deepEqual(gates, []);
  }
});
check('gate exit code is preserved and spawn failure or signal refuses', () => {
  for (const [gateResult, code] of [
    [{ status: 7 }, 7],
    [{ status: null, signal: 'SIGTERM' }, 1],
    [{ status: null, error: new Error('ENOENT') }, 1],
  ]) {
    const { result, gates } = exercise(update(), { gateResult });
    assert.equal(result.code, code);
    assert.equal(gates.length, 1);
  }
});

console.log(`=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);
