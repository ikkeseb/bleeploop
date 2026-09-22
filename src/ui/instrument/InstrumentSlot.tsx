import { For, Show, createSignal } from 'solid-js';
import {
  activeSlot,
  availablePlugins,
  clearPlugin,
  selectPlugin,
  selectSynth,
  scanning,
  setActiveSlot,
  slotIds,
  slotPendingCounts,
  slotPlugins,
} from '../../audio/instrument';
import { inputArmed } from '../../audio/native-io';
import {
  pluginDescriptorKey,
  pluginPickerLabel,
  samePluginDescriptor,
} from '../../audio/plugin-descriptor';
import { SYNTHS } from '../../audio/synths';
import { platform } from '../../platform';
import { PluginBar, PluginParams } from './PluginControls';

/**
 * One instrument slot (A / B) — a compact glass card (mockup .src): the A/B identity button, the
 * source label, the six synth segment pills OR the native plugin controls, the native-plugin picker,
 * and the params drawer. app.tsx's renderInstrument mounts two of these
 * (slot 0 / slot 1). The per-slot `paramsOpen` disclosure is state LOCAL to each instance (created once
 * per mount) so opening one slot's params never touches the other — and it is a SIBLING of the looper
 * region, so opening it never remounts the looper's RAF canvases.
 */
export function InstrumentSlot(props: { slot: 0 | 1 }) {
  const slotIdx = props.slot;
  const isActive = () => activeSlot() === slotIdx;
  const plugin = () => slotPlugins()[slotIdx];
  const pending = () => slotPendingCounts()[slotIdx] > 0;
  const synthName = () => SYNTHS.find((s) => s.id === slotIds()[slotIdx])?.name ?? '';
  const letter = String.fromCharCode(65 + slotIdx); // A / B
  const drawerId = `slot-${slotIdx}-params`;
  // Per-slot accordion disclosure for the plugin params drawer. Created once (the component mounts once
  // per slot); a sibling of the looper region, so opening it never remounts the looper.
  const [paramsOpen, setParamsOpen] = createSignal(false);
  // One-shot: the descriptor key the user just picked in the dropdown. The PluginBar that mounts for it auto-starts
  // (live + editor) and clears this; a reload resync never sets it, so it never reopens windows. Plain
  // variable — it is read once at PluginBar mount, nothing renders from it.
  let freshPickKey: string | null = null;
  // Active state colour: cyan = engaged/selection; a live plugin lifts it to play-green.
  const sc = () => (inputArmed()[slotIdx] ? 'var(--play)' : 'var(--cyan)');
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
        <div class="slot__meta">
          <span class="slot__k">Source{pending() ? '' : ` · ${plugin() ? 'Plugin' : 'Synth'}`}</span>
          <span class="slot__name">
            {pending() ? 'Updating…' : plugin() ? plugin()!.name : synthName()}
            <Show when={plugin() && !pending()}>
              <small>{plugin()!.format}</small>
            </Show>
          </span>
        </div>
        <div class="slot__acts">
          {/* Right cluster: the six synth segment pills when in synth mode; the native plugin controls
              (live monitor + editor + gain/params disclosure) when a plugin is loaded. `keyed` so a
              plugin swap re-mounts (resets editor/live). */}
          <Show
            when={plugin()}
            fallback={
              <div class="seg" role="group" aria-label={`Synth for slot ${slotIdx + 1}`}>
                <For each={SYNTHS}>
                  {(s) => (
                    <button
                      type="button"
                      class="tgl"
                      classList={{ on: !pending() && !plugin() && slotIds()[slotIdx] === s.id }}
                      aria-pressed={!pending() && !plugin() && slotIds()[slotIdx] === s.id}
                      aria-label={`${s.name} for slot ${slotIdx + 1}`}
                      disabled={pending()}
                      onClick={(e) => {
                        e.stopPropagation(); // don't re-trigger slot activation
                        selectSynth(slotIdx, s.id);
                      }}
                    >
                      {s.name}
                    </button>
                  )}
                </For>
              </div>
            }
          >
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
          </Show>
          {/* Native plugins (Tauri only) collapse into ONE dropdown per slot — scales to any plugin
              count. "— none —" reverts the slot to its synth. Present whether or not a plugin is loaded
              (it's how you load one), so it sits after the synth pills / plugin controls. */}
          <Show when={platform.pluginHost.available && availablePlugins().length > 0}>
            <select
              class="slot__select"
              classList={{ 'is-loaded': !!plugin() }}
              aria-label={`Native plugin for slot ${slotIdx + 1}`}
              aria-busy={pending()}
              disabled={pending()}
              value={pending() || !plugin() ? '' : pluginDescriptorKey(plugin()!)}
              onClick={(e) => e.stopPropagation()}
              onChange={(e) => {
                e.stopPropagation();
                const key = e.currentTarget.value;
                if (!key) {
                  freshPickKey = null;
                  void clearPlugin(slotIdx); // "— none —" → back to the slot's synth
                  return;
                }
                const desc = availablePlugins().find((p) => pluginDescriptorKey(p) === key);
                if (!desc) return;
                freshPickKey = key;
                // A failed load mounts no PluginBar — drop the flag so a later resync can't inherit it.
                void selectPlugin(slotIdx, desc).then(() => {
                  if (!samePluginDescriptor(slotPlugins()[slotIdx], desc)) freshPickKey = null;
                });
              }}
            >
              <option value="">{pending() ? 'Updating…' : '— none —'}</option>
              <For each={availablePlugins()}>
                {(p) => (
                  <option value={pluginDescriptorKey(p)} title={p.path}>
                    {pluginPickerLabel(p, availablePlugins())}
                  </option>
                )}
              </For>
            </select>
          </Show>
        </div>
      </div>
      <Show when={platform.pluginHost.available && availablePlugins().length === 0}>
        <span class="slot__plugin-note" role="note">
          {scanning()
            ? 'scanning plugins…'
            : 'No plugins found · CLAP in %COMMONPROGRAMFILES%\\CLAP, VST3 in %COMMONPROGRAMFILES%\\VST3 · rescan ⟳ in the command bar'}
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
