/**
 * Runtime platform selection. `@tauri-apps/api` is pure JS, so importing it in a browser is safe
 * — `isTauri()` returns false and we hand back the web implementation. Under the Tauri shell
 * it returns true and we hand back `tauriPlatform`; nothing else in the app changes.
 */
import { isTauri } from '@tauri-apps/api/core';
import type { Platform } from './host';
import type { EngineCommand } from './engine-wire';
import { tauriInstallFrontendLogPipe } from './logging';
import { webEngineFake, webPlatform, type EngineFake } from './host.web';
import { notifyError } from '../notify';
import { reportTauriDiagnostics, tauriConfirmClose, tauriOnCloseRequested, tauriPlatform } from './host.tauri';

const underTauri = isTauri();

export const platform: Platform = underTauri ? tauriPlatform : webPlatform;

/**
 * DEV-only: report WebView2-internal facts (secure context, MIDI, …) to `tauri dev` stdout —
 * the only way to verify them headlessly (no Playwright into WebView2). No-op in the browser build.
 * Informational; does not touch the plugin host.
 */
export const reportDiagnostics: () => Promise<void> = underTauri
  ? reportTauriDiagnostics
  : async () => {};

/**
 * Pipe the frontend's `console.error` + uncaught errors/rejections into the native log.
 * Release WebView2 has no visible console, so this is the only way a friend's crash reaches a log
 * file. Call once, early, at startup. No-op in the browser build.
 */
export const installFrontendLogPipe: () => void = underTauri
  ? tauriInstallFrontendLogPipe
  : () => {};

/**
 * Close guard (native builds): register a callback for the vetoed OS window close. Rust intercepts
 * `CloseRequested`, prevents it, and emits an event; the app confirms with the user (jam in
 * progress?) and calls `confirmNativeClose` to really close. No-op in the browser build — there the
 * app uses the standard `beforeunload` veto instead.
 */
export const onNativeCloseRequested: (cb: () => void) => void = underTauri
  ? tauriOnCloseRequested
  : () => {};

/** Approve the close: Rust lifts its guard and closes the window. No-op in the browser build. */
export const confirmNativeClose: () => Promise<void> = underTauri
  ? tauriConfirmClose
  : async () => {};

// ── The engine's command queue ─────────────────────────────────────────────────────────────────────

let outbox: EngineCommand[] = [];
/** The outbox's batch: whether the host took it, once it is flushed. */
let submitted: Promise<boolean> = Promise.resolve(true);

/**
 * Queue commands for the engine. What one task sends leaves as ONE ordered `engine_send` batch a
 * microtask later, so a gesture's commands reach the same block together. A batch the host could not take
 * is logged and toasted. Nothing is sent while the platform has no engine (the browser build without the
 * DEV fake).
 *
 * Resolves with the SUBMISSION of the batch these commands left in: true once the host took all of it,
 * false when it did not wholly take it (or there is no engine). False is no per-command answer: the
 * native host pushes a batch command by command and stops at the first it refuses, keeping the prefix it
 * took, so a command of a failed batch may still reach the engine. Never an acknowledgement either: the
 * engine applies a command later, and its outcome arrives on the feed. Most callers ignore it; a mix
 * gesture drops its overlay on false, and a command the engine did take brings the value back through
 * its lane's `Mix` (`engine-store.ts`).
 */
export function sendEngine(...commands: EngineCommand[]): Promise<boolean> {
  if (!platform.engine.available) return Promise.resolve(false);
  if (commands.length === 0) return Promise.resolve(true);
  if (outbox.length === 0) submitted = new Promise((resolve) => queueMicrotask(() => resolve(flushEngine())));
  outbox.push(...commands);
  return submitted;
}

async function flushEngine(): Promise<boolean> {
  const batch = outbox;
  outbox = [];
  try {
    await platform.engine.send(batch);
    return true;
  } catch (err) {
    console.error('[platform] engine command batch failed', err);
    notifyError('The audio engine did not take a command', err);
    return false;
  }
}

/** The web engine fake a probe scripts through `__lf.native`; null under Tauri. */
export const engineFake: EngineFake | null = underTauri ? null : webEngineFake;

export * from './host';
export * from './engine-wire';
