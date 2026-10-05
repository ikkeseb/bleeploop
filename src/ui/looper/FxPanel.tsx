import { For, Show } from 'solid-js';
import { looper } from '../state/audio';
import { engineDubFeedback } from '../state/engine-store';
import { FX_META, FX_PARAM_DEFS, type FxParamDef } from '../state/fx-metadata';
import { mixDisposal, mixGesture } from './mix-gesture';
import './fxpanel.css';

/**
 * Per-track FX panel. Renders the five FX (fixed chain order) for one track: each is a bypass
 * toggle plus its params (sliders for continuous params, a select for tempo-synced divisions).
 * All edits go through the looper's setFx* API, which sends them to the engine; each control shows its
 * gesture's value until the engine's `Mix` has it (`mix-gesture.ts`), and builds on what it shows. No
 * audio runs here.
 * A last module holds the lane's DUB FEEDBACK: not an effect on what plays, but what
 * an overdub keeps of the layers under it (`lf_engine::looper`), so a heading instead of a bypass key.
 */

function formatVal(v: number, def: FxParamDef): string {
  if (def.choices) return def.choices[Math.round(v)] ?? String(v);
  if (def.unit === 'Hz') return `${Math.round(v)}`;
  if (def.unit === 'st') return `${v > 0 ? '+' : ''}${Math.round(v)}`;
  if (def.step >= 1) return `${Math.round(v)}`;
  return v.toFixed(2);
}

function ParamControl(props: { index: number; fxIndex: number; def: FxParamDef }) {
  const key = `fx${props.fxIndex}.${props.def.key}` as const;
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
          <span class="fxp-param__label">Old</span>
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

export function FxPanel(props: { index: number }) {
  return (
    <div class="fxp">
      <For each={FX_META}>
        {(meta, fxI) => {
          // A bypass press is a whole gesture: it toggles what the key shows.
          const bypassed = () => looper.fxBypassShown(props.index, fxI());
          mixDisposal(props.index, `fx${fxI()}`);
          return (
            <div class="fxp-mod" classList={{ 'fxp-mod--on': !bypassed() }}>
              <button
                class="fxp-mod__toggle"
                classList={{ 'fxp-mod__toggle--on': !bypassed() }}
                onClick={() => looper.setFxBypass(props.index, fxI(), !bypassed())}
                aria-pressed={!bypassed()}
                aria-label={`${meta.label}, track ${props.index + 1}`}
              >
                {meta.label}
              </button>
              <Show when={!bypassed()}>
                <div class="fxp-mod__params">
                  <For each={FX_PARAM_DEFS[meta.kind]}>
                    {(def) => (
                      <ParamControl index={props.index} fxIndex={fxI()} def={def} />
                    )}
                  </For>
                </div>
              </Show>
            </div>
          );
        }}
      </For>
      <DubFeedback index={props.index} />
    </div>
  );
}
