/**
 * Ambient typing for WebView2's `chrome.webview` host object (the shared-buffer plugin transport).
 *
 * WebView2 ships no official `@types` package for this, so we declare the slice we use. `chrome`
 * and `webview` are optional: in a plain browser / under Playwright they're absent, matching the
 * runtime `isTauri()` selection in `src/platform/`. This file is pure web typing (no `@tauri-apps/*`
 * import, no `src/audio` | `src/ui` import), so it doesn't trip `check:boundary`.
 */
export {};

declare global {
  /** Event from `chrome.webview` when the host calls `ICoreWebView2_17::PostSharedBufferToScript`. */
  interface WebView2SharedBufferReceivedEvent extends Event {
    /** Parsed object from the host's `additionalDataAsJson`; `undefined` if none was sent. */
    readonly additionalData: unknown;
    /**
     * The buffer's bytes as a regular `ArrayBuffer` (NOT a `SharedArrayBuffer`) over the same
     * cross-process OS shared memory. Note: `Atomics` are NOT available on a non-shared
     * ArrayBuffer in Chromium 149 — read it with plain typed-array views.
     */
    getBuffer(): ArrayBuffer;
  }

  interface WebView2 extends EventTarget {
    addEventListener(
      type: 'sharedbufferreceived',
      listener: (this: WebView2, ev: WebView2SharedBufferReceivedEvent) => unknown,
      options?: boolean | AddEventListenerOptions,
    ): void;
    addEventListener(
      type: 'message',
      listener: (this: WebView2, ev: MessageEvent) => unknown,
      options?: boolean | AddEventListenerOptions,
    ): void;
    postMessage(message: unknown): void;
    /** Detaches the ArrayBuffer and frees the underlying shared memory. */
    releaseBuffer(buffer: ArrayBuffer): void;
  }

  interface Window {
    chrome?: { webview?: WebView2 };
  }
}
