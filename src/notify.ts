/**
 * Framework-level error-notification store — the ONE user-visible surface for failures that would
 * otherwise vanish into `console.error`. A release WebView2 build has no visible dev
 * console, so a user who hits a plugin-load / arm / device failure sees nothing without this. Every
 * wired call site keeps its existing `console.error` (`src/platform/logging.ts` pipes those into the
 * release log file — load-bearing diagnostics), and adds ONE `notifyError` alongside it.
 *
 * BOUNDARY: this file lives at `src/` ROOT — deliberately NOT under an app layer (`src/ui/`,
 * `src/session/`, …) nor `src/platform/` — so every layer, `src/platform/` too, may import it without
 * tripping the capability-boundary guard (`verify/guards/boundary.mjs`): platform/ may not import the
 * app layers, and this module is none. It imports ONLY `solid-js` — no ui/session/platform deps — so it
 * stays a leaf every layer can safely depend on. `<Toasts>` (in `src/ui/`) renders the signal.
 */
import { createSignal, type Setter } from 'solid-js';

export interface Toast {
  id: number;
  /** `info` confirms a finished action, `note` tells something to know or do (neutral), `error` a failure. */
  kind: 'error' | 'info' | 'note';
  message: string;
  /** Reactive, like `count`: the newest repeat's detail. */
  readonly detail?: string;
  /** One thing the player can do about it from the toast (`toast/Toasts.tsx` renders a button): e.g.
   * open Audio Settings on the page the message names. A repeat keeps the first toast's. */
  readonly action?: ToastAction;
  /** Repeat count — a duplicate message increments this instead of stacking a second toast. Reactive
   * (a getter over the toast's own signal): a repeat updates the toast in place, never replaces it,
   * so `<For>` keeps its row and a focused Dismiss keeps keyboard focus. */
  readonly count: number;
}

export interface ToastAction {
  label: string;
  run: () => void;
}

interface Repeat {
  count: number;
  detail?: string;
}

/** How long a toast lingers before auto-dismiss (ms). A repeat resets its timer. */
const AUTO_DISMISS_MS = 8000;
const FOCUS_DEFER_MS = 2000;
/** Hard ceiling on an unfocused toast's TOTAL lifetime (ms). The dedupe timer-reset must not let a
 * repeating failure (e.g. a spammy per-frame error) camp over the bottom-right looper strip forever
 * during live play. Keyboard focus defers removal past this ceiling until it leaves the toast stack. */
const HARD_MAX_MS = 30000;
/** Cap on visible toasts — the oldest is dropped past this so a burst of failures can't wall the app. */
const MAX_VISIBLE = 4;
/** Truncate serialized detail to keep the surface quiet (a full stack / long string is dev-log's job). */
const DETAIL_MAX = 140;

const [toasts, setToasts] = createSignal<Toast[]>([]);
/** Per-toast auto-dismiss timer handles, keyed by id — cleared on EVERY removal path (no leaks, and a
 * recycled id can't be dismissed by a stale timer). Kept outside the signal (not render state). */
const timers = new Map<number, ReturnType<typeof setTimeout>>();
/** Per-toast birth timestamp (ms), for the HARD_MAX_MS lifetime ceiling. Cleaned with the timer. */
const bornAt = new Map<number, number>();
/** Per-toast setter behind its `count` and `detail` getters. Cleaned with the timer. */
const repeats = new Map<number, Setter<Repeat>>();
let nextId = 1;
let autoDismissMs = AUTO_DISMISS_MS;

/** Read-only signal getter: the currently-visible toasts (oldest → newest). */
export { toasts };

/** Serialize an arbitrary caught value into a short, user-safe detail line. An `Error` yields its
 * MESSAGE (never the stack — this is a user surface, not the dev log); a string passes through; any
 * other value is `String()`-ed defensively. Truncated to DETAIL_MAX. Returns undefined for no detail. */
function serializeDetail(detail: unknown): string | undefined {
  if (detail == null) return undefined;
  try {
    // String(detail.message) — not a bare read — because a tampered/foreign Error can carry a
    // non-string `message` at runtime (lib.d.ts can't see it), and the trim/slice below must not
    // throw out of notifyError and change the wired catch block's semantics. Everything stays
    // inside the try for the same reason.
    let s: string;
    if (detail instanceof Error) s = String(detail.message);
    else if (typeof detail === 'string') s = detail;
    else s = String(detail);
    s = s.trim();
    if (!s) return undefined;
    return s.length > DETAIL_MAX ? s.slice(0, DETAIL_MAX - 1) + '…' : s;
  } catch {
    return undefined; // a throwing toString() must never break the notification itself
  }
}

function autoDismissToast(id: number): void {
  const stack = document.querySelector<HTMLElement>('.toasts');
  if (stack?.contains(document.activeElement)) {
    clearTimer(id);
    timers.set(id, setTimeout(() => autoDismissToast(id), FOCUS_DEFER_MS));
    return;
  }
  dismissToast(id, true);
}

