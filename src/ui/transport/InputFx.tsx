import { For, Show, createSignal, onCleanup, onMount } from 'solid-js';
import { Portal } from 'solid-js/web';
import { engineMode, type InputSendId } from '../../platform';
import { engineInputSends, type InputSendParamDef } from '../state/engine-store';
import '../looper/fxpanel.css';
import './input-fx.css';

/**
 * IN FX: the input sends, ECHO and REVERB on the live input (the amp-sim's output, or the dry input on
 * MIC). What they add is heard and recorded on the same frame; the dry signal never passes through them
 * (`src-tauri/crates/lf-engine/src/input_fx.rs`). A rig setting, kept in localStorage by the engine
 * store. Engine mode only: the web path has no input sends, so nothing renders there.
 *
 * The command-bar pill opens a small popover under itself with the two sends as FxPanel's modules (the
 * same key-face toggle, sliders and division select); the pill reads engaged (warm white) while any
 * send is on.
 */

const SENDS: readonly { id: InputSendId; label: string }[] = [
  { id: 'echo', label: 'Echo' },
  { id: 'reverb', label: 'Reverb' },
];

function formatValue(v: number, def: InputSendParamDef): string {
  if (def.choices) return def.choices[Math.round(v)] ?? String(v);
  return v.toFixed(2);
}

function SendParam(props: { def: InputSendParamDef; send: string }) {
  const sends = engineInputSends;
  const value = () => sends.value(props.def.key);
  const name = () => `${props.send} ${props.def.label.toLowerCase()}`;
  return (
    <label class="fxp-param">
      <span class="fxp-param__label">{props.def.label}</span>
      <Show
        when={props.def.choices}
        fallback={
          <input
            type="range"
            class="lf-range fxp-param__range"
            min={props.def.min}
            max={props.def.max}
            step={props.def.step}
            value={value()}
            aria-label={name()}
            onInput={(e) => sends.setValue(props.def.key, Number(e.currentTarget.value))}
          />
        }
      >
        <select
          class="fxp-param__select"
          value={value()}
          aria-label={name()}
          onChange={(e) => sends.setValue(props.def.key, Number(e.currentTarget.value))}
        >
          <For each={props.def.choices}>{(c, i) => <option value={i()}>{c}</option>}</For>
        </select>
      </Show>
      <Show when={!props.def.choices}>
        <span class="fxp-param__val">{formatValue(value(), props.def)}</span>
      </Show>
    </label>
  );
}

/** Close on Escape wherever focus is (a pointer press blurs the control it activated: transport-keys). */
function EscapeCloses(props: { close: () => void }) {
  const onKey = (e: KeyboardEvent) => {
    if (e.key === 'Escape') props.close();
  };
  onMount(() => window.addEventListener('keydown', onKey));
  onCleanup(() => window.removeEventListener('keydown', onKey));
  return null;
}

function InputFxControl(props: { returnFocus?: (el: HTMLElement | undefined) => void }) {
  const sends = engineInputSends;
  const [open, setOpen] = createSignal(false);
  let trigger: HTMLButtonElement | undefined;
  // Non-modal, like the command bar's other popovers (app.tsx): focus moves into the panel on open, and
  // a keyboard close hands it back to the pill on the app's behalf (`returnFocus`, so Space after
  // Escape still arms REC); the play path stays live behind it.
  const close = (keyboard: boolean) => {
    setOpen(false);
    if (keyboard) props.returnFocus?.(trigger);
  };
  return (
    <>
      <button
        ref={trigger}
        class="transport__tgl infx__pill"
        classList={{ 'is-on': sends.anyOn() }}
        aria-label="Input effects"
        aria-haspopup="dialog"
        aria-expanded={open()}
        aria-controls="lf-infx-popover"
        title="Echo and reverb on the live input: heard and recorded, the dry sound untouched"
        onClick={() => setOpen((v) => !v)}
      >
        IN FX
      </button>
      <Show when={open()}>
        <EscapeCloses close={() => close(true)} />
        <Portal>
          <div class="settings-popover__backdrop" onClick={() => close(false)}>
            <div
              class="infx-popover"
              id="lf-infx-popover"
              role="dialog"
              aria-label="Input effects"
              tabindex={-1}
              ref={(el) => queueMicrotask(() => el.focus())}
              onClick={(e) => e.stopPropagation()}
            >
              <div class="fxp infx">
                <For each={SENDS}>
                  {(send) => (
                    <div class="fxp-mod" classList={{ 'fxp-mod--on': sends.on(send.id) }}>
                      <button
                        class="fxp-mod__toggle"
                        classList={{ 'fxp-mod__toggle--on': sends.on(send.id) }}
                        aria-pressed={sends.on(send.id)}
                        aria-label={`Input ${send.label.toLowerCase()}`}
                        onClick={() => sends.setOn(send.id, !sends.on(send.id))}
                      >
                        {send.label}
                      </button>
                      <Show when={sends.on(send.id)}>
                        <div class="fxp-mod__params">
                          <For each={sends.params.filter((d) => d.send === send.id)}>
                            {(def) => <SendParam def={def} send={send.label} />}
                          </For>
                        </div>
                      </Show>
                    </div>
                  )}
                </For>
              </div>
            </div>
          </div>
        </Portal>
      </Show>
    </>
  );
}

/** The IN FX pill and its popover, in engine mode; nothing in web mode. `returnFocus` is the transport
 * keys' (app.tsx), for a keyboard close. */
export function InputFx(props: { returnFocus?: (el: HTMLElement | undefined) => void }) {
  return (
    <Show when={engineMode()}>
      <InputFxControl returnFocus={props.returnFocus} />
    </Show>
  );
}
