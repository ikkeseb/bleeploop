import { For, Show, createEffect, onCleanup, onMount } from 'solid-js';
import { pendingConfirm, type PendingConfirm } from './confirm-store';
import './confirm.css';

/**
 * The ask `confirm-store.ts` holds, as a modal alert dialog over the whole window: the native `confirm` it
 * replaces blocked the page, so this one takes every key and click until it is answered. Focus lands on
 * the no button; Escape and a click outside the panel answer no; Enter and Space press the focused
 * button; Tab and Shift+Tab move between the two buttons (the focused one wears a lit face, so Enter's
 * target shows without a key ring). A key is taken in the window's capture phase, before the transport
 * keys (B's stage view among them), the note keys and the popovers' Escape see it, so an ask opened from
 * Audio Settings leaves that panel open. Focus stays on a button for the ask's whole life: what moves it
 * elsewhere (a learned pedal opening the stage view, which focuses its own root) is pulled back.
 *
 * Answered, focus goes back where it was, except from a <select> (the rate pick's): it goes to the panel
 * around it (tabindex -1, which keeps no keys) or nowhere. A select the pointer picked carries a mark
 * that hands the next key to the transport (`src/app/transport-keys.ts`); the ask's focus ended it, and
 * focused again without it the select would keep the transport's and the note keys. Which hand picked
 * cannot be read here (the list's own keys make a mouse-opened select match `:focus-visible`, measured
 * in the sample-rate probe), so every select gives way: a keyboard player is one Tab from the panel's
 * controls again.
 *
 * `suspended` (the tuner is on, `src/app.tsx`): the ask steps aside, its question kept: not shown, inert,
 * taking no key and no focus, so the tuner's Escape and the transport keys reach it. When it ends the ask
 * is back as it was, focus on its button. An ask that arrives suspended takes focus when it ends, and
 * learns where to send it back from the first focus move it pulls in (the app's return from the tuner).
 */
export function Confirm(props: { suspended: boolean }) {
  return <Show when={pendingConfirm()}>{(ask) => <ConfirmDialog ask={ask()} suspended={props.suspended} />}</Show>;
}

/** Where focus goes back to once the ask is answered, from where it was. */
function backFrom(at: Element | null): HTMLElement | null {
  if (!(at instanceof HTMLElement)) return null;
  return at instanceof HTMLSelectElement ? at.closest<HTMLElement>('[tabindex="-1"]') : at;
}

function ConfirmDialog(props: { ask: PendingConfirm; suspended: boolean }) {
  let root!: HTMLDivElement;
  let no!: HTMLButtonElement;
  let yes!: HTMLButtonElement;
  const late = props.suspended; // mounted under the tuner: `back` is learned when it ends
  let back = late ? null : backFrom(document.activeElement);
  let learn = late;
  let held: HTMLButtonElement | undefined; // the button focus belongs on
  const onFocusIn = (e: FocusEvent) => {
    if (props.suspended) return;
    if (e.target === yes) held = yes;
    else if (e.target === no) held = no;
    else {
      if (learn && e.target instanceof Node && !root.contains(e.target)) {
        learn = false;
        back = backFrom(e.target as Element);
      }
      (held ?? no).focus();
    }
  };
  const onKeyDown = (e: KeyboardEvent) => {
    if (props.suspended) return;
    e.stopPropagation();
    if (e.key === 'Escape') {
      e.preventDefault();
      props.ask.answer(false);
    } else if (e.key === 'Tab') {
      e.preventDefault();
      (document.activeElement === no ? yes : no).focus();
    }
  };
  onMount(() => {
    window.addEventListener('keydown', onKeyDown, { capture: true });
    document.addEventListener('focusin', onFocusIn, { capture: true });
    if (!props.suspended) no.focus();
  });
  // Back from suspension: focus returns to the button that had it (or no, for an ask that arrived suspended).
  createEffect<boolean>((was) => {
    const now = props.suspended;
    if (was && !now) (held ?? no).focus();
    return now;
  }, late);
  onCleanup(() => {
    window.removeEventListener('keydown', onKeyDown, { capture: true });
    document.removeEventListener('focusin', onFocusIn, { capture: true });
    if (!props.suspended && back?.isConnected && back !== document.body) back.focus();
  });
  return (
    <div
      class="confirm"
      classList={{ 'confirm--suspended': props.suspended }}
      inert={props.suspended}
      ref={root}
      onClick={() => props.ask.answer(false)}
    >
      <div
        class="confirm__panel"
        role="alertdialog"
        aria-modal="true"
        aria-labelledby="lf-confirm-title"
        aria-describedby="lf-confirm-body"
        tabindex={-1}
        onClick={(e) => e.stopPropagation()}
      >
        <div class="confirm__title" id="lf-confirm-title">
          {props.ask.title}
        </div>
        <div class="confirm__body" id="lf-confirm-body">
          <For each={props.ask.body.split('\n\n')}>{(para) => <p>{para}</p>}</For>
        </div>
        <div class="confirm__actions">
          {/* Windows' order, as the native dialog had it: yes first. */}
          <button type="button" class="confirm__btn" ref={yes} onClick={() => props.ask.answer(true)}>
            {props.ask.yes}
          </button>
          <button type="button" class="confirm__btn" ref={no} onClick={() => props.ask.answer(false)}>
            {props.ask.no}
          </button>
        </div>
      </div>
    </div>
  );
}
