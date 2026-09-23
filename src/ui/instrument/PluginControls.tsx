import { For, Show, createMemo, createSignal, onCleanup, onMount } from 'solid-js';
import { createStore } from 'solid-js/store';
import { platform, type PluginDescriptor, type PluginParamDesc } from '../../platform';
import {
  editorAffinityBlocker,
  noteEditorOpened,
  pluginGain,
  setPluginGain,
  slotPendingCounts,
} from '../../audio/instrument';
import { goLive, inputArmed, monitorArmed, setMonitorGain, stopLive } from '../../audio/native-io';
import { readAudioDeviceSettings } from '../../audio/audio-settings';
import { usingAsio } from '../../audio/audio-devices';
import { notifyError } from '../../notify';
import './plugin-controls.css';

/**
 * Source-row plugin controls, split across the compact slot row and its params drawer:
 *
 *  - `PluginBar`    — the right-cluster controls that ride the compact slot row: a native LIVE monitor
 *                     toggle, the plugin EDITOR toggle (pop-out window), and the PARAMS drawer
 *                     disclosure carrying the gain readout. Everything that must stay one row tall.
 *  - `PluginParams` — the accordion drawer body: the OUTPUT gain slider + a preview grid of the plugin's
 *                     first params, all as slim groove sliders. Mounts when a plugin is loaded
 *                     (so `onParamChanged` tracks live positions even while collapsed); the drawer's open
 *                     state is CSS-toggled by the owning slot, so it never remounts on open/close.
 *
 * Both mount under the slot's `<Show keyed>` on the loaded descriptor, so a plugin swap remounts them
 * (re-fetching params / resetting editor + live). Both read the bridge's reactive gain array, so the row
 * readout and drawer slider stay in lockstep with the type-aware default.
 */
const PARAM_PREVIEW_COUNT = 12;
const GAIN_MAX = 1.5;

// Each mounted PluginBar's GO LIVE press, by slot, so the named-action table (`src/app/actions.ts`)
// runs the cap's own path (settings read, single-flight, toasts) rather than a copy of it.
const livePresses: [(() => boolean) | null, (() => boolean) | null] = [null, null];

/**
 * Press slot `slot`'s GO LIVE / INPUT LIVE cap, exactly as a click would. False when that slot shows no
 * enabled cap: no plugin loaded, the browser build, or a load or arm in flight.
 */
export function pressGoLive(slot: 0 | 1): boolean {
  return livePresses[slot]?.() ?? false;
}

/** Format a linear gain (0..1.5) as a dB string for the output readout. Uses a single U+2212 minus
 * glyph throughout (matches the −∞ branch), and shows plain "0.0 dB" at unity. */
function gainDb(v: number): string {
  if (v <= 0) return '−∞ dB';
  const db = 20 * Math.log10(v);
  if (Math.abs(db) < 0.05) return '0.0 dB';
  return `${db > 0 ? '+' : '−'}${Math.abs(db).toFixed(1)} dB`;
}

