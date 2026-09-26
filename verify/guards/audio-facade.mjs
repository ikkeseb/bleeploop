// verify/guards/audio-facade.mjs — `src/ui/AGENTS.md`'s engine-mode rule as a gate: code under src/ui/
// (but src/ui/state/, the switch itself) and src/app/ + src/app.tsx takes `looper`, `clock`, `master` and
// `sampleRate` from src/ui/state/audio.ts, never from the web modules behind them (src/audio/looper/looper,
// clock, master, engine). A direct import runs the web path in engine mode without an error.
//
// Walks the real tree and resolves every relative import (static, side-effect, re-export, dynamic, type-
// only alike). EXCEPTIONS lists the web-only paths that must reach the web engine itself, each with its
// reason; an exception no longer used fails too, so the list cannot rot. The classifier is self-tested on
// planted sources first. Run: node verify/guards/audio-facade.mjs

import assert from 'node:assert/strict';
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '../..');

/** The web modules the switch (src/ui/state/audio.ts) stands in front of, repo-relative, no extension. */
const WEB_AUDIO = ['src/audio/looper/looper', 'src/audio/clock', 'src/audio/master', 'src/audio/engine'];

/** file → the web modules it may import, and why. */
const EXCEPTIONS = {
  'src/app/boot.ts': {
    'src/audio/engine': "the web boot chain: the plugin bridge on the web AudioContext, resumed on the first gesture (engine mode takes bootEngine)",
  },
  'src/ui/keyboard/Keyboard.tsx': {
    'src/audio/engine': 'the web path resumes the AudioContext on the first key; engine mode skips it',
  },
};

const SPECIFIER = /(?:\bfrom\s*|\bimport\s*\(\s*|\bimport\s+)['"`]([^'"`]+)['"`]/g;

/** The web modules `source` (at repo-relative `file`) imports. */
export function webAudioImports(file, source) {
  const found = [];
  for (const [, spec] of source.matchAll(SPECIFIER)) {
    if (!spec.startsWith('.')) continue;
    const target = relative(ROOT, resolve(ROOT, dirname(file), spec)).replace(/\\/g, '/').replace(/\.(tsx?|js)$/, '');
    if (WEB_AUDIO.includes(target)) found.push(target);
  }
  return found;
}

/** Whether the rule covers `file` (repo-relative). */
export function covered(file) {
  return (file.startsWith('src/ui/') && !file.startsWith('src/ui/state/')) || file.startsWith('src/app/') || file === 'src/app.tsx';
}

let passed = 0;
let failed = 0;
function check(name, fn) {
  try {
    fn();
    passed++;
  } catch (err) {
    failed++;
    console.error(`FAIL ${name}: ${err.message}`);
  }
}

// ── The classifier on planted sources ──────────────────────────────────────────────────────────────
const planted = [
  ['src/ui/looper/X.tsx', "import { looper } from '../../audio/looper/looper';", ['src/audio/looper/looper']],
  ['src/ui/looper/X.tsx', "import { clock } from '../../audio/clock.ts';", ['src/audio/clock']],
  ['src/app/X.ts', "import type { TrackState } from '../audio/looper/looper';", ['src/audio/looper/looper']],
  ['src/app.tsx', "import { master } from './audio/master';", ['src/audio/master']],
  ['src/ui/X.tsx', "const m = await import('../audio/engine');", ['src/audio/engine']],
  ['src/ui/X.tsx', "export { engine } from '../audio/engine';", ['src/audio/engine']],
  ['src/ui/looper/X.tsx', "import { looper } from '../state/audio';", []],
  ['src/ui/looper/X.tsx', "import { autoRecordThreshold } from '../../audio/looper/auto-record';", []],
];
for (const [file, source, expected] of planted) {
  check(`planted ${file}: ${source}`, () => assert.deepEqual(webAudioImports(file, source), expected));
}
check('the rule covers the UI and the app, not the switch', () => {
  assert.ok(covered('src/ui/looper/Looper.tsx') && covered('src/app/boot.ts') && covered('src/app.tsx'));
  assert.ok(!covered('src/ui/state/audio.ts') && !covered('src/audio/autosave.ts') && !covered('src/debug/lf.ts'));
});

// ── The tree ───────────────────────────────────────────────────────────────────────────────────────
function walk(dir, out = []) {
  for (const name of readdirSync(dir)) {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) walk(path, out);
    else if (/\.tsx?$/.test(name)) out.push(relative(ROOT, path).replace(/\\/g, '/'));
  }
  return out;
}
const files = walk(join(ROOT, 'src')).filter(covered);
check('the walk found the UI', () => assert.ok(files.length > 20, `${files.length} files`));
const used = new Set();
for (const file of files) {
  const imports = webAudioImports(file, readFileSync(join(ROOT, file), 'utf8'));
  check(`${file} takes the audio facade from src/ui/state/audio.ts`, () => {
    const allowed = EXCEPTIONS[file] ?? {};
    const refused = imports.filter((target) => !(target in allowed));
    assert.deepEqual(refused, [], `imports ${refused.join(', ')} directly`);
  });
  for (const target of imports) used.add(`${file} → ${target}`);
}
for (const [file, targets] of Object.entries(EXCEPTIONS)) {
  for (const target of Object.keys(targets)) {
    check(`exception ${file} → ${target} is still used`, () => assert.ok(used.has(`${file} → ${target}`)));
  }
}

console.log(`=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);
