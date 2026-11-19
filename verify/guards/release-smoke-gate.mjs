// verify/guards/release-smoke-gate.mjs — guard for the release's Windows-smoke gate: a tag publishes
// only when `pnpm release:smoke` passed on that very commit (scripts/release-smoke-rule.mjs).
//
// Imports the REAL rule and reads the REAL .github/workflows/build-exe.yml. Covers: the rule passes only
// when the newest `release-smoke` status is a success, whatever order the list comes in; a later
// failure or pending status cancels an earlier pass; other contexts, no status, an unreadable list or
// time fail closed, as does any status in the context whose time cannot be read; ties on time go to the
// higher id; a success passes only as the smoke's `PASS …` or an `owner waiver: …` with its reason; the
// paginated API answer flattens only when every page is a list. The workflow: `release` needs
// `smoke-verdict` and requires its success outside every `||`; `smoke-verdict` runs on a tag push,
// runs after `exe`, reads the statuses for this commit through the verdict script on an unconditional
// step with its token and repo, may read statuses and cannot be skipped past with continue-on-error.
// Cannot see: GitHub's answer (the API call and the runner's token) or the smoke's own post
// (scripts/release-smoke.mjs runs on Windows with the rig); a tag's run and a smoke run exercise those.
// Run: node verify/guards/release-smoke-gate.mjs

import assert from 'node:assert';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { SMOKE_CONTEXT, smokeVerdict, statusesOf } from '../../scripts/release-smoke-rule.mjs';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
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

// ── 1. The rule ─────────────────────────────────────────────────────────────────────────────────────
let id = 0;
const status = (state, at, context = SMOKE_CONTEXT, description = 'PASS 8/8 with the input take') => ({
  id: ++id,
  context,
  state,
  description,
  created_at: `2026-11-19T0${at}:00:00Z`,
});

for (const [name, list, ok] of [
  ['a pass on the commit passes', [status('success', 1)], true],
  ['an owner waiver passes', [status('success', 1, SMOKE_CONTEXT, 'owner waiver: no input take')], true],
  ['no status fails', [], false],
  ['only another context\'s success fails', [status('success', 1, 'ci')], false],
  ['a failure fails', [status('failure', 1)], false],
  ['a pending status fails', [status('pending', 1)], false],
  ['an error fails', [status('error', 1)], false],
  ['a failure after a pass fails', [status('failure', 2), status('success', 1)], false],
  ['a failure after a pass fails in either order', [status('success', 1), status('failure', 2)], false],
  ['a pass after a failure passes', [status('failure', 1), status('success', 2)], true],
  ['a pass after a failure passes in either order', [status('success', 2), status('failure', 1)], true],
  ['another context\'s later failure does not cancel a pass', [status('failure', 2, 'ci'), status('success', 1)], true],
  ['a newest status with no time fails', [{ ...status('success', 1), created_at: undefined }], false],
  ['an undated failure beside a dated pass fails', [{ ...status('failure', 2), created_at: 'soon' }, status('success', 1)], false],
  ['an undated failure beside a dated pass fails in either order', [status('success', 1), { ...status('failure', 2), created_at: null }], false],
  ['a same-time failure with the higher id fails', [status('success', 1), status('failure', 1)], false],
  ['a same-time failure with the higher id fails in either order', [status('success', 1), status('failure', 1)].reverse(), false],
  ['a same-time pass with the higher id passes', [status('failure', 1), status('success', 1)], true],
  ['a success with no description fails', [status('success', 1, SMOKE_CONTEXT, '')], false],
  ['a success with another description fails', [status('success', 1, SMOKE_CONTEXT, 'looks fine')], false],
  ['a waiver with no reason fails', [status('success', 1, SMOKE_CONTEXT, 'owner waiver: ')], false],
  ['null entries are skipped', [null, status('success', 1)], true],
  ['an unreadable list fails', null, false],
  ['an object instead of a list fails', { context: SMOKE_CONTEXT, state: 'success' }, false],
]) {
  check(name, () => {
    const v = smokeVerdict(list);
    assert.strictEqual(v.ok, ok, v.why);
    assert.strictEqual(typeof v.why, 'string');
  });
}
check('a pass says when and what passed', () =>
  assert.match(smokeVerdict([status('success', 1)]).why, /release-smoke passed on this commit at 2026-11-19T01:00:00Z: PASS 8\/8/));
check('statusesOf flattens pages of lists', () => assert.deepStrictEqual(statusesOf([[1, 2], [], [3]]), [1, 2, 3]));
check('statusesOf takes an empty answer as no status', () => assert.deepStrictEqual(statusesOf([[]]), []));
for (const [name, pages] of [
  ['a page that is no list', [[status('success', 1)], { message: 'oops' }]],
  ['an answer that is no list', { message: 'Not Found' }],
  ['no answer', null],
]) check(`statusesOf refuses ${name}`, () => assert.strictEqual(statusesOf(pages), null));
check('a refusal names the newest state', () =>
  assert.match(smokeVerdict([status('failure', 2), status('success', 1)]).why, /newest release-smoke status is "failure"/));

