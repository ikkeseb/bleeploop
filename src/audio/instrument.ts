/**
 * OWNS: what is in each of the two slots — the synth id / loaded plugin, the load + unload + swap chains,
 * the plugin scan, editor affinity, and which slot is active (input routing). Native input/monitor arm:
 * `native-io.ts`; device lists, buffer size, ASIO tier: `audio-devices.ts`; the slot state cell + per-slot
 * op chain: `instrument-slots.ts`.
 */
import { createSignal } from 'solid-js';
import { inputRouter, type PluginNoteSink } from './input-router';
import { SYNTHS } from './synths';
import type { SynthEngine } from './synths/synth';
import { pluginBridge } from './plugin-bridge';
import { platform, type PluginDescriptor, type PluginInfo } from '../platform';
import { notifyError } from '../notify';
import {
  activeSlot,
  nativeHostReady,
  serializeSlot,
  setActiveSlotSignal,
  setSlotIds,
  setSlotPlugins,
  slotIds,
  slotPendingCounts,
  slotPlugins,
  withAt,
} from './instrument-slots';
import { disarmInputInternal, disarmMonitorInternal } from './native-io';
import { forgetSlotPlugin, rememberSlotPlugin } from './rig-recall';
import { warm as warmCapture } from './looper/capture';
import { reconcilePluginDescriptors, samePluginDescriptor } from './plugin-descriptor';

/**
 * Two-slot instrument host. Each slot holds EITHER a built-in synth (a selectable id + a lazily-built
 * SynthEngine) OR a native plugin (a loaded CLAP/VST3 descriptor). Only one slot is "active" at a
 * time; keyboard/MIDI input routes to that slot's instrument through `inputRouter` —
 * `applyActiveRouting()` picks the synth engine or the plugin note-sink based on the slot's kind.
 *
 * This module owns WHAT is in each slot (synth id / plugin, the load + unload + swap chains, the
 * plugin scan, editor affinity). The slot state cell + per-slot op chain live in
 * `instrument-slots.ts`; native input/monitor arm in `native-io.ts`; device lists, buffer size and
 * the ASIO tier in `audio-devices.ts`.
 */

// Scanned native plugins available to the picker (empty in the browser build).
const [availablePlugins, setAvailablePlugins] = createSignal<PluginDescriptor[]>([]);
const [scanning, setScanning] = createSignal(false);

// The live synth engine instances (not reactive — managed imperatively).
const engines: [SynthEngine | null, SynthEngine | null] = [null, null];

/**
 * Stable per-slot note sinks routing to the native plugin in that slot. STABLE refs (built once) so
 * `inputRouter.setActivePlugin` early-returns when the routing is unchanged — a fresh closure per
 * keypress would otherwise flush held notes on every key. `velocity` is the CLAP-normalised 0..1 form
 * (the input router divides MIDI velocity by 127). Web build: `noteOn`/`noteOff` are no-ops.
 */
const PLUGIN_SINKS: [PluginNoteSink, PluginNoteSink] = [
  {
    noteOn: (note, velocity) => void platform.pluginHost.noteOn(0, note, velocity),
    noteOff: (note) => void platform.pluginHost.noteOff(0, note),
  },
  {
    noteOn: (note, velocity) => void platform.pluginHost.noteOn(1, note, velocity),
    noteOff: (note) => void platform.pluginHost.noteOff(1, note),
  },
];

function findFactory(id: string) {
  return SYNTHS.find((s) => s.id === id) ?? SYNTHS[0];
}

function buildSlot(i: 0 | 1): void {
  if (engines[i]) {
    engines[i]!.dispose();
    engines[i] = null;
  }
  engines[i] = findFactory(slotIds()[i]).create();
}

/**
 * Point the input router at whatever the ACTIVE slot currently holds: the plugin note-sink (plugin
 * mode) or the synth engine (synth mode, built on demand). Idempotent — the plugin path passes a
 * stable sink ref and the synth path a stable engine ref, so repeated calls (every keypress, via
 * `ensureActive`) don't flush held notes.
 */
