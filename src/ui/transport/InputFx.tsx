import { For, Show } from 'solid-js';
import type { InputSendId } from '../../platform';
import { engineInputSends, type InputSendParamDef } from '../state/engine-store';
import { BarPopoverPanel, createBarPopover } from './BarPopover';
import '../looper/fxpanel.css';
import './input-fx.css';

/**
 * IN FX: the input sends, ECHO, REVERB and RING MOD on the live input (the amp-sim's output, or the dry
 * input of an Off slot). What they add is heard and recorded on the same frame; the dry signal never passes
 * through them (`src-tauri/crates/lf-engine/src/input_fx.rs`). A rig setting, kept in localStorage by the
 * engine store.
 *
 * The command-bar pill opens its popover (`BarPopover.tsx`) with the three sends stacked as rows in FxPanel's
 * faces (the same key-face toggle, sliders and division select; the layout is `input-fx.css`'s); a send that
 * is off shows what it adds instead of its params. The pill reads engaged (warm white) while any send is
 * on. The ring's frequency reads in whole Hz with its unit.
 */

const SENDS: readonly { id: InputSendId; label: string; blurb: string }[] = [
  { id: 'echo', label: 'Echo', blurb: 'Repeats in time with the tempo' },
  { id: 'reverb', label: 'Reverb', blurb: 'A room around the note' },
  { id: 'ring', label: 'Ring mod', blurb: 'Metallic, bell-like overtones' },
];

function formatValue(v: number, def: InputSendParamDef): string {
  if (def.choices) return def.choices[Math.round(v)] ?? String(v);
  if (def.unit === 'Hz') return `${Math.round(v)} Hz`;
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

function InputFxControl(props: { returnFocus?: (el: HTMLElement | undefined) => void }) {
  const sends = engineInputSends;
  const pop = createBarPopover(props.returnFocus);
  return (
    <>
      <button
        ref={pop.trigger}
        class="transport__tgl bar-pop__trigger"
        classList={{ 'is-on': sends.anyOn() }}
        aria-label="Input effects"
        aria-haspopup="dialog"
        aria-expanded={pop.open()}
        aria-controls="lf-infx-popover"
        title="Echo, reverb and ring mod on the live input: heard and recorded, the dry sound untouched"
        onClick={pop.toggle}
      >
        IN FX
      </button>
      <BarPopoverPanel pop={pop} id="lf-infx-popover" label="Input effects" class="infx-popover">
        <div class="fxp infx">
          <For each={SENDS}>
            {(send) => (
              <div class="fxp-mod" classList={{ 'fxp-mod--on': sends.on(send.id) }}>
                <button
                  class="fxp-mod__toggle"
                  classList={{ 'fxp-mod__toggle--on': sends.on(send.id) }}
                  aria-pressed={sends.on(send.id)}
                  aria-label={`Input ${send.label.toLowerCase()}`}
                  onClick={() => sends.toggle(send.id)}
                >
                  {send.label}
                </button>
                <Show when={sends.on(send.id)} fallback={<span class="infx__blurb">{send.blurb}</span>}>
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
      </BarPopoverPanel>
    </>
  );
}

/** The IN FX pill and its popover. `returnFocus` is the transport keys' (app.tsx), for a keyboard
 * close. */
export function InputFx(props: { returnFocus?: (el: HTMLElement | undefined) => void }) {
  return <InputFxControl returnFocus={props.returnFocus} />;
}
