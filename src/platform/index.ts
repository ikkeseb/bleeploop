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
import {
  releasePluginBuffer as tauriReleasePluginBuffer,
  reportTauriDiagnostics,
  setPluginBufferSink,
  tauriConfirmClose,
  tauriOnCloseRequested,
  tauriPlatform,
} from './host.tauri';

const underTauri = isTauri();

export const platform: Platform = underTauri ? tauriPlatform : webPlatform;

/**
 * Detach + free a plugin SharedBuffer's JS view. The native counterpart to the host's
 * `Close()`; the audio bridge calls this on teardown. No-op in the browser build.
 */
export const releasePluginBuffer: (ab: ArrayBuffer) => void = underTauri
  ? tauriReleasePluginBuffer
  : () => {};

/**
 * Register the audio sink for posted plugin SharedBuffers. The platform layer owns the
 * WebView2 `sharedbufferreceived` event and forwards a plain ArrayBuffer + meta to this sink
 * (→ `src/audio/plugin-bridge.ts`). No-op in the browser build.
 */
export const registerPluginBufferSink: (sink: (ab: ArrayBuffer, meta: unknown) => void) => void =
  underTauri ? setPluginBufferSink : () => {};

/**
 * DEV-only: report WebView2-internal facts (crossOriginIsolated, MIDI, …) to `tauri dev` stdout —
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

// ── Engine mode (`docs/plans/native-engine.md` § Stage 5) ────────────────────────────────────────

let engineOn = false;

/**
 * Read the engine toggle once, before the app renders (`src/main.tsx`). The toggle applies on restart,
 * never live, so every later `engineMode()` returns this answer. A host that cannot answer runs the
 * web path.
 */
export async function resolveEngineMode(): Promise<boolean> {
  if (!platform.engine.available) return false;
  try {
    engineOn = await platform.engine.mode();
  } catch (err) {
    console.error('[platform] engine mode query failed; this launch runs the web audio path', err);
    engineOn = false;
  }
  return engineOn;
}

/** Whether this launch runs on the native engine. Fixed once `resolveEngineMode` settled. */
export function engineMode(): boolean {
  return engineOn;
}

let outbox: EngineCommand[] = [];

/**
 * Queue commands for the engine. What one task sends leaves as ONE ordered `engine_send` batch a
 * microtask later, so a gesture's commands reach the same block together. Fire-and-forget: a batch the
 * host could not take is logged and toasted. Nothing is sent outside engine mode.
 */
export function sendEngine(...commands: EngineCommand[]): void {
  if (!engineOn || commands.length === 0) return;
  if (outbox.length === 0) queueMicrotask(flushEngine);
  outbox.push(...commands);
}

function flushEngine(): void {
  const batch = outbox;
  outbox = [];
  platform.engine.send(batch).catch((err: unknown) => {
    console.error('[platform] engine command batch failed', err);
    notifyError('The audio engine did not take a command', err);
  });
}

/** The web engine fake a probe scripts through `__lf.native`; null under Tauri. */
export const engineFake: EngineFake | null = underTauri ? null : webEngineFake;

export * from './host';
export * from './engine-wire';
export { markerProbeNative } from './marker-probe';