function applyActiveRouting(): void {
  const i = activeSlot();
  if (slotPlugins()[i]) {
    // Drop the synth-engine ref FIRST (flushes it if live) so a later panic/allNotesOff can't call
    // into a SynthEngine we dispose right after loading a plugin into this slot. Then the plugin
    // override takes over. Both calls are idempotent on repeat (engine already null, same sink ref).
    inputRouter.setActiveEngine(null);
    inputRouter.setActivePlugin(PLUGIN_SINKS[i]);
    return;
  }
  if (!engines[i]) buildSlot(i);
  inputRouter.setActivePlugin(null);
  inputRouter.setActiveEngine(engines[i]);
}

/**
 * Ensure the active slot is routed (builds its synth engine on demand). Called on every keypress. Also
 * warms the looper's capture path (no clock start) so the record-level meter reads from the first note.
 */
export function ensureActive(): void {
  applyActiveRouting();
  warmCapture();
}

/**
 * Make slot `i` the active slot and route input to it. Builds the synth engine on demand. Panics the
 * previous active slot's notes (via the router's sink-swap flush).
 */
export function setActiveSlot(i: 0 | 1): void {
  setActiveSlotSignal(i);
  applyActiveRouting();
}

/**
 * Assign synth `id` to slot `slotIndex` and make that slot the active (MIDI/keys) slot — picking a
 * source is picking what you play. If the slot was in plugin mode, the plugin is unloaded and the slot
 * reverts to this synth; a failed unload keeps the plugin and leaves the active slot where it was. If
 * the synth engine was already live it is disposed and rebuilt.
 */
export function selectSynth(slotIndex: 0 | 1, id: string): void {
  setSlotIds((prev) => withAt(prev, slotIndex, id)); // immediate UI highlight
  // Serialize the routing work on the SAME per-slot chain as selectPlugin/clearPlugin, so a synth
  // pick made while a plugin load is in flight isn't clobbered by that load's continuation: the load
  // completes, then this op clears it and reverts to the chosen synth (last action wins).
  void serializeSlot(slotIndex, async () => {
    if (slotPlugins()[slotIndex]) {
      await doClearPlugin(slotIndex); // plugin → synth: unload + revert to the now-selected synth id
      // A failed unload restores the (now silent) plugin: leave MIDI where it is rather than move it there.
      if (slotPlugins()[slotIndex]) return;
    } else if (engines[slotIndex]) {
      // Route the OLD engine away (flushing it while it's still live) BEFORE buildSlot disposes it, so
      // the router never holds a reference to a disposed SynthEngine — the same route-away-then-dispose
      // order doSelectPlugin uses. Without this, applyActiveRouting()→setActiveEngine(E_new) below sees
      // router.active === the just-disposed E_old and calls allNotesOff() on it (benign for today's
      // synths, but a latent throw for any future synth whose allNotesOff touches a live node post-dispose).
      if (activeSlot() === slotIndex) inputRouter.setActiveEngine(null);
      buildSlot(slotIndex);
    }
    setActiveSlot(slotIndex);
  });
}

/**
 * Editor affinity per plugin FILE. The same plugin file loaded in BOTH slots shares ONE OS module →
 * one GUI runtime (JUCE `MessageManager`), whose message thread binds to the FIRST owner thread that
 * opens an editor — and stays bound while the module is loaded (closing the editor does NOT release
 * it; a second concurrent OR serial editor from the other slot wedges inside `attached()`/`gui.create`
 * forever → `open_editor` timeout → frozen half-window → closing it kills the app). Mixed formats of
 * the same plugin (CLAP + VST3 = two modules) are fine. So: the first slot to open an editor for a
 * path OWNS that path's editor; the other slot is refused with a toast (guard in
 * `PluginControls.toggleEditor`). The entry dies when the module truly unloads — i.e. when the LAST
 * slot holding the path unloads. Real fix (one shared GUI thread for all editors) is a fragile
 * host rework, deliberately deferred.
 */
