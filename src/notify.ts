/**
 * Framework-level error-notification store — the ONE user-visible surface for failures that would
 * otherwise vanish into `console.error`. A release WebView2 build has no visible dev
 * console, so a user who hits a plugin-load / arm / device failure sees nothing without this. Every
 * wired call site keeps its existing `console.error` (`src/platform/logging.ts` pipes those into the
 * release log file — load-bearing diagnostics), and adds ONE `notifyError` alongside it.
 *
 * BOUNDARY: this file lives at `src/` ROOT — deliberately NOT under `src/audio/`, `src/ui/`, nor
 * `src/platform/` — so BOTH `src/audio/` and `src/platform/` may import it without tripping the
 * capability-boundary guard (`scripts/check-boundary.mjs`): platform/ may not import ../audio or
 * ../ui, and this module is neither. It imports ONLY `solid-js` — no audio/ui/platform deps — so it
 * stays a leaf every layer can safely depend on. `<Toasts>` (in `src/ui/`) renders the signal.
 */
import { createSignal } from 'solid-js';

export interface Toast {
  id: number;
  message: string;
  detail?: string;
  /** Repeat count — a duplicate message increments this instead of stacking a second toast. */
  count: number;
}

/** How long a toast lingers before auto-dismiss (ms). A repeat resets its timer. */
const AUTO_DISMISS_MS = 8000;
/** Hard ceiling on a toast's TOTAL lifetime (ms). The dedupe timer-reset must not let a repeating
 * failure (e.g. a spammy per-frame error) camp over the bottom-right looper strip forever during
 * live play — past this age the toast dies no matter how often its message repeats. */
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
let nextId = 1;

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

function arm(id: number): void {
  clearTimer(id);
  // The linger window, but never past the toast's hard lifetime ceiling — a dedupe reset extends
  // the 8 s window, not the toast's total stage time.
  const born = bornAt.get(id) ?? Date.now();
  const remaining = Math.max(0, born + HARD_MAX_MS - Date.now());
  timers.set(
    id,
    setTimeout(() => dismissToast(id), Math.min(AUTO_DISMISS_MS, remaining)),
  );
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
}

/**
 * Surface a user-visible error toast. `message` is the short user-readable headline; `detail` is the
 * caught value (Error/string/anything), serialized safely for display.
 *
 * DEDUPE: if a visible toast already carries the same `message`, its count is incremented, its detail
 * replaced with the newest, and its auto-dismiss timer RESET — so repeated arm failures (a spammy
 * retry loop) collapse into one "×N" toast instead of stacking four identical ones. Otherwise the
 * toast is pushed; past MAX_VISIBLE the oldest is dropped (its timer cleared).
 */
export function notifyError(message: string, detail?: unknown): void {
  const text = serializeDetail(detail);
  const current = toasts();
  const existing = current.find((t) => t.message === message);
  if (existing) {
    setToasts(
      current.map((t) =>
        t.id === existing.id ? { ...t, count: t.count + 1, detail: text } : t,
      ),
    );
    arm(existing.id); // reset the linger window on repeat
    return;
  }
  const toast: Toast = { id: nextId++, message, detail: text, count: 1 };
  bornAt.set(toast.id, Date.now());
  let next = [...current, toast];
  while (next.length > MAX_VISIBLE) {
    const dropped = next[0];
    forget(dropped.id);
    next = next.slice(1);
  }
  setToasts(next);
  arm(toast.id);
}

/** Dismiss a toast by id (user click / keyboard, or the auto-dismiss timer). Clears its timer too, so
 * no handle leaks and the id can't be re-dismissed. */
export function dismissToast(id: number): void {
  forget(id);
  setToasts((prev) => prev.filter((t) => t.id !== id));
}
