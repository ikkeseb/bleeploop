import { For, Show } from 'solid-js';
import { looper } from '../state/audio';
import { engineDubFeedback } from '../state/engine-store';
import { FX_META, FX_PARAM_DEFS, type FxParamDef } from '../state/fx-metadata';
import { mixDisposal, mixGesture } from './mix-gesture';
import './fxpanel.css';

/**
 * Per-track FX panel, two rows. The keys: the five FX (fixed chain order) as bypass toggles, then DUB
 * FEEDBACK. Under them, while any FX is engaged, ONE row of every engaged FX's params (sliders for
 * continuous params, a select for tempo-synced divisions), each FX's group led by a tag naming it, so
 * engaging all five grows the drawer by one row, not by the tallest FX's stack.
 * All edits go through the looper's setFx* API, which sends them to the engine; each control shows its
 * gesture's value until the engine's `Mix` has it (`mix-gesture.ts`), and builds on what it shows. No
 * audio runs here.
 * A last module holds the lane's DUB FEEDBACK: not an effect on what plays, but what
 * an overdub keeps of the layers under it (`lf_engine::looper`), so a heading instead of a bypass key.
 */

/**
 * A param's read-out, with its unit, formatted from the definition's `unit` and range (`fx-metadata.ts`):
 * Hz, kHz from 1000 (one decimal below 10 kHz); semitones signed (`+3 st`, `−3 st`, as the dB read-outs
 * sign); a unitless 0..1 amount in percent, as DUB FEEDBACK beside it; any other unitless value (the
 * filter's Reso) with two decimals.
 */
function formatVal(v: number, def: FxParamDef): string {
  if (def.choices) return def.choices[Math.round(v)] ?? String(v);
  if (def.unit === 'Hz') {
    if (v < 1000) return `${Math.round(v)} Hz`;
    const k = v / 1000;
    return `${k < 9.95 ? k.toFixed(1) : Math.round(k)} kHz`;
  }
  if (def.unit === 'st') {
    const n = Math.round(v);
    return `${n > 0 ? '+' : n < 0 ? '−' : ''}${Math.abs(n)} st`;
  }
  if (def.unit) return `${def.step >= 1 ? Math.round(v) : v.toFixed(2)} ${def.unit}`;
  if (def.min >= 0 && def.max <= 1) return `${Math.round(v * 100)} %`;
  if (def.step >= 1) return `${Math.round(v)}`;
  return v.toFixed(2);
}

function ParamControl(props: { index: number; fxIndex: number; fx: string; def: FxParamDef }) {
  const key = `fx${props.fxIndex}.${props.def.key}` as const;
  // The control's name: track, effect and parameter ("Track 1 delay mix"), as IN FX names its sends'
  // (`src/ui/transport/InputFx.tsx`). The wrapping label alone would give every effect's Mix the same
  // name, with the numeric readout read into it.
  const name = `Track ${props.index + 1} ${props.fx.toLowerCase()} ${props.def.label.toLowerCase()}`;
  const value = () => looper.mixShown(props.index, key);
  const set = (v: number) => looper.setFxParam(props.index, props.fxIndex, props.def.key, v);
  // A select's change is the whole gesture; a slider's runs from its pointer or key down to its release.
  const gesture = props.def.choices ? null : mixGesture(props.index, key);
  if (!gesture) mixDisposal(props.index, key);
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
            aria-label={name}
            aria-valuetext={formatVal(value(), props.def)}
            onInput={(e) => set(Number(e.currentTarget.value))}
            onPointerDown={gesture?.onPointerDown}
            onLostPointerCapture={gesture?.onLostPointerCapture}
            onKeyDown={gesture?.onKeyDown}
            onKeyUp={gesture?.onKeyUp}
            onBlur={gesture?.onBlur}
          />
        }
      >
        {/* Controlled: the native select changes its own value before the engine has it, so it is set
            back to what the control shows (its overlay now; the engine's value once a failed send drops
            the overlay, through `value`). */}
        <select
          class="fxp-param__select"
          value={value()}
          aria-label={name}
          onChange={(e) => {
            set(Number(e.currentTarget.value));
            e.currentTarget.value = String(value());
          }}
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

