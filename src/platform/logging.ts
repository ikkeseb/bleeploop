/**
 * Frontend → native log pipe ("field debuggability"). A release WebView2 surfaces no
 * console, so a friend's `console.error` — and any uncaught exception or promise rejection, which is
 * exactly what "it broke" looks like — would otherwise vanish. This wraps `console.error` and the
 * global error/rejection events, forwards a serialized string to the `frontend_log` Rust command
 * (→ the same rotated log file as the native side), and NEVER changes the original console behaviour
 * (the real `console.error` runs first, unmodified). Tauri-only; the browser build installs a no-op
 * (see `index.ts`).
 */
import { invoke } from '@tauri-apps/api/core';

/** Install-once guard so a second call is a no-op. */
let installed = false;

/**
 * Re-entrancy guard. Anything that runs *inside* the pipe (serialization, the failing invoke) must
 * not recurse back through the wrapped `console.error` and re-enter the sink — a runaway loop that
 * could flood the log or hang the app. While this is true the wrapper only calls through to the
 * original console and forwards nothing.
 */
let inPipe = false;

/** Hard cap on a forwarded message so a huge object dump can't bloat the log file. */
const MAX_LEN = 4000;

/** Serialize one console/error argument to a string without throwing. */
function serializeArg(arg: unknown): string {
  if (arg instanceof Error) return arg.stack ?? String(arg);
  if (typeof arg === 'string') return arg;
  try {
    // Prefer a String() coercion (cheap, keeps primitives readable); fall back to JSON for plain
    // objects. Either can throw (getters, circular refs), so the whole thing is guarded.
    const s = String(arg);
    if (s === '[object Object]') return JSON.stringify(arg);
    return s;
  } catch {
    try {
      return JSON.stringify(arg);
    } catch {
      return '<unserializable>';
    }
  }
}

/** Join + truncate, then fire-and-forget to the native sink. Its own failure is swallowed silently. */
function forward(parts: string[]): void {
  let message = parts.join(' ');
  if (message.length > MAX_LEN) message = `${message.slice(0, MAX_LEN)}…`;
  // .catch is mandatory: a rejected invoke must NEVER log its own failure (that would recurse) and
  // must not surface as an unhandled rejection (which our own listener would then re-forward).
  void invoke('frontend_log', { message }).catch(() => {});
}

export function tauriInstallFrontendLogPipe(): void {
  if (installed) return;
  installed = true;

  const original = console.error.bind(console);
  console.error = (...args: unknown[]): void => {
    original(...args); // original behaviour first, always, unchanged
    if (inPipe) return;
    inPipe = true;
    try {
      forward(args.map(serializeArg));
    } catch {
      // The pipe must NEVER let a serialization/invoke throw escape into the code that logged —
      // this wrapper's contract (header) is to leave console.error's behaviour unchanged. Swallow
      // silently; do NOT re-log from here (that would recurse, same rule as forward()'s .catch).
    } finally {
      inPipe = false;
    }
  };

  window.addEventListener('error', (e: ErrorEvent) => {
    if (inPipe) return;
    inPipe = true;
    try {
      const detail = e.error instanceof Error ? (e.error.stack ?? String(e.error)) : e.message;
      forward(['[window.error]', detail]);
    } catch {
      // The pipe must never throw (listener throws are already contained by the event loop, but keep
      // all three handlers consistent — and inPipe still resets in finally).
    } finally {
      inPipe = false;
    }
  });

  // A CSP block (tauri.conf.json `security.csp`, release builds only — the dev server is not covered)
  // is a browser-generated console message, not a `console.error` call, so nothing above sees it.
  // Without this line a blocked worklet or asset is a silent release-only failure.
  document.addEventListener('securitypolicyviolation', (e: SecurityPolicyViolationEvent) => {
    if (inPipe) return;
    inPipe = true;
    try {
      forward(['[csp]', e.violatedDirective, e.blockedURI, `${e.sourceFile ?? ''}:${e.lineNumber}`]);
    } catch {
      // The pipe must never throw (see the console.error wrapper above) — inPipe still resets in finally.
    } finally {
      inPipe = false;
    }
  });

  window.addEventListener('unhandledrejection', (e: PromiseRejectionEvent) => {
    if (inPipe) return;
    inPipe = true;
    try {
      const r = e.reason;
      const detail = r instanceof Error ? (r.stack ?? String(r)) : serializeArg(r);
      forward(['[unhandledrejection]', detail]);
    } catch {
      // The pipe must never throw (see the console.error wrapper above) — inPipe still resets in finally.
    } finally {
      inPipe = false;
    }
  });
}
