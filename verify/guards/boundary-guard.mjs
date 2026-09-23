// verify/guards/boundary-guard.mjs — self-test for scripts/check-boundary.mjs's classification.
//
// The boundary guard is the ONLY automated check for the load-bearing capability invariant (src/platform/
// is the sole @tauri-apps importer; app layers must use its public seam; platform/ must not reach up into
// audio/ or ui/). Its own header records
// a PAST silent miss: the old `(?:import|from)\s+` regex missed dynamic `import()`. Nothing guarded the
// guard until this. We import its PURE exported classify()/isInPlatform() (no FS, no exit thanks to the
// run-as-main guard) and assert: every @tauri import FORM is flagged in a non-platform file, the SAME
// import is allowed inside platform/, every platform/->audio|ui form (incl type-only + dynamic) is flagged,
// and clean files pass. Real import (plain .mjs), so it cannot drift from the guard. Run: node verify/guards/boundary-guard.mjs

import assert from 'node:assert';
import { classify, isInPlatform } from '../../scripts/check-boundary.mjs';

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

// Path fixtures — forward + backslash, since isInPlatform splits on both.
const PLATFORM = 'C:/Users/x/bleeploop/src/platform/host.tauri.ts';
const PLATFORM_WIN = 'C:\\Users\\x\\bleeploop\\src\\platform\\sub\\deep.ts';
const AUDIO = 'C:/Users/x/bleeploop/src/audio/engine.ts';
const UI = 'C:/Users/x/bleeploop/src/ui/looper/Looper.tsx';

// ---------------------------------------------------------------------------
// 1. isInPlatform — directory membership, not a substring match.
// ---------------------------------------------------------------------------
check('isInPlatform true (forward slash)', () => assert.strictEqual(isInPlatform(PLATFORM), true));
check('isInPlatform true (backslash, nested)', () => assert.strictEqual(isInPlatform(PLATFORM_WIN), true));
check('isInPlatform false (audio/)', () => assert.strictEqual(isInPlatform(AUDIO), false));
check('isInPlatform false (ui/)', () => assert.strictEqual(isInPlatform(UI), false));
// A file merely NAMED platform-ish is NOT inside a platform/ dir (segment match, not substring).
check('isInPlatform false for platform-ish filename', () =>
  assert.strictEqual(isInPlatform('src/audio/platform-utils.ts'), false));
// A nested platform/ dir under audio/ or ui/ is NOT the boundary layer — only src/platform/ is exempt.
check('isInPlatform false for nested src/ui/platform/', () =>
  assert.strictEqual(isInPlatform('C:/Users/x/bleeploop/src/ui/platform/win-titlebar.ts'), false));
check('isInPlatform false for nested src/audio/platform/ (backslash)', () =>
  assert.strictEqual(isInPlatform('C:\\Users\\x\\bleeploop\\src\\audio\\platform\\native-io.ts'), false));
// A `platform` ANCESTOR dir outside src/ must not exempt the whole tree.
check('isInPlatform false for platform ancestor dir outside src', () =>
  assert.strictEqual(isInPlatform('/Users/x/platform/bleeploop/src/audio/engine.ts'), false));

// ---------------------------------------------------------------------------
// 2. @tauri-apps import FORMS — flagged 'tauri' OUTSIDE platform/, allowed INSIDE.
// ---------------------------------------------------------------------------
const TAURI_FORMS = {
  static: `import { invoke } from '@tauri-apps/api/core';`,
  sideEffect: `import '@tauri-apps/api';`,
  reexport: `export { listen } from '@tauri-apps/api/event';`,
  dynamic: `const m = await import('@tauri-apps/api/webview');`,
  doubleQuote: `import { x } from "@tauri-apps/api";`,
  backtick: 'const m = await import(`@tauri-apps/api/core`);',
};
for (const [form, src] of Object.entries(TAURI_FORMS)) {
  check(`@tauri ${form} in audio/ -> 'tauri'`, () => assert.strictEqual(classify(AUDIO, src), 'tauri'));
  check(`@tauri ${form} in ui/ -> 'tauri'`, () => assert.strictEqual(classify(UI, src), 'tauri'));
  check(`@tauri ${form} in platform/ -> null (allowed)`, () => assert.strictEqual(classify(PLATFORM, src), null));
}
// A nested platform/ dir is NOT the boundary layer — a @tauri import there is still a violation.
check('@tauri static in src/audio/platform/ -> tauri (nested platform dir is NOT exempt)', () =>
  assert.strictEqual(classify('C:/Users/x/bleeploop/src/audio/platform/native-io.ts', TAURI_FORMS.static), 'tauri'));

