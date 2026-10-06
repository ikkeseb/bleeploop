import { For, Show, createSignal } from 'solid-js';
import {
  activeSlot,
  availablePlugins,
  selectOff,
  selectPlugin,
  selectSynth,
  scanning,
  setActiveSlot,
  slotIds,
  slotLevel,
  slotOff,
  slotPendingCounts,
  slotPlugins,
} from '../state/instrument';
import { inputArmed } from '../state/native-io';
import {
  pluginDescriptorKey,
  pluginPickerLabel,
  samePluginDescriptor,
} from '../state/plugin-descriptor';
import { SYNTHS } from '../state/instruments';
import { platform } from '../../platform';
import { engineDevice, engineOpenFailure } from '../state/engine-store';
import { PluginBar, PluginParams } from './PluginControls';
import { InputPick, LiveButton, SlotVolume } from './SlotControls';
import { liveShown } from './live';

/**
 * One instrument slot (A / B) — a compact card. Its header: the A/B identity button, the source picker
 * (Off, the built-in instruments, the scanned plugins), the slot's volume, and the source's
 * own controls — the input pick and GO LIVE of a source that takes input, the plugin's EDITOR / PARAMS —
 * on a second line, or beside the picker when the card is wide. Below it, the plugin params drawer.
 * app.tsx's renderInstrument mounts two of these (slot 0 / slot 1). The per-slot `paramsOpen` disclosure
 * is state LOCAL to each instance (created once per mount) so opening one slot's params never touches
 * the other — and it is a SIBLING of the looper region, so opening it never remounts the looper's RAF
 * canvases.
 */
