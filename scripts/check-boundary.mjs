/**
 * Boundary guard — enforces the capability boundary in BOTH directions:
 *
 *   1. `@tauri-apps/*` may only be imported INSIDE `src/platform/`. Keeps the frontend
 *      buildable/runnable in a plain browser (no Tauri/Rust dependency).
 *   2. Files outside `src/platform/` may only import its public seam, not implementation modules.
 *   3. `src/platform/` may NOT import from `src/audio/` or `src/ui/`. The boundary serves those
 *      layers; it must not depend on them. Without this, a `node.connect(engine.looperInputBus)`
 *      inside `host.tauri.ts` would regress the architecture invisibly — P9 keeps all AudioNode
 *      wiring in `src/audio/plugin-bridge.ts` instead, with `ctx` injected into the host.
 *
 * Run via `pnpm check:boundary`.
 *
 * The classification (`classify` / `isInPlatform` / the two regexes) is exported as a PURE helper so
 * `verify/guards/boundary-guard.mjs` can self-test it against planted-violation fixtures — the guard's
 * own coverage must not be able to rot silently (its history below records a past regex miss). The
 * filesystem walk + reporting + exit run ONLY when this file is executed directly (see the isMain guard
 * at the bottom); importing it for the self-test does not touch the FS or exit.
 */
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const SRC = fileURLToPath(new URL('../src', import.meta.url));

// Match the quoted specifier itself, so this catches every import form: static
// `import … from '@tauri-apps/…'`, side-effect `import '@tauri-apps/…'`, re-export
// `export … from '@tauri-apps/…'`, AND dynamic `import('@tauri-apps/…')` — the last of which
// the previous `(?:import|from)\s+` regex missed (P9/P10 will reach for lazy import()/listen()).
export const TAURI_IMPORT = /['"`]@tauri-apps\//;
// The directory entry (`../platform`) is the ONLY public seam. Every relative import that reaches
// deeper into platform/ is private automatically, including modules added after this guard.
export const PLATFORM_PRIVATE_IMPORT =
  /['"`](?:\.\.?\/)+(?:[^'"`/]+\/)*platform\/[^'"`]+['"`]/;
// A relative import from inside platform/ reaching up into src/audio/ or src/ui/ (any nesting
// depth). Matches the specifier string, so static, side-effect, re-export and dynamic forms are
// all caught — and so are type-only imports (`import type … from '../audio/…'`), which are still
// an architectural coupling even though they erase at compile time.
export const PLATFORM_LEAK = /['"`](?:\.\.\/)+(?:audio|ui)\//;

/**
 * True if `fullPath` lies inside `src/platform/` (the only place @tauri-apps is allowed) — the
 * `platform/` segment must sit immediately under `src/`. A nested `platform/` dir elsewhere (e.g.
 * `src/ui/platform/`, `src/audio/platform/`) or a repo path with a `platform` ancestor dir does NOT
 * count — only the real boundary layer is exempt.
 */
export function isInPlatform(fullPath) {
  return /[\\/]src[\\/]platform[\\/]/.test(fullPath);
}

/**
 * Classify one source file by path + contents. Returns:
 *   'tauri' — a `@tauri-apps/*` import in a file OUTSIDE `src/platform/`,
 *   'impl'  — a file outside `src/platform/` importing a private platform implementation,
 *   'leak'  — a `src/platform/` file importing up into `src/audio/` or `src/ui/`,
 *   null    — clean.
 * Pure: no filesystem, no process exit. The single source of truth for both the CLI walk and the
 * self-test guard.
 */
export function classify(fullPath, src) {
  if (isInPlatform(fullPath)) {
    // Inside the boundary: @tauri-apps is allowed here; importing the app layers is not.
    return PLATFORM_LEAK.test(src) ? 'leak' : null;
  }
  // Outside the boundary: @tauri-apps and every import below the public platform entry are forbidden.
  if (TAURI_IMPORT.test(src)) return 'tauri';
  return PLATFORM_PRIVATE_IMPORT.test(src) ? 'impl' : null;
}

function walk(dir, tauriViolations, implViolations, leakViolations) {
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) {
      walk(full, tauriViolations, implViolations, leakViolations);
      continue;
    }
    if (!/\.[cm]?[tj]sx?$/.test(full)) continue;
    const src = readFileSync(full, 'utf8');
    const rel = full.slice(full.lastIndexOf('src'));
    const verdict = classify(full, src);
    if (verdict === 'tauri') tauriViolations.push(rel);
    else if (verdict === 'impl') implViolations.push(rel);
    else if (verdict === 'leak') leakViolations.push(rel);
  }
}

function main() {
  const tauriViolations = [];
  const implViolations = [];
  const leakViolations = [];
  walk(SRC, tauriViolations, implViolations, leakViolations);

  let failed = false;
  if (tauriViolations.length > 0) {
    failed = true;
    console.error('x  boundary violation: @tauri-apps imported outside src/platform/:');
    for (const v of tauriViolations) console.error('     ' + v);
  }
  if (implViolations.length > 0) {
    failed = true;
    console.error('x  boundary violation: private platform module imported outside src/platform/:');
    for (const v of implViolations) console.error('     ' + v);
  }
  if (leakViolations.length > 0) {
    failed = true;
    console.error('x  boundary violation: src/platform/ imported from src/audio/ or src/ui/:');
    for (const v of leakViolations) console.error('     ' + v);
  }
  if (failed) process.exit(1);

  console.log(
    'ok boundary: no @tauri-apps or private platform modules outside src/platform/; no platform/ → audio|ui imports',
  );
}

// Run the FS walk only when executed directly (`node scripts/check-boundary.mjs` / `pnpm check:boundary`).
// Importing this module for the self-test must NOT walk the tree or call process.exit.
if (import.meta.url === pathToFileURL(process.argv[1]).href) main();
