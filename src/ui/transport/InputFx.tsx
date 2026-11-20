import { For, Show } from 'solid-js';
import type { InputSendId } from '../../platform';
import { formatVal } from '../looper/FxPanel';
import { engineInputSends, type InputSendParamDef } from '../state/engine-store';
import { BarPopoverPanel, createBarPopover } from './BarPopover';
import '../looper/fxpanel.css';
import './input-fx.css';

/**
 * IN FX: what the live input gets on its way to the monitor and the looper (the slot's output: the
 * amp-sim's, a synth's, or the dry input of an Off slot). Rows one to three are the sends, ECHO, REVERB
 * and RING MOD: what they add is heard and recorded on the same frame, and the dry signal never passes
 * through them (`src-tauri/crates/lf-engine/src/input_fx.rs`). Row four is the OTT, inline: a three-band
 * upward/downward compressor on each slot's output and the built-in synths' (`dsp/ott.rs`), so what is
 * heard and what is recorded is the same compressed sound. All four ride the input-send wire and are a
 * rig setting, kept in localStorage by the engine store; each key sends the engine's toggle and shows its
 * Toggled answer, so a pedal and a click never undo each other.
 *
 * The command-bar pill opens its popover (`BarPopover.tsx`) with the rows in FxPanel's faces (the same
 * key-face toggle, sliders and division select; the layout is `input-fx.css`'s); a row that is off shows
 * what it does instead of its params. The pill reads engaged (warm white) while any row is on. A param
 * reads and speaks its value as the drawer's do (FxPanel.tsx `formatVal`): a level in percent, the
 * ring's frequency in Hz or kHz; the OTT's DEPTH, TIME, UP and DOWN in percent.
 */
const SENDS: readonly { id: InputSendId; label: string; name: string; blurb: string }[] = [
  { id: 'echo', label: 'Echo', name: 'Input echo', blurb: 'Repeats in time with the tempo' },
  { id: 'reverb', label: 'Reverb', name: 'Input reverb', blurb: 'A room around the note' },
  { id: 'ring', label: 'Ring mod', name: 'Input ring mod', blurb: 'Metallic, bell-like overtones' },
  { id: 'ott', label: 'OTT', name: 'Input OTT', blurb: 'Evens out loud and quiet, in three bands' },
];

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
            aria-valuetext={formatVal(value(), props.def)}
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
        <span class="fxp-param__val">{formatVal(value(), props.def)}</span>
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
        title="Echo, reverb, ring mod and OTT on the live input: heard and recorded"
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
                  aria-label={send.name}
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
