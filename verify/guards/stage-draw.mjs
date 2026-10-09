// verify/guards/stage-draw.mjs — invariant 6 for the stage view (src/ui/stage/): the 60 fps draw loop
// reads a plain feed and the looper's non-reactive getters, never a Solid signal.
//
// A static check of the real source. The draw modules are everything `stage-loop.ts` (the rAF loop) and
// `views.ts` (the looks it draws) reach through relative imports. Each of them may import only its
// siblings and, from `../state/audio`, the looper facade, `PEAK_FRAMES` and types; none may import
// `solid-js`; and on the looper facade it may touch only the plain getters (`PLAIN`), since the same
// object also carries signal accessors (`selectedTrack`, `trackVolume`, `track`, ...). Solid's side of
// the stage (`StageView.tsx`, `stage-store.ts`) is not reached and is free to read signals: it writes the
// plain feed from effects. The facade may be used only as `looper.<getter>`: an alias, a destructuring
// or a pass-along would hide what is read, and a re-export counts as an import (and is followed).
//
// `violations` is first run on planted sources (each must be flagged, a clean one must pass), then on
// the modules themselves. It cannot see a signal that reaches the loop some other way (an accessor
// stored in the plain feed, say): the feed's shape is a review matter. Run: node verify/guards/stage-draw.mjs

