import { For, Show } from 'solid-js';
import { looper } from '../state/audio';
import { engineDubFeedback } from '../state/engine-store';
import { FX_META, FX_PARAM_DEFS, type FxParamDef } from '../state/fx-metadata';
import './fxpanel.css';

/**
 * Per-track FX panel. Renders the five FX (fixed chain order) for one track: each is a bypass
 * toggle plus its params (sliders for continuous params, a select for tempo-synced divisions).
 * All edits go through the looper's imperative setFx* API, which applies them click-free to the
 * live chain and bumps a reactive version so this panel re-reads state. No audio runs here.
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

function ParamControl(props: { index: number; fxIndex: number; def: FxParamDef; value: number }) {
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
            value={props.value}
            onInput={(e) =>
              looper.setFxParam(props.index, props.fxIndex, props.def.key, Number(e.currentTarget.value))
            }
          />
        }
      >
        <select
          class="fxp-param__select"
          value={props.value}
          onChange={(e) =>
            looper.setFxParam(props.index, props.fxIndex, props.def.key, Number(e.currentTarget.value))
          }
        >
          <For each={props.def.choices}>{(c, i) => <option value={i()}>{c}</option>}</For>
        </select>
      </Show>
      <Show when={!props.def.choices}>
        <span class="fxp-param__val">{formatVal(props.value, props.def)}</span>
      </Show>
    </label>
  );
}

/** DUB FEEDBACK: 100 % keeps every old layer (an overdub sums, as ever), 0 % replaces what the dub passes
 * over; between, continuous dubbing fades the old layers pass by pass. */
function DubFeedback(props: { index: number }) {
  const pct = () => Math.round(engineDubFeedback.value(props.index) * 100);
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
          />
          <span class="fxp-param__val">{pct() === 0 ? 'REPLACE' : `${pct()} %`}</span>
        </label>
      </div>
    </div>
  );
}

export function FxPanel(props: { index: number }) {
  const states = () => looper.fxState(props.index);

  return (
    <div class="fxp">
      <For each={FX_META}>
        {(meta, fxI) => {
          const st = () => states()[fxI()];
          const bypassed = () => st()?.bypassed ?? true;
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
                      <ParamControl
                        index={props.index}
                        fxIndex={fxI()}
                        def={def}
                        value={st()?.params[def.key] ?? def.default}
                      />
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