/** DUB FEEDBACK: 100 % keeps every old layer (an overdub sums, as ever), 0 % replaces what the dub passes
 * over; between, continuous dubbing fades the old layers pass by pass. */
function DubFeedback(props: { index: number }) {
  const pct = () => Math.round(engineDubFeedback.value(props.index) * 100);
  const gesture = mixGesture(props.index, 'dubFeedback');
  return (
    <div class="fxp-mod fxp-mod--on fxp-dub">
      <span class="fxp-mod__title" title="What an overdub keeps of the layers under it, pass by pass. 0 %: the dub replaces them">
        Dub feedback
      </span>
      <div class="fxp-mod__params">
        <label class="fxp-param">
          <span class="fxp-param__label">Keep</span>
          <input
            type="range"
            class="lf-range fxp-param__range"
            min={0}
            max={100}
            step={1}
            value={pct()}
            aria-label={`Track ${props.index + 1} dub feedback`}
            aria-valuetext={pct() === 0 ? '0 percent: the dub replaces the old layers' : `${pct()} percent of the old layers kept`}
            onInput={(e) => engineDubFeedback.set(props.index, Number(e.currentTarget.value) / 100)}
            onPointerDown={gesture.onPointerDown}
            onLostPointerCapture={gesture.onLostPointerCapture}
            onKeyDown={gesture.onKeyDown}
            onKeyUp={gesture.onKeyUp}
            onBlur={gesture.onBlur}
          />
          <span class="fxp-param__val">{pct() === 0 ? 'REPLACE' : `${pct()} %`}</span>
        </label>
      </div>
    </div>
  );
}

// The params row's columns, one per param in chain order: each FX's key heads its own params' columns
// (`--col`, `--span`), so a group sits under its key whichever FX are engaged (fxpanel.css).
const FX_SPANS = FX_META.map((meta) => FX_PARAM_DEFS[meta.kind].length);
const FX_COLS = FX_SPANS.reduce((sum, n) => sum + n, 0);
const FX_STARTS = FX_SPANS.map((_, i) => 1 + FX_SPANS.slice(0, i).reduce((sum, n) => sum + n, 0));

export function FxPanel(props: { index: number }) {
  // A bypass press is a whole gesture: it toggles what the key shows.
  const engaged = (fxI: number) => !looper.fxBypassShown(props.index, fxI);
  return (
    <div class="fxp" style={{ '--cols': FX_COLS }}>
      <div class="fxp-keys">
        <For each={FX_META}>
          {(meta, fxI) => {
            mixDisposal(props.index, `fx${fxI()}`);
            return (
              <button
                class="fxp-mod__toggle"
                classList={{ 'fxp-mod__toggle--on': engaged(fxI()) }}
                style={{ '--col': FX_STARTS[fxI()], '--span': FX_SPANS[fxI()] }}
                onClick={() => looper.setFxBypass(props.index, fxI(), engaged(fxI()))}
                aria-pressed={engaged(fxI())}
                aria-label={`${meta.label}, track ${props.index + 1}`}
              >
                {meta.label}
              </button>
            );
          }}
        </For>
        <DubFeedback index={props.index} />
      </div>
      <Show when={FX_META.some((_, fxI) => engaged(fxI))}>
        <div class="fxp-params">
          <For each={FX_META}>
            {(meta, fxI) => (
              <Show when={engaged(fxI())}>
                <div class="fxp-group" style={{ '--col': FX_STARTS[fxI()], '--params': FX_SPANS[fxI()] }}>
                  <span class="fxp-group__tag">{meta.label}</span>
                  <For each={FX_PARAM_DEFS[meta.kind]}>
                    {(def) => <ParamControl index={props.index} fxIndex={fxI()} fx={meta.label} def={def} />}
                  </For>
                </div>
              </Show>
            )}
          </For>
        </div>
      </Show>
    </div>
  );
}
