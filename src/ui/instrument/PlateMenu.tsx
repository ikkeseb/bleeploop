import { For, Show } from 'solid-js';
import { Icon } from './icons';
import { RowPopoverPanel, createRowPopover } from './RowPopover';

/** One entry of a plate's menu. `checked` makes it a checkbox item (the editor open, the parameters
 * shown); `controls` names the region it opens. */
export interface PlateMenuItem {
  label: string;
  icon: 'win' | 'params' | 'swap' | 'x';
  onSelect: () => void;
  checked?: boolean;
  disabled?: boolean;
  controls?: string;
}

/**
 * A plate's menu: the small chevron at the plate's very end, the same on every plate, and the list it
 * opens under the plate (what the plate's keys and pickers did before the redesign: the editor, the
 * parameters, a change of sound, its removal). Groups are parted by a rule. A pick closes the menu;
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
  returnFocus?: (el: HTMLElement | undefined) => void;
}) {
  const pop = createRowPopover(props.returnFocus);
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
        disabled={props.disabled}
        onClick={(e) => {
          e.stopPropagation();
          pop.toggle();
        }}
      >
        <Icon name="drop" size={14} />
      </button>
      <RowPopoverPanel pop={pop} id={props.id} label={props.menuLabel} role="menu" class="slot-menu">
        <For each={props.groups.filter((g) => g.length > 0)}>
          {(group, i) => (
            <>
              <Show when={i() > 0}>
                <div class="slot-menu__sep" role="separator" />
              </Show>
              <For each={group}>
                {(item) => (
                  <button
                    type="button"
                    class="slot-menu__item"
                    classList={{ 'is-on': item.checked === true }}
                    role={item.checked === undefined ? 'menuitem' : 'menuitemcheckbox'}
                    aria-checked={item.checked === undefined ? undefined : item.checked}
                    aria-controls={item.controls}
                    disabled={item.disabled}
                    onClick={(e) => {
                      pop.close(e.detail === 0);
                      item.onSelect();
                    }}
                  >
                    <Icon name={item.icon} size={16} />
                    <span>{item.label}</span>
                    <Show when={item.checked}>
                      <i class="k-led k-led--dot is-on" aria-hidden="true" />
                    </Show>
                  </button>
                )}
              </For>
            </>
          )}
        </For>
      </RowPopoverPanel>
    </>
  );
}