/**
 * The compact-row controls (live monitor + editor + gain readout/disclosure). Stays one row tall; the
 * flat-pill styling comes from the shared `.tgl` rules in app.css, so this only owns the wiring.
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
  const [liveBusy, setLiveBusy] = createSignal(false);
  const [liveError, setLiveError] = createSignal<string | null>(null);
  // Input stays armed after an output-stream fault so the web path can take over. Keep that degraded
  // state visible after the fault toast disappears.
  const live = createMemo(() => inputArmed()[props.slot]);
  const webMonitor = createMemo(() => live() && !monitorArmed()[props.slot]);
  // Gain readout only; the actual control lives in the drawer (PluginParams).
  const gain = () => pluginGain()[props.slot] ?? 0.9;
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

  // `quiet` = the auto-start after a dropdown pick (same rule as toggleEditor): the scan's effect flag
  // comes from the plugin's category, not its actual input bus, so a refusal here is not an error the
  // user caused — no toast, no error chip. GO LIVE stays available and reports when clicked.
  async function toggleLive(quiet = false) {
    if (liveBusy()) return; // single-flight: ignore re-clicks while an arm/disarm is pending
    const wantLive = !live();
    setLiveBusy(true);
    setLiveError(null);
    try {
      if (wantLive) {
        // Read the capture device/channel + monitor output device chosen in the global Audio Settings
        // popover (persisted) at arm time. goLive arms input THEN monitor as one unit and mutes the web
        // path; the current output level is pushed to the native monitor inside goLive.
        const s = readAudioDeviceSettings();
        const ch = s.inputChannel === '' ? null : Number(s.inputChannel);
        // ASIO uses the cached driver; the persisted Windows device IDs apply only to WASAPI.
        await goLive(props.slot, usingAsio() ? null : s.inputDeviceId || null, ch,
          usingAsio() ? null : s.outputDeviceId || null);
      } else await stopLive(props.slot);
    } catch (e) {
      // The worth-surfacing failure is going live on a plugin with no audio-input bus (a pure synth in
      // the slot) — goLive rejects at the input-arm step. A cpal monitor-open failure also lands here
      // (goLive rolled the input back). A stop failure is rare; report generically.
      console.error('[PluginControls] go-live toggle failed', e);
      if (quiet) return;
      setLiveError(wantLive ? 'input failed' : 'stop failed');
      notifyError(
        wantLive
          ? "Couldn't start live input"
          : 'Live input stop failed',
        e,
      );
    } finally {
      setLiveBusy(false);
    }
  }

  // The cap's press for pressGoLive, with the button's own disabled rule. A swap remounts the bar, so
  // the old registration only clears itself if no newer bar has replaced it.
  if (platform.pluginHost.available) {
    const slot = props.slot;
    const press = () => {
      if (sourcePending() || liveBusy()) return false;
      void toggleLive();
      return true;
    };
    livePresses[slot] = press;
    onCleanup(() => {
      if (livePresses[slot] === press) livePresses[slot] = null;
    });
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
      if (props.descriptor.isEffect === true && platform.pluginHost.available) await toggleLive(true);
      if (!gone) await toggleEditor(true);
    })();
  });

  return (
    <div class="slot__plug" onClick={(e) => e.stopPropagation()}>
      {/* Native LIVE monitor: feeds the hardware capture into this plugin AND plays its
          wet through a cpal OUTPUT stream on the same device (low-latency, bypassing the WebView2
          round-trip), muting the web monitor so the wet isn't heard twice (the looper record tap is
          untouched). Only meaningful for a plugin with an audio-input bus (amp-sim/FX). Tauri-only. */}
      <Show when={platform.pluginHost.available}>
        <button
          type="button"
          class="tgl live"
          classList={{ 'on-green': live() && !webMonitor(), 'is-degraded': webMonitor() }}
          aria-pressed={live()}
          aria-label={
            webMonitor()
              ? 'Input live, monitoring through the web path; click to stop'
              : live()
                ? `Stop live input for slot ${props.slot + 1}`
                : `Go live for slot ${props.slot + 1}`
          }
          title={webMonitor() ? 'native monitor lost; go live again to restore low-latency monitoring' : undefined}
          disabled={sourcePending() || liveBusy()}
          onClick={() => void toggleLive()}
        >
          <Show when={live()}>
            <i class="tgl__dot" aria-hidden="true" />
          </Show>
          {webMonitor() ? 'INPUT LIVE · WEB MONITOR' : live() ? 'INPUT LIVE' : 'GO LIVE'}
        </button>
      </Show>
      {/* editor toggle (open = the lit warm-white `.on` legend) */}
      <button
        type="button"
        class="tgl"
        classList={{ on: editorOpen() }}
        aria-pressed={editorOpen()}
        aria-label={editorOpen() ? `Close editor for slot ${props.slot + 1}` : `Open editor for slot ${props.slot + 1}`}
        disabled={sourcePending() || editorBusy()}
        onClick={() => void toggleEditor()}
      >
        EDITOR
      </button>
      {/* PARAMS — the params-drawer disclosure, named for what it opens. The output gain rides along as a
          dim readout (its slider is the drawer's first row, so the compact row stays one line tall). */}
      <button
        type="button"
        class="tgl slot__gain"
        classList={{ on: props.paramsOpen }}
        aria-expanded={props.paramsOpen}
        aria-controls={props.drawerId}
        aria-label={`Plugin parameters for slot ${props.slot + 1}, output gain ${gainDb(gain())}`}
        disabled={sourcePending()}
        onClick={props.onToggleParams}
      >
        PARAMS <span class="slot__gainval">{gainDb(gain())}</span>{' '}
        <span class="slot__caret" aria-hidden="true">▾</span>
      </button>
      <Show when={editorError()}>
        <span class="pc__err">{editorError()}</span>
      </Show>
      <Show when={liveError()}>
        <span class="pc__err">{liveError()}</span>
      </Show>
    </div>
  );
}