const editorAffinity = new Map<string, 0 | 1>();

/** Non-null = refuse: the OTHER slot already owns this file's editor (returns its 1-based label). */
export function editorAffinityBlocker(slot: 0 | 1, path: string): string | null {
  const owner = editorAffinity.get(path);
  return owner !== undefined && owner !== slot ? `slot ${owner + 1}` : null;
}

/** Record a successful editor open — first opener becomes the path's editor owner. */
export function noteEditorOpened(slot: 0 | 1, path: string): void {
  if (!editorAffinity.has(path)) editorAffinity.set(path, slot);
}

/** Drop a path's affinity when NO slot holds it anymore (the module actually unloaded). */
function releaseEditorAffinity(outgoingPath: string | undefined): void {
  if (!outgoingPath) return;
  if (!slotPlugins().some((d) => d?.path === outgoingPath)) editorAffinity.delete(outgoingPath);
}

/**
 * Load native plugin `desc` into `slot`. Tears down any plugin already in the slot first (clean
 * swap), loads via the host (Rust provisions the SharedBuffer → the registered sink wires the audio
 * bridge), disposes the slot's synth engine, and — if the slot is active — routes notes to it. On
 * load failure the slot is left on its synth. Serialized per slot (see `serializeSlot`).
 */
export function selectPlugin(slot: 0 | 1, desc: PluginDescriptor): Promise<void> {
  if (!nativeHostReady()) return Promise.resolve();
  return serializeSlot(slot, () => doSelectPlugin(slot, desc));
}

async function doSelectPlugin(slot: 0 | 1, desc: PluginDescriptor): Promise<void> {
  const outgoing = slotPlugins()[slot];
  if (samePluginDescriptor(outgoing, desc)) return; // already loaded
  // Swap: tear down + unload any existing plugin in this slot before loading the new one. A failed
  // unload aborts the swap — the native host may still hold the old plugin, so a load would only fail
  // on "slot already loaded" while the UI showed the new one.
  if (outgoing && !(await unloadSlotPlugin(slot, outgoing, 'swap'))) return;
  // Tell the bridge this slot's plugin kind BEFORE the load, so acceptPluginBuffer picks the right
  // output-gain default (FX ~unity / synth conservative) from the scan category, not the input bus.
  const loadToken = pluginBridge.beginPluginLoad(slot, desc.isEffect);
  try {
    await platform.pluginHost.loadPlugin(slot, desc.path, desc.id, loadToken);
  } catch (e) {
    pluginBridge.cancelPluginLoad(slot, loadToken);
    console.error('[instrument] plugin load failed', e);
    notifyError('Plugin load failed', e);
    // The slot holds nothing now, so the next launch must not retry this load (`rig-recall.ts`).
    forgetSlotPlugin(slot);
    if (activeSlot() === slot) applyActiveRouting(); // ensure we're back on the synth
    return;
  }
  setSlotPlugins((prev) => withAt(prev, slot, desc));
  rememberSlotPlugin(slot, desc);
  // Route to the plugin FIRST (drops the live synth-engine ref), THEN dispose the engine — so the
  // router never holds a reference to a disposed SynthEngine. If the slot isn't active the router
  // doesn't reference this engine anyway, so disposing it is safe regardless. An instrument plugin
  // makes its slot the MIDI/keys slot; an effect (amp sim on the guitar) leaves routing where it is.
  if (desc.isEffect !== true) setActiveSlot(slot);
  else if (activeSlot() === slot) applyActiveRouting();
  if (engines[slot]) {
    engines[slot]!.dispose();
    engines[slot] = null;
  }
}

/**
 * Tear down + natively unload the plugin in `slot`. The slot reads empty while the unload is in
 * flight: a clear routes an active slot to its synth at once, a swap leaves it silent (no synth is
 * built only to be disposed when the next plugin lands). On failure the OLD descriptor is restored
 * and routing follows it: the host may still hold that plugin, so the slot keeps saying so and a
 * later swap/clear retries the unload. Its audio bridge stays torn down (silent) until then. Returns
 * whether it unloaded.
 */
