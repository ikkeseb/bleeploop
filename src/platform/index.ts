/**
 * Runtime platform selection. `@tauri-apps/api` is pure JS, so importing it in a browser is safe
 * — `isTauri()` returns false and we hand back the web implementation. Under the Tauri shell
 * it returns true and we hand back `tauriPlatform`; nothing else in the app changes.
 */
import { isTauri } from '@tauri-apps/api/core';
import type { Platform } from './host';
import { tauriInstallFrontendLogPipe } from './logging';
import { webPlatform } from './host.web';
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

export * from './host';
export { markerProbeNative } from './marker-probe';