export function InstrumentSlot(props: { slot: 0 | 1 }) {
  const slotIdx = props.slot;
  const isActive = () => activeSlot() === slotIdx;
  const plugin = () => slotPlugins()[slotIdx];
  const pending = () => slotPendingCounts()[slotIdx] > 0;
  const letter = String.fromCharCode(65 + slotIdx); // A / B
  const drawerId = `slot-${slotIdx}-params`;
  const showPlugins = () => platform.pluginHost.available && availablePlugins().length > 0;
  // Per-slot accordion disclosure for the plugin params drawer. Created once (the component mounts once
  // per slot); a sibling of the looper region, so opening it never remounts the looper.
  const [paramsOpen, setParamsOpen] = createSignal(false);
  // One-shot: the descriptor key the user just picked in the picker. The PluginBar that mounts for it
  // auto-starts (live + editor) and clears this; a reload resync never sets it, so it never reopens
  // windows. Plain variable — it is read once at PluginBar mount, nothing renders from it.
  let freshPickKey: string | null = null;
  // Active state colour: cyan = engaged/selection; a live slot lifts it to play-green.
  const sc = () => (inputArmed()[slotIdx] ? 'var(--play)' : 'var(--cyan)');
  // The picker's value: '' (its hidden "Updating…" entry) while a source change is queued.
  const source = () =>
    pending() ? '' : plugin() ? pluginDescriptorKey(plugin()!) : slotOff()[slotIdx] ? 'off' : slotIds()[slotIdx];
  const pick = (value: string) => {
    if (value === 'off' || SYNTHS.some((s) => s.id === value)) {
      freshPickKey = null;
      // selectSynth activates the slot itself once its queued work lands (not when a failed unload
      // keeps the plugin); Off leaves the MIDI slot where it is.
      if (value === 'off') selectOff(slotIdx);
      else selectSynth(slotIdx, value);
      return;
    }
    const desc = availablePlugins().find((p) => pluginDescriptorKey(p) === value);
    if (!desc) return;
    freshPickKey = value;
    // A failed load mounts no PluginBar — drop the flag so a later resync can't inherit it.
    void selectPlugin(slotIdx, desc).then(() => {
      if (!samePluginDescriptor(slotPlugins()[slotIdx], desc)) freshPickKey = null;
    });
  };
  return (
    // Presentational group (not itself a button — it holds buttons). Whole-card onClick is a mouse
    // convenience; the keyboard/SR activation control is the A/B identity button.
    <div
      class="slot"
      classList={{ 'slot--active': isActive() }}
      role="group"
      aria-label={`Slot ${slotIdx + 1}${isActive() ? ', active' : ''}`}
      aria-busy={pending()}
      style={{ '--sc': sc() }}
      onClick={() => setActiveSlot(slotIdx)}
    >
      <div class="slot__row">
        <button
          class="slot__id"
          aria-pressed={isActive()}
          aria-label={`Activate slot ${slotIdx + 1}`}
          onClick={(e) => {
            e.stopPropagation();
            setActiveSlot(slotIdx);
          }}
        >
          {letter}
        </button>
        {/* The source picker: Off first (no notes; GO LIVE passes the slot's input dry),
            the built-in instruments, then the native plugins (Tauri only; ONE list scales to any count). */}
        <select
          class="slot__source"
          aria-label={`Source for slot ${slotIdx + 1}`}
          aria-busy={pending()}
          disabled={pending()}
          value={source()}
          onClick={(e) => e.stopPropagation()}
          onChange={(e) => {
            e.stopPropagation();
            pick(e.currentTarget.value);
          }}
        >
          <option value="" hidden selected={source() === ''}>
            Updating…
          </option>
          <option value="off" selected={source() === 'off'}>
            Off
          </option>
          <optgroup label="Built-in">
            <For each={SYNTHS}>
              {(s) => (
                <option value={s.id} selected={source() === s.id}>
                  {s.name}
                </option>
              )}
            </For>
          </optgroup>
          <Show when={showPlugins()}>
            <optgroup label="Plugins">
              <For each={availablePlugins()}>
                {(p) => (
                  <option value={pluginDescriptorKey(p)} title={p.path} selected={source() === pluginDescriptorKey(p)}>
                    {pluginPickerLabel(p, availablePlugins())}
                  </option>
                )}
              </For>
            </optgroup>
          </Show>
        </select>
        <Show when={slotLevel(slotIdx) !== null}>
          <SlotVolume slot={slotIdx} />
        </Show>
        {/* The source's own controls. `keyed` on the plugin so a swap remounts them (resets editor/live). */}
        <Show when={liveShown(slotIdx) || plugin()}>
          <div class="slot__acts" onClick={(e) => e.stopPropagation()}>
            <Show when={liveShown(slotIdx)}>
              <InputPick slot={slotIdx} />
            </Show>
            <Show when={liveShown(slotIdx)}>
              <LiveButton slot={slotIdx} />
            </Show>
            <Show keyed when={slotPlugins()[slotIdx]}>
              {(desc) => (
                <PluginBar
                  slot={slotIdx}
                  descriptor={desc}
                  paramsOpen={paramsOpen()}
                  onToggleParams={() => setParamsOpen((v) => !v)}
                  drawerId={drawerId}
                  autoStart={freshPickKey === pluginDescriptorKey(desc)}
                  onAutoStartDone={() => (freshPickKey = null)}
                />
              )}
            </Show>
          </div>
        </Show>
      </div>
      <Show when={platform.pluginHost.available && availablePlugins().length === 0}>
        <span class="slot__plugin-note" role="note">
          {scanning()
            ? 'scanning plugins…'
            : !engineDevice() && engineOpenFailure()
              ? 'No audio device open — see Audio Settings'
              : 'No plugins found · the standard CLAP, VST3 and VST2 folders are scanned · add your own in Audio Settings · rescan ⟳ in the command bar'}
        </span>
      </Show>
      {/* Params drawer (accordion). Mounted whenever a plugin is loaded (so onParamChanged tracks live
          positions even while collapsed); open/close is CSS-only, so it never remounts on toggle. */}
      <Show keyed when={slotPlugins()[slotIdx]}>
        {(_desc) => (
          <div
            class="slot__drawer"
            classList={{ 'slot__drawer--open': paramsOpen() }}
            id={drawerId}
            onClick={(e) => e.stopPropagation()}
          >
            <PluginParams slot={slotIdx} />
          </div>
        )}
      </Show>
    </div>
  );
}
