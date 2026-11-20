import { For, Show } from 'solid-js';
import type { InputSendId } from '../../platform';
import { formatVal } from '../looper/FxPanel';
import { engineInputSends, type InputSendParamDef } from '../state/engine-store';
import { RowPopoverPanel, createRowPopover } from '../instrument/RowPopover';
import { Icon } from '../instrument/icons';
import '../looper/fxpanel.css';
import './input-fx.css';

/**
 * IN FX: what the live input gets on its way to the monitor and the looper (the slot's output: the
 * amp-sim's, a synth's, or the dry input of an Off slot). ECHO, REVERB and RING MOD are sends: what they
 * add is heard and recorded on the same frame, and the dry signal never passes through them
 * (`src-tauri/crates/lf-engine/src/input_fx.rs`). The OTT is inline: a three-band upward/downward
 * compressor on each slot's output and the built-in synths' (`dsp/ott.rs`), so what is heard and what is
 * recorded is the same compressed sound. All four ride the input-send wire and are a rig setting, kept in
 * localStorage by the engine store; each power lamp sends the engine's toggle and shows its Toggled
 * answer, so a pedal and a click never undo each other.
 *
 * Drawn in slot A's signal line (`InstrumentSlot.tsx`) as four unit plates after its source: each its
 * power lamp (the send's toggle), its name and its menu chevron, which opens its settings under the row
 * (`RowPopover.tsx`). They act on both slots' sound today, and each plate's title says so. Where the row
 * is narrow (`row-fit.ts`) the four fold into one IN FX plate whose popover holds the four rows. A param
 * reads and speaks its value as the lane drawer's do (FxPanel.tsx `formatVal`).
 */
const SENDS: readonly { id: InputSendId; label: string; name: string; blurb: string }[] = [
  { id: 'echo', label: 'Echo', name: 'Input echo', blurb: 'Repeats in time with the tempo' },
  { id: 'reverb', label: 'Reverb', name: 'Input reverb', blurb: 'A room around the note' },
  { id: 'ring', label: 'Ring mod', name: 'Input ring mod', blurb: 'Metallic, bell-like overtones' },
  { id: 'ott', label: 'OTT', name: 'Input OTT', blurb: 'Evens out loud and quiet, in three bands' },
];

const BOTH = "acts on both slots' sound, heard and recorded";

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

/** One send's params, as its popover and the folded popover's rows list them. */
function SendParams(props: { send: (typeof SENDS)[number] }) {
  return (
    <div class="fxp-mod__params">
      <For each={engineInputSends.params.filter((d) => d.send === props.send.id)}>
        {(def) => <SendParam def={def} send={props.send.label} />}
      </For>
    </div>
  );
}

/** A send's power lamp: the engine's toggle, lit while the feed says it is on. */
function PowerLamp(props: { send: (typeof SENDS)[number] }) {
  const sends = engineInputSends;
  return (
    <button
      type="button"
      class="plate__lamp"
      aria-pressed={sends.on(props.send.id)}
      aria-label={props.send.name}
      title={`${props.send.label} on or off: ${BOTH}`}
      onClick={(e) => {
        e.stopPropagation();
        sends.toggle(props.send.id);
      }}
    />
  );
}

/** One input effect as a unit plate in slot A's line. */
function SendPlate(props: { send: (typeof SENDS)[number]; returnFocus?: (el: HTMLElement | undefined) => void }) {
  const sends = engineInputSends;
  const on = () => sends.on(props.send.id);
  const pop = createRowPopover(props.returnFocus);
  const id = `lf-infx-${props.send.id}`;
  return (
    <>
      <i class="slot__sig" classList={{ 'is-dim': !on() }} aria-hidden="true" />
      <div class="plate plate--fx" classList={{ 'plate--byp': !on() }} title={`${props.send.label}: ${BOTH}`}>
        <PowerLamp send={props.send} />
        <span class="plate__name plate__name--fx">{props.send.label}</span>
        <button
          type="button"
          ref={pop.trigger}
          class="plate__menu slot-pop__trigger"
          aria-label={`Settings for input ${props.send.label}`}
          aria-haspopup="dialog"
          aria-expanded={pop.open()}
          aria-controls={id}
          onClick={(e) => {
            e.stopPropagation();
            pop.toggle();
          }}
        >
          <Icon name="drop" size={14} />
        </button>
        <RowPopoverPanel pop={pop} id={id} label={`Input ${props.send.label.toLowerCase()} settings`} role="dialog" class="infx-pop">
          <div class="fxp infx infx--one">
            <p class="infx__head">
              <span class="infx__name">{props.send.label}</span>
              <span class="infx__blurb">
                {props.send.blurb}; {BOTH}
              </span>
            </p>
            <SendParams send={props.send} />
          </div>
        </RowPopoverPanel>
      </div>
    </>
  );
}

/** The four folded into one plate: its lamp lit while any is on, its popover the four rows. */
function FoldedPlate(props: { returnFocus?: (el: HTMLElement | undefined) => void }) {
  const sends = engineInputSends;
  const pop = createRowPopover(props.returnFocus);
  return (
    <>
      <i class="slot__sig" classList={{ 'is-dim': !sends.anyOn() }} aria-hidden="true" />
      <div class="plate plate--fx" classList={{ 'plate--byp': !sends.anyOn() }} title={`Echo, reverb, ring mod and OTT on the live input: ${BOTH}`}>
        <span class="plate__lamp" classList={{ 'is-on': sends.anyOn() }} aria-hidden="true" />
        <span class="plate__name plate__name--fx">IN FX</span>
        <button
          type="button"
          ref={pop.trigger}
          class="plate__menu slot-pop__trigger"
          classList={{ 'is-on': sends.anyOn() }}
          aria-label="Input effects"
          aria-haspopup="dialog"
          aria-expanded={pop.open()}
          aria-controls="lf-infx-popover"
          onClick={(e) => {
            e.stopPropagation();
            pop.toggle();
          }}
        >
          <Icon name="drop" size={14} />
        </button>
        <RowPopoverPanel pop={pop} id="lf-infx-popover" label="Input effects" role="dialog" class="infx-pop">
          <div class="fxp infx">
            <For each={SENDS}>
              {(send) => (
                <div class="fxp-mod infx__row" classList={{ 'fxp-mod--on': sends.on(send.id) }}>
                  <PowerLamp send={send} />
                  <span class="infx__name">{send.label}</span>
                  <Show when={sends.on(send.id)} fallback={<span class="infx__blurb">{send.blurb}</span>}>
                    <SendParams send={send} />
                  </Show>
                </div>
              )}
            </For>
          </div>
        </RowPopoverPanel>
      </div>
    </>
  );
}

/** The input effects in slot A's line: four plates, or one where the row is narrow (`folded`).
 * `returnFocus` is the transport keys' (app.tsx), for a keyboard close. */
export function InputFx(props: { folded: boolean; returnFocus?: (el: HTMLElement | undefined) => void }) {
  return (
    <Show when={!props.folded} fallback={<FoldedPlate returnFocus={props.returnFocus} />}>
      <For each={SENDS}>{(send) => <SendPlate send={send} returnFocus={props.returnFocus} />}</For>
    </Show>
  );
}
