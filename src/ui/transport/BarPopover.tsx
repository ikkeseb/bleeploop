import { Show, createEffect, createSignal, type JSX } from 'solid-js';
import { Portal } from 'solid-js/web';
import { stageOpen } from '../stage/stage-store';
import { EscapeCloses } from '../looper/shared';
import './bar-popover.css';

/**
 * A command-bar pill's popover: IN FX's sends, CLICK's volume, AUTO REC's sensitivity. Non-modal, like
 * the bar's other popovers (app.tsx): focus moves into the panel on open, Escape or a click outside
 * closes it, and a keyboard close hands focus back to the pill on the app's behalf (`returnFocus`, the
 * transport keys', so Space after Escape still arms REC); the play path stays live behind it. The panel
 * hangs under the pill that opened it (`bar-popover.css`).
 */
export interface BarPopover {
  open: () => boolean;
  toggle: () => void;
  close: (keyboard: boolean) => void;
  /** The trigger's ref: focus returns to it. */
  trigger: (el: HTMLButtonElement) => void;
}

/** The one bar popover that is open, if any. The panels share one anchor (`bar-popover.css`), so two open
 * at once would stack at one spot and leave one outside click closing only one: opening a popover closes
 * the other, which unmounts its backdrop and releases its Escape handler. */
const [owner, setOwner] = createSignal<symbol | null>(null);

export function createBarPopover(returnFocus?: (el: HTMLElement | undefined) => void): BarPopover {
  const me = Symbol('bar-popover');
  const open = (): boolean => owner() === me;
  const setOpen = (v: boolean): void => {
    if (v) setOwner(me);
    else if (owner() === me) setOwner(null);
  };
  let trigger: HTMLButtonElement | undefined;
  // The stage view covers the command bar but not this portal: left open, its controls stay focusable
  // behind the stage and take the transport keys. Opening the stage closes it, as it does Settings and
  // Help (app.tsx).
  createEffect(() => {
    if (stageOpen()) setOpen(false);
  });
  return {
    open,
    toggle: () => setOpen(!open()),
    close: (keyboard) => {
      setOpen(false);
      if (keyboard) returnFocus?.(trigger);
    },
    trigger: (el) => (trigger = el),
  };
}

/** The panel. Its trigger carries `bar-pop__trigger`, `aria-haspopup="dialog"`, `aria-expanded` and
 * `aria-controls={id}`. */
export function BarPopoverPanel(props: { pop: BarPopover; id: string; label: string; class: string; children: JSX.Element }) {
  return (
    <Show when={props.pop.open()}>
      <EscapeCloses close={() => props.pop.close(true)} />
      <Portal>
        <div class="settings-popover__backdrop" onClick={() => props.pop.close(false)}>
          <div
            class={`bar-pop ${props.class}`}
            id={props.id}
            role="dialog"
            aria-label={props.label}
            tabindex={-1}
            ref={(el) => queueMicrotask(() => el.focus())}
            onClick={(e) => e.stopPropagation()}
          >
            {props.children}
          </div>
        </div>
      </Portal>
    </Show>
  );
}