/**
 * The accordion drawer body: the OUTPUT gain slider (proves "knobs move sound") + a preview grid of the
 * plugin's first params. The `onParamChanged` subscription proves "editor → web UI updates" — a knob
 * dragged in the plugin's own GUI repositions the matching slider and shows a last-edited readout. A
 * plugin like Surge exposes thousands of params, so we render only the first PARAM_PREVIEW_COUNT — the
 * editor is the full UI.
 */
export function PluginParams(props: { slot: 0 | 1 }) {
  const [params, setParams] = createSignal<PluginParamDesc[]>([]);
  const [totalParams, setTotalParams] = createSignal(0);
  const [values, setValues] = createStore<Record<number, number>>({});
  const [lastEdit, setLastEdit] = createSignal<{ name: string; value: number } | null>(null);
  // Plugin output gain: the per-slot wet level, with a type-aware default from the bridge.
  const gain = () => pluginGain()[props.slot] ?? 0.9;
  const sourcePending = () => slotPendingCounts()[props.slot] > 0;
  const nameById = new Map<number, string>();
  const renderedIds = new Set<number>();

  // (Re)list the plugin's params and seed every rendered slider from its LIVE value — on mount, and
  // again whenever the plugin reports a wholesale change (preset loaded in its own GUI).
  async function refresh() {
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
    renderedIds.clear();
    for (const p of preview) {
      init[p.id] = p.value;
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
    if (renderedIds.has(e.id)) setValues(e.id, e.value);
  });
  onCleanup(offParam);

  function onSlider(p: PluginParamDesc, raw: string) {
    const v = Number(raw);
    setValues(p.id, v);
    void platform.pluginHost.setParameter(props.slot, p.id, v);
  }

  function applyGain(v: number) {
    const g = Math.max(0, Math.min(GAIN_MAX, v));
    setPluginGain(props.slot, g); // web wet level (record + web monitor)
    void setMonitorGain(props.slot, g); // native monitor level — kept in sync so the heard level matches
  }
  function onGainInput(input: HTMLInputElement) {
    let v = Number(input.value);
    const detentLimit = 0.03 + Number.EPSILON;
    const inUnityDetent = (value: number) => Math.abs(value - 1.0) <= detentLimit;
    if (!inUnityDetent(gain()) && inUnityDetent(v)) v = 1.0; // snap only while approaching unity
    applyGain(v);
    // Solid does not update the native range when the snapped value equals the existing signal.
    // Keep the thumb aligned with the stored value inside the unity detent.
    input.value = String(v);
  }

  const hidden = createMemo(() => Math.max(0, totalParams() - params().length));

  return (
    <div class="pc">
      <div class="pc__params">
        {/* OUTPUT gain — the per-slot wet level, defaulted by plugin type at load (amp-sim ~0.9 / synth
            ~0.1), trimmed by ear. Native range for a11y + keyboard; double-click resets to unity. */}
        <label class="param param--gain">
          <span class="param__top">
            <span class="param__name">Output</span>
            <span class="param__val">{gainDb(gain())}</span>
          </span>
          <input
            class="lf-range param__range"
            type="range"
            min={0}
            max={GAIN_MAX}
            step={0.01}
            value={gain()}
            onInput={(ev) => onGainInput(ev.currentTarget)}
            onDblClick={() => applyGain(1.0)}
            aria-label={`Plugin output gain for slot ${props.slot + 1}`}
            aria-valuetext={gainDb(gain())}
            disabled={sourcePending()}
          />
        </label>

        <Show
          when={params().length > 0}
          fallback={
            <div class="pc__empty param--gain">
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
