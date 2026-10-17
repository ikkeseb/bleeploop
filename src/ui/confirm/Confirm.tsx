import { For, Show, onCleanup, onMount } from 'solid-js';
import { pendingConfirm, type PendingConfirm } from './confirm-store';
import './confirm.css';

/**
 * The ask `confirm-store.ts` holds, as a modal alert dialog over the whole window: the native `confirm` it
 * replaces blocked the page, so this one takes every key and click until it is answered. Focus lands on
 * the no button; Escape and a click outside the panel answer no; Enter and Space press the focused
 * button; Tab and Shift+Tab move between the two buttons (the focused one wears a lit face, so Enter's target shows without a key ring) and never leave. Answered, focus goes back to
 * where it was. A key is taken in the window's capture phase, before the transport keys, the note keys
 * and the popovers' Escape see it, so an ask opened from Audio Settings leaves that panel open.
 */
export function Confirm() {
  return <Show when={pendingConfirm()}>{(ask) => <ConfirmDialog ask={ask()} />}</Show>;
}

function ConfirmDialog(props: { ask: PendingConfirm }) {
  let no!: HTMLButtonElement;
  let yes!: HTMLButtonElement;
  const back = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  const onKeyDown = (e: KeyboardEvent) => {
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
    no.focus();
  });
  onCleanup(() => {
    window.removeEventListener('keydown', onKeyDown, { capture: true });
    if (back?.isConnected && back !== document.body) back.focus();
  });
  return (
    <div class="confirm" onClick={() => props.ask.answer(false)}>
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
