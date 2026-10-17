import { For, Match, Show, Switch, createEffect, createSignal } from 'solid-js';
import { Portal } from 'solid-js/web';
import { looper } from '../state/audio';
import { stageOpen } from '../stage/stage-store';
import { copyGate, editGate, trimGate, why, type Gate } from './gates';
import { EscapeCloses } from './shared';
import { TrimDialog } from './Trim';
import './edit.css';

/**
 * The lane's EDIT pill, the last of its fixed columns (FX · MUTE · REV · UNDO · EDIT), and its popover:
 * ⧉ COPY, ↔ WIDE and ✂ TRIM, the lane edits a jam reaches for less often than the four beside it. Each
 * stays its own named action (`src/app/actions.ts`: `copy`, `copyWide`, `halveTrack`), so a pedal or a
 * learned MIDI note reaches it without the popover.
 *
 * The pill opens over a committed loop (`editGate`); the empty lane's is disabled with the rest of the
 * cluster (D4). The popover hangs under the pill, IN FX's pattern (`src/ui/transport/InputFx.tsx`), and
 * lists the three edits, each disabled with the engine's reason where it would be refused. COPY and WIDE
 * send and close; TRIM turns the popover into its bar stepper (`Trim.tsx`). Escape, an outside click or
 * the stage view closes it; a keyboard close hands focus back to the pill on the app's behalf
 * (`returnFocus`), so Space after it still arms REC.
 */

type Item = {
  glyph: string;
  word: string;
  hint: string;
  label: (track: number) => string;
  title: string;
  gate: (i: number) => Gate;
  /** Sends the edit; none for TRIM, whose bar stepper comes first. */
  run?: (i: number) => void;
};

const ITEMS: readonly Item[] = [
  {
    glyph: '⧉',
    word: 'COPY',
    hint: 'to the first empty track',
    label: (t) => `Copy track ${t} to the first empty lane`,
    title: 'Copy this lane to the first empty lane',
    gate: copyGate,
    run: (i) => looper.copy(i),
  },
  {
    glyph: '↔',
    word: 'WIDE',
    hint: 'a copy 20 ms behind, the other side',
    label: (t) => `Copy track ${t} wide to the first empty lane`,
    title: 'Copy wide: the copy plays 20 ms behind, panned to the other side',
    gate: copyGate,
    run: (i) => looper.copyWide(i),
  },
  {
    glyph: '✂',
    word: 'TRIM',
    hint: 'keep the first bars, repeat them',
    label: (t) => `Trim track ${t}`,
    title: 'Keep the first bars and repeat them across the loop (undo gives the whole loop back)',
    gate: trimGate,
  },
];

export function Edit(props: { index: number; returnFocus?: (el: HTMLElement | undefined) => void }) {
  const [view, setView] = createSignal<'menu' | 'trim' | null>(null);
  const track = props.index + 1;
  const id = `lf-edit-popover-${props.index}`;
  const gate = () => editGate(props.index);
  let trigger: HTMLButtonElement | undefined;
  const close = (keyboard: boolean) => {
    setView(null);
    if (keyboard) props.returnFocus?.(trigger);
  };
  // The stage view hides the lanes, and a lane cleared or re-recorded under the popover leaves nothing to
  // edit: the popover goes with them.
  createEffect(() => {
    if (view() !== null && (stageOpen() || !gate().ok)) setView(null);
  });
  // An edit whose gate closes under the open menu while it has the focus (a pedal starts an overdub, a
  // copy fills the last empty lane) disables its row, and a disabled button drops the focus to the page,
  // out of the lane. The menu closes as a keyboard close does: the focus goes back to the lane's EDIT, on
  // the app's behalf, so Space there still arms REC.
  let menu: HTMLDivElement | undefined;
  createEffect(() => {
    for (const item of ITEMS) item.gate(props.index);
    const el = document.activeElement;
    if (menu && el instanceof HTMLButtonElement && el.disabled && menu.contains(el)) close(true);
  });

  const press = (item: Item, e: MouseEvent) => {
    if (!item.gate(props.index).ok) return;
    if (!item.run) return setView('trim');
    item.run(props.index);
    close(e.detail === 0);
  };

  return (
    <>
      <button
        ref={trigger}
        class="lp-pb lp-pb--edit"
        classList={{ 'is-open': view() !== null }}
        disabled={!gate().ok}
        aria-label={`Edit track ${track}`}
        aria-haspopup="dialog"
        aria-expanded={view() !== null}
        aria-controls={id}
        title={why(gate()) ?? 'Copy, copy wide or trim this lane'}
        onClick={() => setView((v) => (v === null ? 'menu' : null))}
      >
        EDIT
      </button>
      <Show when={view()}>
        <EscapeCloses close={() => close(true)} />
        <Portal>
          <div class="settings-popover__backdrop" onClick={() => close(false)}>
            <Switch>
              <Match when={view() === 'menu'}>
                <div
                  class="edit-popover"
                  id={id}
                  role="dialog"
                  aria-label={`Edit track ${track}`}
                  tabindex={-1}
                  ref={(el) => {
                    menu = el;
                    queueMicrotask(() => el.focus());
                  }}
                  onClick={(e) => e.stopPropagation()}
                >
                  <div class="edit">
                    <For each={ITEMS}>
                      {(item) => {
                        const g = () => item.gate(props.index);
                        // The hint reads on from the word ("COPY to the first empty track"), a refusal too.
                        const hint = () => {
                          const r = g();
                          return r.ok ? item.hint : r.reason;
                        };
                        return (
                          <button
                            class="edit__item"
                            disabled={!g().ok}
                            aria-label={item.label(track)}
                            title={g().ok ? item.title : undefined}
                            onClick={(e) => press(item, e)}
                          >
                            <span class="edit__word">
                              <span class="edit__glyph" aria-hidden="true">
                                {item.glyph}
                              </span>
                              {item.word}
                            </span>
                            <span class="edit__hint">{hint()}</span>
                          </button>
                        );
                      }}
                    </For>
                  </div>
                </div>
              </Match>
              <Match when={view() === 'trim'}>
                <TrimDialog index={props.index} id={id} done={close} />
              </Match>
            </Switch>
          </div>
        </Portal>
      </Show>
    </>
  );
}
