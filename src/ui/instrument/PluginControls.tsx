import { For, Show, createMemo, createSignal, onCleanup, onMount } from 'solid-js';
import { createStore } from 'solid-js/store';
import { platform, type PluginDescriptor, type PluginParamDesc } from '../../platform';
import { editorAffinityBlocker, noteEditorOpened, slotPendingCounts } from '../state/instrument';
import { notifyError } from '../../notify';
import { liveShown, toggleLive } from './live';
import './plugin-controls.css';

/**
 * Source-row plugin controls, split across the slot header and its params drawer:
 *
 *  - `PluginBar`    — the header's plugin pair: the plugin EDITOR toggle (pop-out window) and the PARAMS
 *                     drawer disclosure. The slot's GO LIVE and its volume (the plugin's output) sit
 *                     beside them (`SlotControls.tsx`).
 *  - `PluginParams` — the accordion drawer body: a preview grid of the plugin's first params, as slim
 *                     groove sliders. Mounts when a plugin is loaded (so `onParamChanged` tracks live
 *                     positions even while collapsed); the drawer's open state is CSS-toggled by the
 *                     owning slot, so it never remounts on open/close.
 *
 * Both mount under the slot's `<Show keyed>` on the loaded descriptor, so a plugin swap remounts them
 * (re-fetching params / resetting the editor).
 */
const PARAM_PREVIEW_COUNT = 12;

/**
 * The header's plugin pair (editor + params disclosure). The flat-pill styling comes from the shared
 * `.tgl` rules in app.css, so this only owns the wiring.
 */
export function PluginBar(props: {
  slot: 0 | 1;
  descriptor: PluginDescriptor;
  paramsOpen: boolean;
  onToggleParams: () => void;
  drawerId: string;
  /** True when the user just picked this plugin in the slot dropdown (not a reload resync): an effect
   * goes live and the editor opens on its own. `onAutoStartDone` clears the owner's one-shot flag. */
  autoStart: boolean;
  onAutoStartDone: () => void;
}) {
  const [editorOpen, setEditorOpen] = createSignal(false);
  const [editorBusy, setEditorBusy] = createSignal(false);
  const [editorError, setEditorError] = createSignal<string | null>(null);
  const sourcePending = () => slotPendingCounts()[props.slot] > 0;

  const offClosed = platform.pluginHost.onEditorClosed((s) => {
    if (s === props.slot) setEditorOpen(false);
  });
  onCleanup(offClosed);

  // `quiet` = the auto-open after a dropdown pick: a refusal or a plugin with no editor is not an error
  // the user caused, so it skips the toasts (the console.error stays — it feeds the release log).
  async function toggleEditor(quiet = false) {
    if (editorBusy()) return; // single-flight: ignore re-clicks while an open/close is pending
    const wantOpen = !editorOpen();
    // Same plugin FILE in both slots shares one GUI runtime whose message thread binds to the FIRST
    // slot that opens an editor — a second open from the other slot (concurrent OR after close) wedges
    // inside the plugin, and killing the frozen window kills the app. Refuse it up front. (Mixed
    // CLAP+VST3 of the same plugin = two modules = fine.)
    if (wantOpen) {
      const owner = editorAffinityBlocker(props.slot, props.descriptor.path);
      if (owner) {
        if (quiet) return;
        setEditorError(`editor owned by ${owner}`);
        notifyError(
          'Same plugin file in both slots',
          `Its GUI can only serve one slot's editor per load. ${owner} opened it first. ` +
            'Load the plugin’s other format (CLAP vs VST3) in this slot for a second editor.',
        );
        return;
      }
    }
    setEditorBusy(true);
    setEditorError(null);
    try {
      if (wantOpen) {
        await platform.pluginHost.openEditor(props.slot, 'embedded');
        setEditorOpen(true);
        noteEditorOpened(props.slot, props.descriptor.path);
      } else {
        await platform.pluginHost.closeEditor(props.slot);
        setEditorOpen(false);
      }
    } catch (e) {
      // Branch by intent: an OPEN failure means the plugin exposes no editor → reflect closed. A CLOSE
      // failure means the window is likely still open → keep editorOpen(true) so the toggle doesn't claim
      // it closed (the next click then retries the close, not a second open).
      if (wantOpen) {
        setEditorError('no editor');
        setEditorOpen(false);
      } else {
        setEditorError('close failed');
      }
      console.error('[PluginControls] editor toggle failed', e);
      if (!quiet) notifyError(wantOpen ? 'Plugin editor failed to open' : 'Plugin editor failed to close', e);
    } finally {
      setEditorBusy(false);
    }
  }

  // A fresh dropdown pick starts itself: hearing yourself through the effect is the point of loading
  // one, so a scanned EFFECT goes live first (a synth or an unclassified plugin is left alone — going
  // live on a plugin with no input bus only yields an error), then the editor opens.
  onMount(() => {
    if (!props.autoStart) return;
    props.onAutoStartDone();
    let gone = false; // a swap while going live remounts this bar — the new one runs its own start
    onCleanup(() => (gone = true));
    void (async () => {
      if (props.descriptor.isEffect === true && liveShown(props.slot)) await toggleLive(props.slot, true);
      if (!gone) await toggleEditor(true);
    })();
  });

  return (
    <>
      {/* editor toggle (open = the lit warm-white `.on` legend; one accessible name, the state in aria-pressed) */}
      <button
        type="button"
        class="tgl"
        classList={{ on: editorOpen() }}
        aria-pressed={editorOpen()}
        aria-label={`Editor for slot ${props.slot + 1}`}
        disabled={sourcePending() || editorBusy()}
        onClick={() => void toggleEditor()}
      >
        EDITOR
      </button>
      {/* PARAMS — the params-drawer disclosure, named for what it opens. */}
      <button
        type="button"
        class="tgl slot__params"
        classList={{ on: props.paramsOpen }}
        aria-expanded={props.paramsOpen}
        aria-controls={props.drawerId}
        aria-label={`Plugin parameters for slot ${props.slot + 1}`}
        disabled={sourcePending()}
        onClick={props.onToggleParams}
      >
        PARAMS <span class="slot__caret" aria-hidden="true">▾</span>
      </button>
      <Show when={editorError()}>
        <span class="pc__err">{editorError()}</span>
      </Show>
    </>
  );
}