// ---------------------------------------------------------------------------
// 3. Any import below the public platform entry — rejected OUTSIDE platform/, legal INSIDE.
// ---------------------------------------------------------------------------
const PRIVATE_PLATFORM_FORMS = {
  futureModule: `import { setMaster } from '../../platform/master-monitor';`,
  nestedFutureModule: `import { stream } from '../platform/native/stream.ts';`,
  rootRelative: `import { webPlatform } from './platform/host.web';`,
  dynamic: `const log = await import('../../platform/logging');`,
  backtick: 'const types = import(`../../platform/webview2`);',
};
for (const [form, src] of Object.entries(PRIVATE_PLATFORM_FORMS)) {
  check(`private platform ${form} in audio/ -> 'impl'`, () => assert.strictEqual(classify(AUDIO, src), 'impl'));
  check(`private platform ${form} in ui/ -> 'impl'`, () => assert.strictEqual(classify(UI, src), 'impl'));
  check(`private platform ${form} in platform/ -> null (allowed)`, () =>
    assert.strictEqual(classify(PLATFORM, src), null));
}

// The real selector uses sibling implementation imports from inside src/platform/; those stay legal.
check('platform/ sibling host.web implementation import -> null (allowed)', () =>
  assert.strictEqual(classify(PLATFORM, `import { webPlatform } from './host.web';`), null));

// Only the package entry is public. Spelling out index.ts still reaches below the seam.
check('platform package entry in audio/ -> null (allowed)', () =>
  assert.strictEqual(classify(AUDIO, `import { platform } from '../platform';`), null));
check('platform index module in audio/ -> impl (private)', () =>
  assert.strictEqual(classify(AUDIO, `import { platform } from '../platform/index';`), 'impl'));

// ---------------------------------------------------------------------------
// 4. platform/ -> audio|ui leak FORMS — flagged 'leak' INSIDE platform/, harmless elsewhere.
// ---------------------------------------------------------------------------
const LEAK_FORMS = {
  staticAudio: `import { engine } from '../audio/engine';`,
  staticUiNested: `import { x } from '../../ui/looper/waveform';`,
  typeOnly: `import type { TrackState } from '../audio/looper/looper';`,
  sideEffect: `import '../ui/foo';`,
  reexport: `export { clock } from '../audio/clock';`,
  dynamic: `const x = await import('../audio/engine');`,
  deepRelative: `import { z } from '../../../audio/engine';`,
};
for (const [form, src] of Object.entries(LEAK_FORMS)) {
  check(`platform/ ${form} -> 'leak'`, () => assert.strictEqual(classify(PLATFORM, src), 'leak'));
  // The SAME relative import OUTSIDE platform/ is normal (audio importing ../audio etc.) -> null.
  check(`non-platform ${form} -> null (not a leak)`, () => assert.strictEqual(classify(AUDIO, src), null));
}

// ---------------------------------------------------------------------------
// 5. Clean imports pass everywhere.
// ---------------------------------------------------------------------------
const CLEAN = {
  sibling: `import { foo } from './bar';`,
  npmSolid: `import { createSignal } from 'solid-js';`,
  npmTone: `import * as Tone from 'tone';`,
  selfType: `import type { PluginHost } from './types';`,
  platformPublicSeam: `import { platform } from '../../platform';`,
};
for (const [form, src] of Object.entries(CLEAN)) {
  check(`clean ${form} in audio/ -> null`, () => assert.strictEqual(classify(AUDIO, src), null));
  check(`clean ${form} in platform/ -> null`, () => assert.strictEqual(classify(PLATFORM, src), null));
}

console.log(`\n=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);
