// verify/guards/ci-paths.mjs — rust-test.yml runs on every file its tests read. A Rust source under
// src-tauri/ may `include_str!`/`include_bytes!` a file outside src-tauri/ (the wire tests read
// verify/fixtures/engine-wire.json and midi-wire.json); a change to that file alone changes what the
// tests assert, so rust-test.yml's push and pull_request `paths` must match it, or the change lands
// with the Rust half of the test never run.
//
// `includesOf` and `covered` are first run on planted sources and path lists (each miss must be
// flagged, a covered one must pass), then on every `.rs` under src-tauri/ (its `target/` skipped) against
// the workflow's own `paths`. An include whose argument is not a string literal fails: the guard cannot
// resolve it. It reads the workflow's inline `paths: [...]` lists and GitHub's `*`, `**` and `!`
// patterns; any other form fails as unreadable. It cannot see a file a test opens at run time
// (`std::fs`), only what the compiler includes. Run: node verify/guards/ci-paths.mjs

import assert from 'node:assert';
import { readFileSync, readdirSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const TAURI = join(root, 'src-tauri');
const WORKFLOW = '.github/workflows/rust-test.yml';
const TRIGGERS = ['push', 'pull_request'];

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

const INCLUDE = /\binclude_(?:str|bytes)\s*!/g;
const LITERAL = /\s*[([{]\s*(?:r#*)?"([^"]*)"/y;

/** Each `include_str!`/`include_bytes!` in `source` outside a whole-line comment: its line and its
 * path literal (null when the argument is not one). */
function includesOf(source) {
  const out = [];
  source.split('\n').forEach((line, i) => {
    if (/^\s*\/\//.test(line)) return;
    for (const m of line.matchAll(INCLUDE)) {
      LITERAL.lastIndex = m.index + m[0].length;
      out.push({ line: i + 1, path: LITERAL.exec(line)?.[1] ?? null });
    }
  });
  return out;
}

/** GitHub's path filter: `**` crosses directories (`**\/` also matches none), `*` stays in one. */
function patternRegex(pattern) {
  let re = '';
  for (let i = 0; i < pattern.length; i++) {
    const c = pattern[i];
    if (c === '*' && pattern[i + 1] === '*') {
      if (pattern[i + 2] === '/') {
        re += '(?:.*/)?';
        i += 2;
      } else {
        re += '.*';
        i += 1;
      }
    } else if (c === '*') re += '[^/]*';
    else re += c.replace(/[.+?^${}()|[\]\\]/g, '\\$&');
  }
  return new RegExp(`^${re}$`);
}

/** Does the filter `patterns` run the workflow for a change to `path`? The last matching pattern wins. */
function covered(path, patterns) {
  let hit = false;
  for (const p of patterns) {
    const negated = p.startsWith('!');
    if (patternRegex(negated ? p.slice(1) : p).test(path)) hit = !negated;
  }
  return hit;
}

/** The inline `paths: [...]` list under each `on:` trigger of a workflow's YAML, by trigger name. */
function pathsOf(yaml) {
  const out = {};
  let inOn = false;
  let trigger = null;
  for (const line of yaml.split('\n')) {
    if (/^\S/.test(line)) [inOn, trigger] = [/^on:\s*$/.test(line), null];
    const t = inOn && line.match(/^ {2}(\w+):/);
    if (t) trigger = t[1];
    const p = trigger && line.match(/^ {4}paths:\s*\[(.*)\]\s*$/);
    if (p) out[trigger] = p[1].split(',').map((s) => s.trim().replace(/^['"]|['"]$/g, '')).filter(Boolean);
  }
  return out;
}

// ── 1. The check itself, on planted sources ────────────────────────────────────────────────────────
check('finds an include_str! path', () =>
  assert.deepStrictEqual(includesOf(`    const F: &str = include_str!("../../../verify/fixtures/x.json");`), [{ line: 1, path: '../../../verify/fixtures/x.json' }]));
check('finds an include_bytes! raw-string path', () =>
  assert.deepStrictEqual(includesOf(`let b = include_bytes!(r#"../assets/a.wav"#);`), [{ line: 1, path: '../assets/a.wav' }]));
check('flags a non-literal include', () =>
  assert.deepStrictEqual(includesOf(`include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/x"))`), [{ line: 1, path: null }]));
check('finds an include with a space before the bang', () =>
  assert.deepStrictEqual(includesOf(`include_str !("../a.json"); include_bytes !("../b.bin")`), [
    { line: 1, path: '../a.json' },
    { line: 1, path: '../b.bin' },
  ]));
check('skips a commented-out include', () => assert.deepStrictEqual(includesOf(`// include_str!("../../x")`), []));
const PLANTED = ['src-tauri/**', '!src-tauri/**/*.md', 'verify/fixtures/**', '.github/workflows/rust-test.yml'];
check('a fixture under a ** path is covered', () => assert.ok(covered('verify/fixtures/engine-wire.json', PLANTED)));
check('a fixture with its path line dropped is not', () =>
  assert.ok(!covered('verify/fixtures/engine-wire.json', PLANTED.filter((p) => p !== 'verify/fixtures/**'))));
check('a later negation excludes it', () => assert.ok(!covered('verify/fixtures/engine-wire.json', [...PLANTED, '!verify/**'])));
check('**/ matches no directory', () => assert.ok(!covered('src-tauri/README.md', PLANTED)));
check('* stays in one directory', () => assert.ok(!covered('verify/fixtures/deep/x.json', ['verify/fixtures/*'])));
check('reads the inline paths per trigger', () =>
  assert.deepStrictEqual(pathsOf(`on:\n  push:\n    branches: [main]\n    paths: ['a/**', "!a/*.md"]\n  pull_request:\n    paths: ['b']\njobs:\n  x:\n    paths: ['c']\n`), {
    push: ['a/**', '!a/*.md'],
    pull_request: ['b'],
  }));

// ── 2. The tree: every include outside src-tauri/ against rust-test.yml's paths ──────────────────────
function* rustFiles(dir) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) {
      if (entry.name !== 'target' && entry.name !== 'node_modules') yield* rustFiles(path);
    } else if (entry.name.endsWith('.rs')) yield path;
  }
}

const filters = pathsOf(readFileSync(join(root, WORKFLOW), 'utf8'));
for (const trigger of TRIGGERS) {
  check(`${WORKFLOW} has a readable ${trigger} paths list`, () => assert.ok(filters[trigger]?.length, `no inline \`paths: [...]\` under on.${trigger}`));
}
let scanned = 0;
const outside = [];
for (const file of rustFiles(TAURI)) {
  scanned++;
  const rel = relative(root, file).replace(/\\/g, '/');
  for (const { line, path } of includesOf(readFileSync(file, 'utf8'))) {
    if (path === null) {
      check(`${rel}:${line} includes a string literal`, () => assert.fail('the guard cannot resolve a non-literal include path'));
      continue;
    }
    const target = relative(root, resolve(dirname(file), path)).replace(/\\/g, '/');
    if (target.startsWith('src-tauri/')) continue;
    outside.push(target);
    for (const trigger of TRIGGERS) {
      check(`${rel}:${line} includes ${target}, covered by ${WORKFLOW} on ${trigger}`, () =>
        assert.ok(covered(target, filters[trigger] ?? []), `add a path matching ${target} to on.${trigger}.paths`));
    }
  }
}
check('the scan reached the Rust source', () => assert.ok(scanned > 50, `only ${scanned} .rs files under src-tauri/`));
check('the scan found the wire fixtures', () => assert.ok(outside.includes('verify/fixtures/engine-wire.json'), `includes outside src-tauri/: ${outside.join(', ') || 'none'}`));

console.log(`ci-paths: ${outside.join(', ')}`);
console.log(`=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);