import assert from 'node:assert';
import { existsSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const stageDir = join(dirname(fileURLToPath(import.meta.url)), '..', '..', 'src', 'ui', 'stage');
const ROOTS = ['stage-loop', 'views'];
/** The looper's non-reactive getters (`engineLooper` in src/ui/state/engine-store.ts). */
const PLAIN = new Set(['peaksInto', 'scopeInto', 'phaseValue', 'levelValue', 'stateOf', 'mutedOf', 'waitingOf', 'recHeadFrac', 'recSpanFrames', 'masterFramesValue', 'gridValue', 'trackCount']);
/** What a draw module may take from `../state/audio`. */
const AUDIO = new Set(['looper', 'PEAK_FRAMES', 'PeakView', 'TrackState', 'ScopeView', 'SCOPE_MASTER', 'SCOPE_MONITOR']);

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

const stripComments = (source) => source.replace(/\/\*[\s\S]*?\*\//g, '').replace(/^\s*\/\/.*$/gm, '');

const IMPORT_FROM = /import\s+(?:type\s+)?([\s\S]*?)\s+from\s+['"]([^'"]+)['"]/g;
const EXPORT_FROM = /export\s+(?:type\s+)?(\*(?:\s+as\s+\w+)?|\{[^}]*\})\s+from\s+['"]([^'"]+)['"]/g;

/** Every import and re-export of `source`: its specifier, the names it binds or passes on (empty for a
 * side-effect or dynamic one) and those it renames. */
function importsOf(source) {
  const out = [];
  for (const m of [...source.matchAll(IMPORT_FROM), ...source.matchAll(EXPORT_FROM)]) {
    const parts = m[1].replace(/[{}]/g, ' ').split(',').map((n) => n.trim().replace(/^type\s+/, '')).filter(Boolean);
    out.push({ from: m[2], names: parts.map((n) => n.split(/\s+as\s+/)[0]), renamed: parts.filter((n) => /\s+as\s+/.test(n)) });
  }
  for (const m of source.matchAll(/import\s*\(?\s*['"]([^'"]+)['"]/g)) out.push({ from: m[1], names: [], renamed: [] });
  return out;
}

/** What `source` (a draw module) does that the draw loop may not. */
function violations(source) {
  const code = stripComments(source);
  const found = [];
  for (const { from, names, renamed } of importsOf(code)) {
    if (from === 'solid-js' || from.startsWith('solid-js/')) found.push('imports solid-js');
    else if (from === '../state/audio') {
      for (const name of names) if (!AUDIO.has(name)) found.push(`imports ${name} from ../state/audio`);
      for (const name of renamed) found.push(`renames ${name} from ../state/audio`);
    } else if (!/^\.\/[\w-]+$/.test(from)) found.push(`imports ${from}`);
  }
  for (const m of code.matchAll(/\blooper\s*\.\s*(\w+)/g)) if (!PLAIN.has(m[1])) found.push(`reads looper.${m[1]}`);
  // Outside its import, the facade appears only as `looper.<name>`: anything else hands it on unread.
  const body = code.replace(IMPORT_FROM, '').replace(EXPORT_FROM, '');
  if (/\blooper\b(?!\s*\.\s*\w)/.test(body)) found.push('uses looper other than as looper.<getter>');
  return found;
}

// ── 1. The check itself, on planted sources ────────────────────────────────────────────────────────
const CLEAN = `import { PEAK_FRAMES, looper, type PeakView } from '../state/audio';
import { feed } from './stage-feed';
// looper.selectedTrack() is named in a comment only
export const phase = () => looper.phaseValue() + looper.recSpanFrames(0) / PEAK_FRAMES;`;
check('a clean draw module passes', () => assert.deepStrictEqual(violations(CLEAN), []));
check('a solid-js import is flagged', () => assert.deepStrictEqual(violations(`import { createEffect } from 'solid-js';`), ['imports solid-js']));
check('a solid-js/web import is flagged', () => assert.deepStrictEqual(violations(`import { render } from "solid-js/web";`), ['imports solid-js']));
check('a signal accessor on the looper is flagged', () => assert.deepStrictEqual(violations(`${CLEAN}\nconst i = looper.selectedTrack();`), ['reads looper.selectedTrack']));
check('a track signal read is flagged', () => assert.deepStrictEqual(violations(`const t = looper\n  .track(0)();`), ['reads looper.track']));
check('the clock is flagged', () => assert.deepStrictEqual(violations(`import { clock, looper } from '../state/audio';`), ['imports clock from ../state/audio']));
check('the lane derivation is flagged', () => assert.deepStrictEqual(violations(`import { createLaneView } from '../looper/lane-state';`), ['imports ../looper/lane-state']));
check('the stage store is reachable only as a sibling, and a sibling import is allowed', () => assert.deepStrictEqual(violations(`import { light } from './visual';`), []));
check('a dynamic import of a store is flagged', () => assert.deepStrictEqual(violations(`const m = await import('../state/engine-store');`), ['imports ../state/engine-store']));
check('a type-only import of a signal module is flagged', () => assert.deepStrictEqual(violations(`import type { Accessor } from 'solid-js';`), ['imports solid-js']));
check('an aliased facade is flagged', () => assert.deepStrictEqual(violations(`import { looper as lp } from '../state/audio';\nconst i = lp.selectedTrack();`), ['renames looper as lp from ../state/audio']));
check('a destructured signal getter is flagged', () => assert.deepStrictEqual(violations(`${CLEAN}\nconst { selectedTrack } = looper;`), ['uses looper other than as looper.<getter>']));
check('a facade handed on is flagged', () => assert.deepStrictEqual(violations(`${CLEAN}\nconst lp = looper;\nread(looper);`), ['uses looper other than as looper.<getter>']));
check('a bracket read is flagged', () => assert.deepStrictEqual(violations(`${CLEAN}\nconst i = looper['selectedTrack']();`), ['uses looper other than as looper.<getter>']));
check('a re-exported signal module is flagged', () => assert.deepStrictEqual(violations(`export { clock } from '../state/audio';`), ['imports clock from ../state/audio']));
check('a star re-export of a store is flagged', () => assert.deepStrictEqual(violations(`export * from '../state/engine-store';`), ['imports ../state/engine-store']));
check('a sibling re-export is followed like an import', () => assert.deepStrictEqual(importsOf(`export { light } from './visual';`).map((i) => i.from), ['./visual']));

// ── 2. The draw modules: everything the loop and the looks reach ───────────────────────────────────
const reached = new Map();
const queue = [...ROOTS];
while (queue.length) {
  const name = queue.shift();
  if (reached.has(name)) continue;
  const file = join(stageDir, `${name}.ts`);
  check(`${name}.ts exists`, () => assert.ok(existsSync(file), `${file} is missing (a .tsx is a component, not a draw module)`));
  if (!existsSync(file)) continue;
  const source = readFileSync(file, 'utf8');
  reached.set(name, source);
  for (const { from } of importsOf(stripComments(source))) if (from.startsWith('./')) queue.push(from.slice(2));
}
for (const [name, source] of reached) {
  check(`${name}.ts reads no signal`, () => assert.deepStrictEqual(violations(source), [], `${name}.ts: ${violations(source).join('; ')}`));
}
check('the walk reached the feed, the shared helpers and at least two looks', () => {
  for (const name of ['stage-feed', 'visual', 'orbit', 'strata']) assert.ok(reached.has(name), `${name}.ts was not reached from ${ROOTS.join(', ')}`);
});
check('the Solid side stays out of the walk', () => {
  assert.ok(!reached.has('stage-store'), 'stage-store.ts (signals) is imported by a draw module');
});

console.log(`stage-draw: ${[...reached.keys()].sort().join(', ')}`);
console.log(`=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);
