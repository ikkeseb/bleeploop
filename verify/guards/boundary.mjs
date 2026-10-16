// verify/guards/boundary.mjs — invariant 7, the capability boundary, in both directions:
//
//   1. `@tauri-apps/*` may only be imported INSIDE `src/platform/`. Keeps the frontend buildable and
//      runnable in a plain browser (no Tauri/Rust dependency).
//   2. Files outside `src/platform/` may only import its public seam (`../platform`), not its modules.
//   3. `src/platform/` may NOT import from the app layers above it: `src/ui/`, `src/session/`, `src/app/`
//      or `src/debug/` (the root-level leaves `src/notify.ts` and `src/settings-opener.ts` are not
//      layers). The boundary serves those layers; a store or the session code reached from
//      `host.tauri.ts` would regress the architecture invisibly.
//
// `classify` is first run on planted sources (every import form of each violation must be flagged, the
// same import where it is legal and clean files must pass), then on every source file under `src/`. It
// matches the quoted specifier, so static, side-effect, re-export, dynamic and type-only imports are all
// seen (a past `(?:import|from)\s+` regex missed dynamic `import()`); it cannot see a specifier built at
// run time. Run: node verify/guards/boundary.mjs

import assert from 'node:assert';
import { readFileSync, readdirSync } from 'node:fs';
import { dirname, join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const SRC = join(root, 'src');

const TAURI_IMPORT = /['"`]@tauri-apps\//;
// The directory entry (`../platform`) is the ONLY public seam. Every relative import that reaches
// deeper into platform/ is private automatically, including modules added after this guard.
const PLATFORM_PRIVATE_IMPORT = /['"`](?:\.\.?\/)+(?:[^'"`/]+\/)*platform\/[^'"`]+['"`]/;
// A relative import from inside platform/ reaching up into an app layer, at any nesting depth.
const PLATFORM_LEAK = /['"`](?:\.\.\/)+(?:app|debug|session|ui)\//;

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

/** True if `fullPath` lies inside `src/platform/`: the `platform/` segment must sit directly under
 * `src/`, so a nested one (e.g. `src/ui/platform/`) or a `platform` ancestor of the repo does not count. */
function isInPlatform(fullPath) {
  return /[\\/]src[\\/]platform[\\/]/.test(fullPath);
}

/** One source file by path and contents: 'tauri' (a `@tauri-apps/*` import outside `src/platform/`),
 * 'impl' (a private platform module imported outside it), 'leak' (a `src/platform/` file importing an
 * app layer) or null (clean). */
function classify(fullPath, src) {
  if (isInPlatform(fullPath)) return PLATFORM_LEAK.test(src) ? 'leak' : null;
  if (TAURI_IMPORT.test(src)) return 'tauri';
  return PLATFORM_PRIVATE_IMPORT.test(src) ? 'impl' : null;
}

// ── 1. The check itself, on planted sources ────────────────────────────────────────────────────────
// Path fixtures, forward and backslash, since isInPlatform splits on both.
const PLATFORM = 'C:/Users/x/bleeploop/src/platform/host.tauri.ts';
const PLATFORM_WIN = 'C:\\Users\\x\\bleeploop\\src\\platform\\sub\\deep.ts';
const SESSION = 'C:/Users/x/bleeploop/src/session/export.ts';
const UI = 'C:/Users/x/bleeploop/src/ui/looper/Looper.tsx';

check('isInPlatform true (forward slash)', () => assert.strictEqual(isInPlatform(PLATFORM), true));
check('isInPlatform true (backslash, nested)', () => assert.strictEqual(isInPlatform(PLATFORM_WIN), true));
check('isInPlatform false (session/)', () => assert.strictEqual(isInPlatform(SESSION), false));
check('isInPlatform false (ui/)', () => assert.strictEqual(isInPlatform(UI), false));
check('isInPlatform false for platform-ish filename', () => assert.strictEqual(isInPlatform('src/session/platform-utils.ts'), false));
check('isInPlatform false for nested src/ui/platform/', () =>
  assert.strictEqual(isInPlatform('C:/Users/x/bleeploop/src/ui/platform/win-titlebar.ts'), false));
check('isInPlatform false for nested src/session/platform/ (backslash)', () =>
  assert.strictEqual(isInPlatform('C:\\Users\\x\\bleeploop\\src\\session\\platform\\native-io.ts'), false));
check('isInPlatform false for platform ancestor dir outside src', () =>
  assert.strictEqual(isInPlatform('/Users/x/platform/bleeploop/src/session/export.ts'), false));

// @tauri-apps import forms: 'tauri' outside platform/, allowed inside.
const TAURI_FORMS = {
  static: `import { invoke } from '@tauri-apps/api/core';`,
  sideEffect: `import '@tauri-apps/api';`,
  reexport: `export { listen } from '@tauri-apps/api/event';`,
  dynamic: `const m = await import('@tauri-apps/api/webview');`,
  doubleQuote: `import { x } from "@tauri-apps/api";`,
  backtick: 'const m = await import(`@tauri-apps/api/core`);',
};
for (const [form, src] of Object.entries(TAURI_FORMS)) {
  check(`@tauri ${form} in session/ -> 'tauri'`, () => assert.strictEqual(classify(SESSION, src), 'tauri'));
  check(`@tauri ${form} in ui/ -> 'tauri'`, () => assert.strictEqual(classify(UI, src), 'tauri'));
  check(`@tauri ${form} in platform/ -> null (allowed)`, () => assert.strictEqual(classify(PLATFORM, src), null));
}
check('@tauri static in src/session/platform/ -> tauri (nested platform dir is NOT exempt)', () =>
  assert.strictEqual(classify('C:/Users/x/bleeploop/src/session/platform/native-io.ts', TAURI_FORMS.static), 'tauri'));

// Any import below the public platform entry: 'impl' outside platform/, legal inside.
const PRIVATE_PLATFORM_FORMS = {
  futureModule: `import { setMaster } from '../../platform/master-monitor';`,
  nestedFutureModule: `import { stream } from '../platform/native/stream.ts';`,
  rootRelative: `import { webPlatform } from './platform/host.web';`,
  dynamic: `const log = await import('../../platform/logging');`,
  backtick: 'const types = import(`../../platform/webview2`);',
};
for (const [form, src] of Object.entries(PRIVATE_PLATFORM_FORMS)) {
  check(`private platform ${form} in session/ -> 'impl'`, () => assert.strictEqual(classify(SESSION, src), 'impl'));
  check(`private platform ${form} in ui/ -> 'impl'`, () => assert.strictEqual(classify(UI, src), 'impl'));
  check(`private platform ${form} in platform/ -> null (allowed)`, () => assert.strictEqual(classify(PLATFORM, src), null));
}
check('platform/ sibling host.web implementation import -> null (allowed)', () =>
  assert.strictEqual(classify(PLATFORM, `import { webPlatform } from './host.web';`), null));
check('platform package entry in session/ -> null (allowed)', () =>
  assert.strictEqual(classify(SESSION, `import { platform } from '../platform';`), null));
check('platform index module in session/ -> impl (private)', () =>
  assert.strictEqual(classify(SESSION, `import { platform } from '../platform/index';`), 'impl'));

// platform/ -> app-layer forms: 'leak' inside platform/, the same import harmless elsewhere.
const LEAK_FORMS = {
  staticSession: `import { exportLoops } from '../session/export';`,
  staticUiNested: `import { x } from '../../ui/looper/waveform';`,
  typeOnly: `import type { TrackState } from '../ui/state/looper-types';`,
  sideEffect: `import '../ui/foo';`,
  reexport: `export { autosave } from '../session/autosave';`,
  dynamic: `const x = await import('../app/boot');`,
  deepRelative: `import { z } from '../../../debug/lf';`,
};
for (const [form, src] of Object.entries(LEAK_FORMS)) {
  check(`platform/ ${form} -> 'leak'`, () => assert.strictEqual(classify(PLATFORM, src), 'leak'));
  check(`non-platform ${form} -> null (not a leak)`, () => assert.strictEqual(classify(SESSION, src), null));
}

// Clean imports pass everywhere.
const CLEAN = {
  sibling: `import { foo } from './bar';`,
  npmSolid: `import { createSignal } from 'solid-js';`,
  selfType: `import type { PluginHost } from './types';`,
  platformPublicSeam: `import { platform } from '../../platform';`,
  rootNotify: `import { notifyError } from '../notify';`,
};
for (const [form, src] of Object.entries(CLEAN)) {
  check(`clean ${form} in session/ -> null`, () => assert.strictEqual(classify(SESSION, src), null));
  check(`clean ${form} in platform/ -> null`, () => assert.strictEqual(classify(PLATFORM, src), null));
}

// ── 2. The source ──────────────────────────────────────────────────────────────────────────────────
const MESSAGES = {
  tauri: '@tauri-apps imported outside src/platform/',
  impl: 'a private platform module imported outside src/platform/ (import `platform` from its directory)',
  leak: 'src/platform/ imports an app layer (ui/, session/, app/, debug/)',
};

function* files(dir) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) yield* files(path);
    else if (/\.[cm]?[tj]sx?$/.test(entry.name)) yield path;
  }
}

let scanned = 0;
let inPlatform = 0;
for (const path of files(SRC)) {
  scanned++;
  const file = relative(root, path).replace(/\\/g, '/');
  // Classified by its path from the repo root, so a `src/platform/` above the checkout exempts nothing.
  if (isInPlatform(`/${file}`)) inPlatform++;
  const verdict = classify(`/${file}`, readFileSync(path, 'utf8'));
  check(`${file} keeps the boundary`, () => assert.strictEqual(verdict, null, verdict && MESSAGES[verdict]));
}
check('the scan reached the source and src/platform/', () =>
  assert.ok(scanned > 50 && inPlatform > 3, `${scanned} files under src/, ${inPlatform} in src/platform/`));

console.log(`=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);