async function unloadSlotPlugin(slot: 0 | 1, outgoing: PluginDescriptor, path: 'swap' | 'clear'): Promise<boolean> {
  await disarmMonitorInternal(slot); // stop the native monitor before its plugin goes away
  await disarmInputInternal(slot); // the outgoing plugin's input feed must stop before its unload
  pluginBridge.teardownPluginSlot(slot); // stop the audio drain + release the hop-1 buffer (sync)
  setSlotPlugins((prev) => withAt(prev, slot, null));
  // Both calls flush held notes: the next plugin reuses the SAME stable PLUGIN_SINKS ref, so a later
  // applyActiveRouting would early-return without releasing a note held across the swap.
  if (activeSlot() === slot) {
    if (path === 'swap') inputRouter.setActivePlugin(null);
    else applyActiveRouting(); // back to the slot's synth immediately
  }
  try {
    await platform.pluginHost.unloadPlugin(slot); // Rust joins the producer + deactivates
  } catch (e) {
    console.error(`[instrument] plugin unload${path === 'swap' ? ' (swap)' : ''} failed`, e);
    notifyError('Plugin unload failed', 'The plugin stays in the slot but is silent. Choose none or another plugin to retry.');
    setSlotPlugins((prev) => withAt(prev, slot, outgoing));
    // Route to the restored plugin FIRST (drops any synth-engine ref), THEN dispose the engine built
    // while the slot read empty, so no idle SynthEngine lives beside the plugin.
    if (activeSlot() === slot) applyActiveRouting();
    if (engines[slot]) {
      engines[slot]!.dispose();
      engines[slot] = null;
    }
    return false;
  }
  forgetSlotPlugin(slot);
  releaseEditorAffinity(outgoing.path);
  return true;
}

/**
 * Unload the plugin in `slot` and revert it to its synth. Tears down the audio bridge synchronously
 * + reverts routing optimistically (so the slot is silent immediately), then awaits the native
 * unload; a failed unload restores the plugin (see `unloadSlotPlugin`). No-op if the slot holds no
 * plugin. Serialized per slot (see `serializeSlot`).
 */
export function clearPlugin(slot: 0 | 1): Promise<void> {
  return serializeSlot(slot, () => doClearPlugin(slot));
}

async function doClearPlugin(slot: 0 | 1): Promise<void> {
  const outgoing = slotPlugins()[slot];
  if (outgoing) await unloadSlotPlugin(slot, outgoing, 'clear');
}

/**
 * Resync the native host with this module's slot state at startup (frontend-reload wedge). A WebView
 * reload (or crash-recovery) resets this module to synth defaults while the Rust host keeps its
 * plugins loaded → every later load fails ("slot N already has a plugin loaded") and the
 * editor/GO-LIVE chrome never renders, with a full app restart the only exit.
 *
 * We query the host for the slots it still holds and UNLOAD each through the PRODUCTION unload path
 * (`platform.pluginHost.unloadPlugin` → Rust `unload`): it joins the owner thread — which drops that
 * slot's `!Send` cpal input/monitor streams, including the ASIO duplex holder, exactly as a normal
 * unload does — and `Close()`s the SharedBuffer in the documented teardown order. We do NOT route
 * through `doClearPlugin`: this module already thinks the slot is empty, so that path would
 * early-return and leave the native slot loaded. Adopting the loaded plugin instead (rebuilding the
 * hop-2 bridge + editor affinity + UI state) is future work — unload-to-clean is the correct v1.
 *
 * Idempotent and cheap on a normal cold start: the host reports no loaded slots, so this is a single
 * query and zero unloads. No-op / empty in the browser build.
 */