// ── 2. The workflow ─────────────────────────────────────────────────────────────────────────────────
const WORKFLOW = '.github/workflows/build-exe.yml';
const yaml = readFileSync(join(root, WORKFLOW), 'utf8');

/** Each job's block under `jobs:`, comments dropped, by job id. */
function jobsOf(text) {
  const out = {};
  let inJobs = false;
  let job = null;
  for (const line of text.split('\n')) {
    if (/^\S/.test(line)) [inJobs, job] = [/^jobs:\s*$/.test(line), null];
    const j = inJobs && line.match(/^ {2}([\w-]+):\s*$/);
    if (j) out[(job = j[1])] = '';
    else if (job && !/^\s*#/.test(line)) out[job] += `${line}\n`;
  }
  return out;
}
/** The `if:` expression of a job block (a `>-` folded block or one line), whitespace collapsed. */
function ifOf(block) {
  const m = block.match(/^ {4}if:\s*>-?\n((?: {6}.*\n)+)/m) ?? block.match(/^ {4}if:\s*(.+)\n/m);
  return m ? m[1].replace(/\s+/g, ' ').trim() : null;
}
/** Whether `term` appears in `expr` outside every parenthesis, so no `||` inside a group can bypass it. */
function topLevel(expr, term) {
  const inner = expr.replace(/^\$\{\{\s*|\s*\}\}$/g, '');
  let depth = 0;
  let found = false;
  for (let i = 0; i < inner.length; i++) {
    if (inner[i] === '(') depth++;
    else if (inner[i] === ')') depth--;
    else if (depth === 0 && inner.startsWith('||', i)) return false;
    else if (depth === 0 && inner.startsWith(term, i)) found = true;
  }
  return found;
}

const jobs = jobsOf(yaml);
const release = jobs.release ?? '';
const smoke = jobs['smoke-verdict'] ?? '';
check('build-exe.yml has a release job', () => assert.ok(release, `no release job in ${WORKFLOW}`));
check('release needs smoke-verdict', () => {
  const needs = release.match(/^ {4}needs:\s*\[(.*)\]\s*$/m)?.[1].split(',').map((s) => s.trim());
  assert.ok(needs?.includes('smoke-verdict'), `release needs ${JSON.stringify(needs)}`);
});
check('release requires smoke-verdict to have succeeded, outside every ||', () => {
  const cond = ifOf(release);
  assert.ok(cond, 'release has no if:');
  assert.ok(topLevel(cond, "needs.smoke-verdict.result == 'success'"), cond);
});
check('smoke-verdict exists', () => assert.ok(smoke, `no smoke-verdict job in ${WORKFLOW}`));
check('smoke-verdict runs on a tag push', () => assert.strictEqual(ifOf(smoke), "github.event_name == 'push'"));
check('smoke-verdict reads after the build', () => assert.match(smoke, /^ {4}needs:\s*\[?\s*exe\s*\]?\s*$/m));
check('no step of smoke-verdict carries a condition', () => assert.doesNotMatch(smoke, /^ {6}(- )? *if:/m));
check('smoke-verdict hands the script its token and repo', () => {
  assert.match(smoke, /^ {10}GH_TOKEN: \$\{\{ github\.token \}\}\s*$/m);
  assert.match(smoke, /^ {10}GH_REPO: \$\{\{ github\.repository \}\}\s*$/m);
});
check('smoke-verdict asks about this commit through the verdict script', () =>
  assert.match(smoke, /^ {8}run: node scripts\/release-smoke-verdict\.mjs "\$\{\{ github\.sha \}\}"\s*$/m));
check('smoke-verdict may read statuses', () => assert.match(smoke, /^ {6}statuses: read\s*$/m));
check('smoke-verdict cannot pass by continue-on-error', () => assert.doesNotMatch(smoke, /continue-on-error/));

// ── 3. The checks above, on planted workflows ───────────────────────────────────────────────────────
check('topLevel refuses the term inside an || group', () =>
  assert.ok(!topLevel("${{ a && (b || needs.smoke-verdict.result == 'success') }}", "needs.smoke-verdict.result == 'success'")));
check('topLevel refuses a top-level || beside the term', () =>
  assert.ok(!topLevel("${{ needs.smoke-verdict.result == 'success' || always() }}", "needs.smoke-verdict.result == 'success'")));
check('topLevel takes the term beside parenthesized ||', () =>
  assert.ok(topLevel("${{ a && (b || c) && needs.smoke-verdict.result == 'success' }}", "needs.smoke-verdict.result == 'success'")));
check('jobsOf drops comments and splits jobs', () =>
  assert.deepStrictEqual(jobsOf('jobs:\n  a:\n    # needs: [x]\n    needs: [y]\n  b-c:\n    if: z\nother:\n  d:\n'), {
    a: '    needs: [y]\n',
    'b-c': '    if: z\n',
  }));

console.log(`=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);