function arm(id: number): void {
  clearTimer(id);
  // The linger window, but never past the toast's hard lifetime ceiling — a dedupe reset extends
  // the 8 s window, not the toast's total stage time.
  const born = bornAt.get(id) ?? Date.now();
  const remaining = Math.max(0, born + HARD_MAX_MS - Date.now());
  timers.set(
    id,
    setTimeout(() => autoDismissToast(id), Math.min(autoDismissMs, remaining)),
  );
}

/** @public Runtime-probe control for the toast linger window. New toasts use this value. */
export function setAutoDismissMsForProbe(ms: number): void {
  autoDismissMs = Number.isFinite(ms) ? Math.max(0, ms) : AUTO_DISMISS_MS;
}

/** Clears ONLY the timer (arm() re-arms through here, so the birth timestamp must survive it —
 * removal paths delete `bornAt` themselves via forget()). */
function clearTimer(id: number): void {
  const h = timers.get(id);
  if (h !== undefined) {
    clearTimeout(h);
    timers.delete(id);
  }
}

/** Full bookkeeping cleanup for a removed toast (every removal path). */
function forget(id: number): void {
  clearTimer(id);
  bornAt.delete(id);
  repeats.delete(id);
}

/** Whether keyboard focus sits inside toast `id` (the store also runs where there is no document). */
function holdsFocus(id: number): boolean {
  const toast = globalThis.document?.querySelector<HTMLElement>(`[data-toast-id="${id}"]`);
  return toast?.contains(document.activeElement) ?? false;
}

/**
 * Surface a user-visible error toast. `message` is the short user-readable headline; `detail` is the
 * caught value (Error/string/anything), serialized safely for display.
 *
 * DEDUPE: if a visible toast already carries the same `message`, its count is incremented, its detail
 * replaced with the newest, and its auto-dismiss timer RESET — so repeated arm failures (a spammy
 * retry loop) collapse into one "×N" toast instead of stacking four identical ones. Otherwise the
 * toast is pushed; past MAX_VISIBLE the oldest one keyboard focus is not in is dropped (its timer
 * cleared).
 */
export function notifyError(message: string, detail?: unknown, action?: ToastAction): void {
  push('error', message, detail, action);
}

/** A success/confirmation toast (e.g. "export saved") — same store, dedupe and lifetime as an error,
 * rendered with the calm accent. NOT a logging surface: nothing here replaces a `console.error`. */
export function notifyInfo(message: string, detail?: unknown, action?: ToastAction): void {
  push('info', message, detail, action);
}

/** A neutral note: something the player should know or do that is neither a failure nor a finished
 * action (a kept recovery, an update on offer). Same store, dedupe and lifetime; no ✓, no "Done". */
export function notifyNote(message: string, detail?: unknown, action?: ToastAction): void {
  push('note', message, detail, action);
}

function push(kind: Toast['kind'], message: string, detail: unknown, action?: ToastAction): void {
  const text = serializeDetail(detail);
  const current = toasts();
  const existing = current.find((t) => t.message === message && t.kind === kind);
  if (existing) {
    repeats.get(existing.id)?.((r) => ({ count: r.count + 1, detail: text }));
    arm(existing.id); // reset the linger window on repeat
    return;
  }
  const [repeat, setRepeat] = createSignal<Repeat>({ count: 1, detail: text });
  const toast: Toast = {
    id: nextId++,
    kind,
    message,
    action,
    get count() {
      return repeat().count;
    },
    get detail() {
      return repeat().detail;
    },
  };
  bornAt.set(toast.id, Date.now());
  repeats.set(toast.id, setRepeat);
  let next = [...current, toast];
  while (next.length > MAX_VISIBLE) {
    // The oldest goes, unless keyboard focus sits in it: the auto-dismiss defers for focus the same
    // way, and removing the focused toast would drop focus on <body>, where Space is REC. The new
    // toast is not rendered yet, so one is always free to go.
    const dropped = next.find((t) => !holdsFocus(t.id)) ?? next[0];
    forget(dropped.id);
    next = next.filter((t) => t !== dropped);
  }
  setToasts(next);
  arm(toast.id);
}

/** Dismiss a toast by id (user click / keyboard, or the auto-dismiss timer). Clears its timer too, so
 * no handle leaks and the id can't be re-dismissed. */
export function dismissToast(id: number, restoreTimerFocus = false): void {
  let focusTarget: HTMLButtonElement | null = null;
  if (restoreTimerFocus) {
    const toast = document.querySelector<HTMLElement>(`[data-toast-id="${id}"]`);
    if (toast?.contains(document.activeElement)) {
      const closes = [...document.querySelectorAll<HTMLButtonElement>('.toast__close')];
      const ownClose = toast.querySelector<HTMLButtonElement>('.toast__close');
      const index = ownClose ? closes.indexOf(ownClose) : -1;
      focusTarget =
        closes[index + 1] ??
        closes[index - 1] ??
        document.querySelector<HTMLButtonElement>('.cmd button:not(:disabled)');
    }
  }
  forget(id);
  setToasts((prev) => prev.filter((t) => t.id !== id));
  if (focusTarget) queueMicrotask(() => focusTarget?.focus());
}