export async function resyncNativeSlots(): Promise<void> {
  if (!platform.pluginHost.available) return;
  let loaded: PluginInfo[];
  try {
    loaded = await platform.pluginHost.listLoaded();
  } catch (e) {
    console.error('[instrument] resync: list loaded slots failed', e);
    notifyError('Could not check the plugin host state', e);
    return;
  }
  for (const { slot, descriptor } of loaded) {
    // Log as an error so it reaches the release log (the codebase's log channel) — recovering a
    // stranded native slot is an abnormal-state event worth a field breadcrumb, not routine.
    console.error(`[instrument] resync: unloading stranded plugin in slot ${slot} (${descriptor.name})`);
    // Serialize on the slot's op chain so the unload can't interleave with any concurrent slot op.
    await serializeSlot(slot, async () => {
      try {
        await platform.pluginHost.unloadPlugin(slot);
      } catch (e) {
        console.error('[instrument] resync: stranded-slot unload failed', e);
        notifyError('Failed to clear a stranded plugin slot', e);
      }
    });
  }
}

/**
 * Scan installed native plugins into `availablePlugins` (no-op / empty in the browser build).
 * Re-entrancy-guarded: a rescan fired while one is in flight is DROPPED (not queued) — the picker's
 * rescan button is also `disabled` during a scan, but the guard additionally covers the startup scan
 * racing a fast manual click and any programmatic double-call via `__lf.scanForPlugins`. Only
 * `availablePlugins` is refreshed; loaded slots (`slotPlugins`), routing and audio are untouched, so a
 * rescan never disturbs playback. A plugin still loaded in a slot but MISSING from the fresh scan
 * (e.g. its `--scan-one` child hit the 20s timeout this pass) is merged back into the list, so its
 * picker button + active highlight survive the rescan instead of vanishing while it's still playing.
 */
export async function scanForPlugins(opts: { force?: boolean } = {}): Promise<void> {
  if (!platform.pluginHost.available) return;
  if (!nativeHostReady()) return;
  if (scanning()) return;
  setScanning(true);
  try {
    const scanned = await platform.pluginHost.scanPlugins(opts.force ?? false);
    setAvailablePlugins(reconcilePluginDescriptors(scanned, slotPlugins()));
  } catch (e) {
    console.error('[instrument] plugin scan failed', e);
    notifyError('Plugin scan failed', e);
  } finally {
    setScanning(false);
  }
}

// ---------------------------------------------------------------------------
// Plugin output gain — per-slot wet level, type-aware default + UI trim
// ---------------------------------------------------------------------------

/**
 * Set the output gain (0..1.5) of slot `slot`'s loaded plugin (the plugin panel's output slider, or
 * `__lf.setPluginGain`). Thin pass-through to the audio bridge; no-op if no plugin is wired there.
 */
export function setPluginGain(slot: 0 | 1, value: number): void {
  pluginBridge.setGain(slot, value);
}

/** Reactive intended output gain for both slots; null means no plugin is wired there yet. */
export const pluginGain = pluginBridge.gains;

/** Read-only reactive accessors: the synth id per slot, the loaded plugin per slot (null = synth
 * mode), the active slot index. (Owned by `instrument-slots.ts`; re-exported as the UI's one path.) */
export { slotIds, slotPendingCounts, slotPlugins, activeSlot };

/**
 * Derived predicate: is the ACTIVE slot playing the built-in GM drum kit (synth id 'drum' AND not
 * overridden by a loaded plugin — a plugin always plays chromatically, never the pad grid)? The ONE
 * source for the "keyboard is a pad grid" decision, read by app.tsx (chrome noun + the digit-select
 * yield) and Keyboard.tsx (pad-vs-piano layout) so the two can never disagree.
 */
export function activeIsDrum(): boolean {
  return !slotPlugins()[activeSlot()] && slotIds()[activeSlot()] === 'drum';
}

/** Read-only reactive accessors: scanned plugins + scan-in-progress flag (the picker). */
export { availablePlugins, scanning };
export { nativeHostReady };
