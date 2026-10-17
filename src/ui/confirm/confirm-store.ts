/**
 * The app's own yes/no ask: `confirm` shows it (`Confirm.tsx` renders it, mounted once in `App` beside
 * the toasts) and resolves with the answer. It replaces `window.confirm`, a WebView2 OS dialog that
 * wears none of the app's face and sits outside its focus order. One ask shows at a time; a second
 * waits for the first's answer. The close guard keeps its native confirms on purpose: the close path
 * needs a synchronous answer (`src/app/close-guard.ts`).
 *
 * A plain `.ts` leaf (`solid-js` only), so a store that asks still loads in plain Node under the verify
 * hooks.
 */
import { createSignal } from 'solid-js';

export interface ConfirmAsk {
  /** The dialog's name (its accessible label). */
  title: string;
  /** The question, as prose; a blank line starts a paragraph. */
  body: string;
  /** The button that answers yes. */
  yes: string;
  /** The button that answers no: it takes the focus, and Escape and a click outside answer it. */
  no: string;
}

export interface PendingConfirm extends ConfirmAsk {
  answer: (yes: boolean) => void;
}

const [pending, setPending] = createSignal<PendingConfirm | null>(null);
/** The ask on screen, or null. */
export const pendingConfirm = pending;

let tail: Promise<unknown> = Promise.resolve();

/** A rig's stand-in for the player: set, it answers every ask and nothing shows (a scripted run that
 * does not test the ask, as `src/debug/recall-restart.ts`). */
export const confirmSeam: { answer: ((ask: ConfirmAsk) => boolean) | null } = { answer: null };

/** Ask the player; resolves true for yes. */
export function confirm(ask: ConfirmAsk): Promise<boolean> {
  if (confirmSeam.answer) return Promise.resolve(confirmSeam.answer(ask));
  const answered = tail.then(
    () =>
      new Promise<boolean>((resolve) => {
        let open = true; // a second answer (a key and a click at once) must not close the next ask
        setPending({
          ...ask,
          answer: (yes) => {
            if (!open) return;
            open = false;
            setPending(null);
            resolve(yes);
          },
        });
      }),
  );
  tail = answered;
  return answered;
}
