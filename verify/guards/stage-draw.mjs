// verify/guards/stage-draw.mjs — invariant 6 for the two 60 fps draw loops, the stage view's
// (src/ui/stage/) and the looper's (src/ui/looper/waveform.ts): each reads a plain feed and the looper's
// non-reactive getters, never a Solid signal.
//
// A static check of the real source. A loop's draw modules are everything its roots reach through
// relative sibling imports: for the stage, `stage-loop.ts` (the rAF loop) and `views.ts` (the looks it
// draws); for the looper, `waveform.ts`, whole (its register calls run from Solid, read no signal
// either). Each may import only its siblings and, from `../state/audio`, the looper facade,
// `PEAK_FRAMES` and types; none may import `solid-js`; and on the looper facade it may touch only the
// plain getters (`PLAIN`), since the same object also carries signal accessors (`selectedTrack`,
// `trackVolume`, `track`, ...). Solid's side of the stage (`StageView.tsx`, `stage-store.ts`) is not
// reached and is free to read signals: it writes the plain feed from effects. The facade may be used only
// as `looper.<getter>`: an alias, a destructuring or a pass-along would hide what is read, and a
// re-export counts as an import (and is followed).
//
// The looper's loop has one documented exception (`src/ui/AGENTS.md`, invariant 6): its bar grid reads
// `clock.bpm()` and `sampleRate()`, inside `rasterise` and only when the master loop's length changes.
// So `waveform.ts` may import `clock` and `sampleRate`, and `masterBars` (pure bar math) from its
// Solid-side sibling `shared.ts`, which is not followed; `clock` and `sampleRate` may appear only as
// those two calls inside `rasterise`'s `if (masterFrames !== lane.gridMaster)` block.
//
// `violations` is first run on planted sources (each must be flagged, a clean one must pass), then on
// the modules themselves. It cannot see a signal that reaches the loop some other way (an accessor
// stored in the plain feed, say): the feed's shape is a review matter. Run: node verify/guards/stage-draw.mjs