/**
 * The accordion drawer body: a preview grid of the plugin's first params (its output level is the slot
 * header's volume). The `onParamChanged` subscription proves "editor → web UI updates" — a knob
 * dragged in the plugin's own GUI repositions the matching slider and shows a last-edited readout. A
 * plugin like Surge exposes thousands of params, so we render only the first PARAM_PREVIEW_COUNT — the
 * editor is the full UI.
 */
export function PluginParams(props: { slot: 0 | 1 }) {
  const [params, setParams] = createSignal<PluginParamDesc[]>([]);
  const [totalParams, setTotalParams] = createSignal(0);
  const [values, setValues] = createStore<Record<number, number>>({});
  const [lastEdit, setLastEdit] = createSignal<{ name: string; value: number } | null>(null);
  const sourcePending = () => slotPendingCounts()[props.slot] > 0;
  const nameById = new Map<number, string>();
  const renderedIds = new Set<number>();
  // Which write last touched each slider: a token per slider drag or editor-originated change, cleared
  // by `refresh`. A refused `setParameter` snaps its slider back only while its own token is still the
  // newest, so a refusal never overrides newer state. `alive` covers an unmount (a plugin swap remounts).
  const touched = new Map<number, number>();
  let touchTick = 0;
  let alive = true;
  onCleanup(() => (alive = false));

  // (Re)list the plugin's params and seed every rendered slider from its LIVE value — on mount, and
  // again whenever the plugin reports a wholesale change (preset loaded in its own GUI). A slider
  // touched while the list was in flight keeps its value and its token: the list predates it.
  async function refresh() {
    const since = touchTick;
    let ps: PluginParamDesc[] = [];
    try {
      ps = await platform.pluginHost.listParams(props.slot);
    } catch (e) {
      console.error('[PluginControls] listParams failed', e);
      notifyError("Couldn't load the plugin's controls", e);
    }
    setTotalParams(ps.length);
    nameById.clear();
    for (const p of ps) nameById.set(p.id, p.name);
    // A plugin can report params with an EMPTY name (seen with DecentSampler VST3). A nameless slider
    // says nothing, so the preview takes the first NAMED params; the rest stay reachable in the
    // editor and count toward "+N more".
    const preview = ps.filter((p) => p.name.trim() !== '').slice(0, PARAM_PREVIEW_COUNT);
    const init: Record<number, number> = {};
    const newer = new Map([...touched].filter(([, token]) => token > since));
    touched.clear();
    renderedIds.clear();
    for (const p of preview) {
      const token = newer.get(p.id);
      if (token === undefined) init[p.id] = p.value;
      else touched.set(p.id, token);
      renderedIds.add(p.id);
    }
    setValues(init);
    setParams(preview);
  }
  onMount(() => void refresh());
  const offParams = platform.pluginHost.onParamsChanged((s) => {
    if (s === props.slot) void refresh();
  });
  onCleanup(offParams);

  // Editor-originated param changes (a knob drag in the plugin's own GUI): reposition the matching
  // rendered slider, and always surface a last-edited readout so the link is visible even when the
  // changed param isn't in the preview slice.
  const offParam = platform.pluginHost.onParamChanged((e) => {
    if (e.slot !== props.slot) return;
    setLastEdit({ name: nameById.get(e.id) ?? `#${e.id}`, value: e.value });
    if (renderedIds.has(e.id)) {
      touched.set(e.id, ++touchTick);
      setValues(e.id, e.value);
    }
  });
  onCleanup(offParam);

  function onSlider(p: PluginParamDesc, raw: string) {
    const v = Number(raw);
    const token = ++touchTick;
    touched.set(p.id, token);
    setValues(p.id, v);
    platform.pluginHost.setParameter(props.slot, p.id, v).catch((e) => {
      console.error(`[PluginControls] setParameter refused (slot ${props.slot + 1}, param ${p.id})`, e);
      void snapBack(p.id, token);
    });
  }

  // The host refused a slider's value: show the plugin's real one again, unless the slider was touched
  // since (or the drawer remounted) while the live value was being read.
  async function snapBack(id: number, token: number) {
    const current = () => alive && touched.get(id) === token;
    if (!current()) return;
    try {
      const live = (await platform.pluginHost.listParams(props.slot)).find((q) => q.id === id);
      if (live && current()) setValues(id, live.value);
    } catch (e) {
      console.error(`[PluginControls] reading the live value failed (slot ${props.slot + 1}, param ${id})`, e);
    }
  }

  const hidden = createMemo(() => Math.max(0, totalParams() - params().length));

  return (
    <div class="pc">
      <div class="pc__params">
        <Show
          when={params().length > 0}
          fallback={
            <div class="pc__empty param--wide">
              {totalParams() > 0 ? 'no named parameters' : 'no parameters exposed'}
            </div>
          }
        >
          <For each={params()}>
            {(p) => {
              const span = p.maxValue - p.minValue;
              const step = span > 0 ? span / 1000 : 0.001;
              return (
                <label class="param">
                  <span class="param__top">
                    <span class="param__name" title={p.name}>
                      {p.name}
                    </span>
                    <span class="param__val">{(values[p.id] ?? p.value).toFixed(2)}</span>
                  </span>
                  <input
                    class="lf-range param__range"
                    type="range"
                    min={p.minValue}
                    max={p.maxValue}
                    step={step}
                    value={values[p.id] ?? p.value}
                    onInput={(ev) => onSlider(p, ev.currentTarget.value)}
                    aria-label={p.name}
                    disabled={sourcePending()}
                  />
                </label>
              );
            }}
          </For>
        </Show>
      </div>

      <Show when={lastEdit()}>
        {(e) => (
          <div class="pc__lastedit" aria-live="polite">
            editor → {e().name}: {e().value.toFixed(3)}
          </div>
        )}
      </Show>

      <Show when={hidden() > 0}>
        <div class="pc__more">+{hidden()} more params, use the editor</div>
      </Show>
    </div>
  );
}
