import { Show, createEffect, createSignal, type JSX } from 'solid-js';
import { Portal } from 'solid-js/web';
import { stageOpen } from '../stage/stage-store';
import { EscapeCloses } from '../looper/shared';

/**
 * A slot-row popover: a plate's menu, an input effect's parameters, a folded slot's or folded IN FX's
 * line. Non-modal, as the command bar's popovers: focus moves into the panel on open, Escape or a click
 * outside closes it, and a keyboard close hands focus back to its trigger on the app's behalf
 * (`returnFocus`, the transport keys', so Space after Escape still arms REC); a Tab in a menu closes it and
 * moves on from its trigger, in its own direction, and a menu closes when the focus leaves it. One is open
 * at a time in the row: they share one anchor (`slot.css`, `--slot-pop` on the open trigger), and opening one closes
 * the other. The panel hangs under its trigger (anchor positioning), flipping to the trigger's right
 * edge near the window's.
 */
export interface RowPopover {
  open: () => boolean;
  toggle: () => void;
  close: (keyboard: boolean) => void;
  /** Close on a Tab: the focus goes to the trigger as a plain control (Space and Enter are its own), and the
   * browser's Tab moves on from it, in its own direction. */
  leave: () => void;
  /** The trigger's ref: focus returns to it. */
  trigger: (el: HTMLElement) => void;
}

const [owner, setOwner] = createSignal<symbol | null>(null);

export function createRowPopover(returnFocus?: (el: HTMLElement | undefined) => void): RowPopover {
  const me = Symbol('row-popover');
  const open = (): boolean => owner() === me;
  const setOpen = (v: boolean): void => {
    if (v) setOwner(me);
    else if (owner() === me) setOwner(null);
  };
  let trigger: HTMLElement | undefined;
  // The stage view covers the row but not this portal: opening it closes the popover (as the bar's).
  createEffect(() => {
    if (stageOpen()) setOpen(false);
  });
  return {
    open,
    toggle: () => setOpen(!open()),
    close: (keyboard) => {
      setOpen(false);
      if (keyboard) (returnFocus ?? ((el) => el?.focus()))(trigger);
    },
    leave: () => {
      setOpen(false);
      trigger?.focus({ preventScroll: true });
    },
    trigger: (el) => (trigger = el),
  };
}

/** The panel. Its trigger carries `slot-pop__trigger`, `aria-expanded` and `aria-controls={id}`. A menu
 * (`role="menu"`) moves focus to its first item and takes the arrow keys; a dialog takes focus itself. */
export function RowPopoverPanel(props: {
  pop: RowPopover;
  id: string;
  label: string;
  role: 'menu' | 'dialog';
  class: string;
  children: JSX.Element;
}) {
  const items = (el: HTMLElement) => [...el.querySelectorAll<HTMLElement>('[role^="menuitem"]:not(:disabled)')];
  return (
    <Show when={props.pop.open()}>
      <EscapeCloses close={() => props.pop.close(true)} />
      <Portal>
        <div class="slot-pop__backdrop" onClick={() => props.pop.close(false)}>
          <div
            class={`slot-pop ${props.class}`}
            id={props.id}
            role={props.role}
            aria-label={props.label}
            tabindex={-1}
            ref={(el) =>
              queueMicrotask(() => {
                const first = props.role === 'menu' ? items(el)[0] : undefined;
                (first ?? el).focus();
              })
            }
            onClick={(e) => e.stopPropagation()}
            onFocusOut={(e) => {
              // a menu closes when the focus leaves it; a dialog keeps its own say
              const to = e.relatedTarget as Node | null;
              if (props.role === 'menu' && to && !e.currentTarget.contains(to)) props.pop.close(false);
            }}
            onKeyDown={(e) => {
              if (props.role !== 'menu') return;
              if (e.key === 'Tab') {
                // no preventDefault: the browser's Tab moves on from the trigger, Shift+Tab back from it
                props.pop.leave();
                return;
              }
              if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(e.key)) return;
              const list = items(e.currentTarget);
              if (!list.length) return;
              e.preventDefault();
              e.stopPropagation();
              const at = list.indexOf(document.activeElement as HTMLElement);
              const next =
                e.key === 'Home' ? 0 : e.key === 'End' ? list.length - 1 : (at + (e.key === 'ArrowDown' ? 1 : -1) + list.length) % list.length;
              list[next].focus();
            }}
          >
            {props.children}
          </div>
        </div>
      </Portal>
    </Show>
  );
}