import assert from 'node:assert';
import { existsSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const uiDir = join(dirname(fileURLToPath(import.meta.url)), '..', '..', 'src', 'ui');
/** The looper's non-reactive getters (`engineLooper` in src/ui/state/engine-store.ts). */
const PLAIN = new Set(['peaksInto', 'scopeInto', 'phaseValue', 'levelValue', 'stateOf', 'mutedOf', 'waitingOf', 'countingValue', 'recHeadFrac', 'recSpanFrames', 'masterFramesValue', 'gridValue', 'heardFrameValue', 'trackCount', 'clipCount', 'takePeak']);
/** What a draw module may take from `../state/audio`. */
const AUDIO = ['looper', 'PEAK_FRAMES', 'PeakView', 'TrackState', 'ScopeView', 'SCOPE_MASTER', 'SCOPE_MONITOR'];

/** The stage view's loop: no exception. */
const STAGE = { dir: 'stage', roots: ['stage-loop', 'views'], audio: new Set(AUDIO), named: {}, gated: null };
/** The looper's loop, with its bar grid's tempo read (the header). */
const LOOPER = {
  dir: 'looper',
  roots: ['waveform'],
  audio: new Set([...AUDIO, 'clock', 'sampleRate']),
  named: { './shared': ['masterBars'] },
  gated: {
    names: ['clock', 'sampleRate'],
    reads: /^(?:clock\s*\.\s*bpm\s*\(|sampleRate\s*\()/,
    fn: 'rasterise',
    gate: /if\s*\(\s*masterFrames\s*!==\s*lane\s*\.\s*gridMaster\s*\)\s*\{/,
  },
};

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

/** The `{ ... }` block whose opening brace is at `open` in `code`: its inner text and its end. */
function block(code, open) {
  let depth = 0;
  for (let i = open; i < code.length; i++) {
    if (code[i] === '{') depth++;
    else if (code[i] === '}' && --depth === 0) return { inner: code.slice(open + 1, i), end: i + 1 };
  }
  return { inner: code.slice(open + 1), end: code.length };
}

/** `body` with the gated block of `gated.fn` cut out, and that block's text ('' when it is not there). */
function cutGate(body, gated) {
  const fn = body.search(new RegExp(`function\\s+${gated.fn}\\s*\\(`));
  if (fn < 0) return { rest: body, inGate: '' };
  const fnBody = block(body, body.indexOf('{', body.indexOf(')', fn)));
  const at = body.slice(0, fnBody.end).slice(fn).search(gated.gate);
  if (at < 0) return { rest: body, inGate: '' };
  const start = fn + at;
  const gate = block(body, body.indexOf('{', start + body.slice(start).search(/\)\s*\{/)));
  return { rest: body.slice(0, start) + body.slice(gate.end), inGate: gate.inner };
}

/** What `source` (a draw module of `set`) does that the draw loop may not. */
function violations(source, set = STAGE) {
  const code = stripComments(source);
  const found = [];
  for (const { from, names, renamed } of importsOf(code)) {
    if (from === 'solid-js' || from.startsWith('solid-js/')) found.push('imports solid-js');
    else if (from === '../state/audio' || Object.hasOwn(set.named, from)) {
      const allowed = from === '../state/audio' ? set.audio : new Set(set.named[from]);
      for (const name of names) if (!allowed.has(name)) found.push(`imports ${name} from ${from}`);
      for (const name of renamed) found.push(`renames ${name} from ${from}`);
    } else if (!/^\.\/[\w-]+$/.test(from)) found.push(`imports ${from}`);
  }
  for (const m of code.matchAll(/\blooper\s*\.\s*(\w+)/g)) if (!PLAIN.has(m[1])) found.push(`reads looper.${m[1]}`);
  // Outside its import, the facade appears only as `looper.<name>`: anything else hands it on unread.
  const body = code.replace(IMPORT_FROM, '').replace(EXPORT_FROM, '');
  if (/\blooper\b(?!\s*\.\s*\w)/.test(body)) found.push('uses looper other than as looper.<getter>');
  // The exception's names: only their allowed reads, only inside the gate.
  if (set.gated) {
    const { rest, inGate } = cutGate(body, set.gated);
    for (const name of set.gated.names) {
      const word = `\\b${name}\\b`;
      if (new RegExp(word).test(rest)) found.push(`uses ${name} outside ${set.gated.fn}'s master-length gate`);
      for (const m of inGate.matchAll(new RegExp(word, 'g'))) {
        if (!set.gated.reads.test(inGate.slice(m.index))) found.push(`uses ${name} other than as its allowed read`);
      }
    }
  }
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

// The looper's loop, its tempo read gated as waveform.ts has it.
const WAVE = `import { PEAK_FRAMES, clock, looper, sampleRate } from '../state/audio';
import { masterBars } from './shared';
function rasterise(lane: Lane, masterFrames: number): void {
  if (masterFrames !== lane.gridMaster) {
    lane.gridMaster = masterFrames;
    lane.gridBars = masterFrames > 0 ? masterBars(masterFrames, clock.bpm(), sampleRate()) : 0;
  }
  drawGrid(lane, lane.gridBars);
}
function frame(): void {
  rasterise(lane, looper.masterFramesValue());
  rafId = requestAnimationFrame(frame);
}`;
const wave = (find, replace) => violations(WAVE.replace(find, replace), LOOPER);
check('a clean looper loop passes, its gated tempo read included', () => assert.deepStrictEqual(violations(WAVE, LOOPER), []));
check('a track signal read in the looper frame is flagged', () => assert.deepStrictEqual(wave('  rafId', '  if (looper.track(0)().muted) return;\n  rafId'), ['reads looper.track']));
check('a tempo read in the looper frame is flagged', () => assert.deepStrictEqual(wave('  rafId', '  const b = clock.bpm();\n  rafId'), ["uses clock outside rasterise's master-length gate"]));
check('a tempo read past the gate in rasterise is flagged', () => assert.deepStrictEqual(wave('  drawGrid(', '  const r = sampleRate();\n  drawGrid('), ["uses sampleRate outside rasterise's master-length gate"]));
check('the read without its gate is flagged', () => assert.deepStrictEqual(wave('if (masterFrames !== lane.gridMaster) {', 'if (true) {'), ["uses clock outside rasterise's master-length gate", "uses sampleRate outside rasterise's master-length gate"]));
check('another clock read inside the gate is flagged', () => assert.deepStrictEqual(wave('clock.bpm()', 'clock.bpmLocked() && clock.bpm()'), ['uses clock other than as its allowed read']));
check('the tempo read stays out of the stage loop', () => assert.deepStrictEqual(violations(WAVE), ['imports clock from ../state/audio', 'imports sampleRate from ../state/audio']));
check('only masterBars comes from shared.ts', () => assert.deepStrictEqual(wave("import { masterBars } from './shared';", "import { masterBars, anyTrackIn } from './shared';"), ['imports anyTrackIn from ./shared']));

// ── 2. The draw modules: everything each loop reaches ───────────────────────────────────────────────
/** Walk `set`'s roots through sibling imports (those `set.named` lists are not followed) and check each. */
function walk(set) {
  const reached = new Map();
  const queue = [...set.roots];
  while (queue.length) {
    const name = queue.shift();
    if (reached.has(name)) continue;
    const file = join(uiDir, set.dir, `${name}.ts`);
    check(`${set.dir}/${name}.ts exists`, () => assert.ok(existsSync(file), `${file} is missing (a .tsx is a component, not a draw module)`));
    if (!existsSync(file)) continue;
    const source = readFileSync(file, 'utf8');
    reached.set(name, source);
    for (const { from } of importsOf(stripComments(source))) if (from.startsWith('./') && !Object.hasOwn(set.named, from)) queue.push(from.slice(2));
  }
  for (const [name, source] of reached) {
    check(`${set.dir}/${name}.ts reads no signal`, () => assert.deepStrictEqual(violations(source, set), [], `${name}.ts: ${violations(source, set).join('; ')}`));
  }
  return reached;
}
const stage = walk(STAGE);
check('the stage walk reached the feed, the shared helpers and at least two looks', () => {
  for (const name of ['stage-feed', 'visual', 'orbit', 'strata']) assert.ok(stage.has(name), `${name}.ts was not reached from ${STAGE.roots.join(', ')}`);
});
check('the Solid side stays out of the stage walk', () => {
  assert.ok(!stage.has('stage-store'), 'stage-store.ts (signals) is imported by a draw module');
});
const looperLoop = walk(LOOPER);
check('the looper walk holds waveform.ts, not its Solid-side sibling', () => assert.deepStrictEqual([...looperLoop.keys()], ['waveform']));

console.log(`stage-draw: ${[...stage.keys()].sort().join(', ')}; looper: ${[...looperLoop.keys()].join(', ')}`);
console.log(`=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);
