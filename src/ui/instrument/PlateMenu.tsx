import { For, Show, createMemo } from 'solid-js';
import { Icon, type IconName } from './icons';
import { RowPopoverPanel, createRowPopover } from './RowPopover';

/** One entry of a plate's menu. `checked` makes it a checkbox item (the editor open, the parameters
 * shown); `controls` names the region it opens. */
export interface PlateMenuItem {
  /** What the entry does, unique in its menu (the label when absent): entries are kept by it, so a report
   * that rebuilds the groups never replaces the focused entry. */
  key?: string;
  label: string;
  icon: IconName;
  onSelect: () => void;
  checked?: boolean;
  disabled?: boolean;
  controls?: string;
}

/**
 * A plate's menu: the small chevron at the plate's very end, the same on every plate, and the list it
 * opens under the plate (what the plate's keys and pickers did before the redesign: the editor, the
 * parameters, a change of sound, its removal; on a chain's unit, its moves). Groups are parted by a rule. A pick closes the menu;
 * a keyboard pick hands focus back to the chevron.
 */
export function PlateMenu(props: {
  id: string;
  /** The chevron's accessible name, e.g. `Menu for slot A`. */
  label: string;
  /** The open menu's name. */
  menuLabel: string;
  groups: PlateMenuItem[][];
  disabled?: boolean;
  /** A line read before the entries (a failed unit's whole reason), and the chevron's description. */
  note?: string | null;
  returnFocus?: (el: HTMLElement | undefined) => void;
}) {
  const pop = createRowPopover(props.returnFocus);
  // Entries keyed by action: the groups may be new objects on every report; the buttons stay.
  const flat = createMemo(() => {
    const out = new Map<string, { item: PlateMenuItem; sep: boolean }>();
    props.groups
      .filter((g) => g.length > 0)
      .forEach((g, gi) => g.forEach((item, k) => out.set(item.key ?? item.label, { item, sep: gi > 0 && k === 0 })));
    return out;
  });
  const noteId = `${props.id}-note`;
  return (
    <>
      <button
        type="button"
        ref={pop.trigger}
        class="plate__menu slot-pop__trigger"
        aria-label={props.label}
        aria-haspopup="menu"
        aria-expanded={pop.open()}
        aria-controls={props.id}
        aria-describedby={props.note ? noteId : undefined}
        disabled={props.disabled}
        onClick={(e) => {
          e.stopPropagation();
          pop.toggle();
        }}
      >
        <Icon name="drop" size={14} />
      </button>
      <Show when={props.note}>
        <span id={noteId} class="lf-visually-hidden">
          {props.note}
        </span>
      </Show>
      <RowPopoverPanel pop={pop} id={props.id} label={props.menuLabel} role="menu" class="slot-menu">
        <Show when={props.note}>
          <p class="slot-menu__note">{props.note}</p>
        </Show>
        <For each={[...flat().keys()]}>
          {(key) => {
            const entry = () => flat().get(key);
            const item = (): PlateMenuItem | undefined => entry()?.item;
            return (
              <Show when={item()}>
                <Show when={entry()?.sep}>
                  <div class="slot-menu__sep" role="separator" />
                </Show>
                <button
                  type="button"
                  class="slot-menu__item"
                  classList={{ 'is-on': item()?.checked === true }}
                  role={item()?.checked === undefined ? 'menuitem' : 'menuitemcheckbox'}
                  aria-checked={item()?.checked === undefined ? undefined : item()?.checked}
                  aria-controls={item()?.controls}
                  data-key={key}
                  disabled={item()?.disabled}
                  onClick={(e) => {
                    // the entry as it is now, never as the menu first drew it
                    const now = flat().get(key)?.item;
                    if (!now || now.disabled) return;
                    pop.close(e.detail === 0);
                    now.onSelect();
                  }}
                >
                  <Icon name={item()!.icon} size={16} />
                  <span>{item()!.label}</span>
                  <Show when={item()?.checked}>
                    <i class="k-led k-led--dot is-on" aria-hidden="true" />
                  </Show>
                </button>
              </Show>
            );
          }}
        </For>
      </RowPopoverPanel>
    </>
  );
}
