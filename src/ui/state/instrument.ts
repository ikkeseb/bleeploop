/**
 * OWNS: what is in each of the two slots — the built-in instrument id / Off / loaded plugin, the load +
 * unload + swap chains (and the in-place reload a session's tone asks for), the plugin scan, editor
 * affinity, which slot is active (the engine's note target), each slot's level (its instrument's, its
 * plugin's or its input's) and the source and levels kept for the next launch. GO LIVE: `native-io.ts`;
 * device lists, buffer size, ASIO tier: `audio-devices.ts`; the slot state cell + per-slot op chain:
 * `instrument-slots.ts`; tones in a session: `slot-tones.ts`.
 */
import { batch, createSignal } from 'solid-js';
import { SYNTHS } from './instruments';
import {
  input,
  platform,
  sendEngine,
  type InstrumentId,
  type NoteTarget,
  type PluginDescriptor,
  type PluginInfo,
} from '../../platform';
import { notifyError } from '../../notify';
import {
  activeSlot,
  isBusyRefusal,
  nativeHostReady,
  serializeSlot,
  setActiveSlotSignal,
  setSlotIds,
  setSlotOff,
  setSlotInstances,
  setSlotPlugins,
  slotIds,
  slotInstance,
  slotInstances,
  slotOff,
  slotPendingCounts,
  slotPlugins,
  withAt,
} from './instrument-slots';
import { disarmEngineLive, disarmInputInternal, holdEngineLive, inputArmed, resumeEngineLive } from './native-io';
import { readStoredNumber, writeStoredNumber } from './persist';
import { forgetSlotPlugin, rememberSlotPlugin } from './rig-recall';
import { pluginDescriptorKey, reconcilePluginDescriptors, samePluginDescriptor } from './plugin-descriptor';

/**
 * Two-slot instrument host. Each slot holds EITHER a built-in instrument (a selectable id the engine
 * plays), Off (nothing plays, GO LIVE passes the slot's input dry) OR a native plugin (a loaded
 * CLAP/VST3/VST2 descriptor). Only one slot is "active" at a time; keyboard/MIDI input routes to that slot
 * through native MIDI's router, which this module tells the note target (`routeEngine`).
 */

// Scanned native plugins available to the picker (empty in the browser build).
const [availablePlugins, setAvailablePlugins] = createSignal<PluginDescriptor[]>([]);
const [scanning, setScanning] = createSignal(false);
// The scan in flight, and the one asked for while it runs (`scanForPlugins`).
let scanRun: Promise<void> | null = null;
let scanAgain: { force: boolean } | null = null;

// What the player chose this launch: a source per slot (a synth, a plugin or none) and the active
// slot. The rig recall (`restorePlugin`) leaves a chosen slot alone and moves the MIDI slot only while
// nothing is chosen, so a restore that lands mid-play overrides nothing the player set.
const chosenSource: [boolean, boolean] = [false, false];
let chosenActive = false;
const nothingChosen = () => !chosenSource[0] && !chosenSource[1] && !chosenActive;

// Every user source pick per slot, counted at enqueue: the last pick wins, so a pick queued behind a
// pending one that a newer pick has since overtaken does nothing when its turn comes (`serializePick`).
const pickGenerations: [number, number] = [0, 0];

/** A user source pick on `slot`'s chain (`selectPlugin`, `selectSynth`, `selectOff`, `clearPlugin`),
 * skipped when it runs if a newer pick was made meanwhile. */
function serializePick(slot: 0 | 1, op: () => Promise<void>): Promise<void> {
  const generation = ++pickGenerations[slot];
  return serializeSlot(slot, () => (pickGenerations[slot] === generation ? op() : Promise.resolve()));
}

// Every native load and unload a slot has begun, counted: one plugin instance per value. A tone reload
// that saw the slot's plugin before a session import runs only while the count is unchanged
// (`reloadPlugin`); the same plugin loaded again meanwhile is another instance.
const sourceOps: [number, number] = [0, 0];

/** Which instance `slot` holds: read before a session import asks the host, for its reload. */
export const slotSourceGeneration = (slot: 0 | 1): number => sourceOps[slot];

/** A slot's volume range (the lane faders' and the plugin output's): unity at 1. */
export const SLOT_GAIN_MAX = 1.5;

/** A plugin's output level before the player sets one, by its scan kind: an amp-sim/FX is already
 * gain-staged, so its wet wants ~unity; a synth's default patches clip, so it starts conservative. */
const FX_DEFAULT_GAIN = 0.9;
const SYNTH_DEFAULT_GAIN = 0.1;

// Kept for the next launch: each slot's source without its plugin (an instrument id, or 'off'; a plugin
// over it comes back through the rig recall), each slot's input level, each instrument's level and each
// plugin's level per slot.
const sourceKey = (slot: 0 | 1) => `lf.slotSource.${slot}`;
const inputGainKey = (slot: 0 | 1) => `lf.slotInputGain.${slot}`;
const synthGainKey = (id: string) => `lf.synthGain.${id}`;
const pluginGainKey = (slot: 0 | 1, desc: PluginDescriptor) => `lf.pluginGain.${slot}.${pluginDescriptorKey(desc)}`;

/** The level `desc` last had in `slot`, else its kind's default (an unclassified plugin at the quieter
 * synth level). */
const savedPluginLevel = (slot: 0 | 1, desc: PluginDescriptor): number =>
  readStoredNumber(pluginGainKey(slot, desc), desc.isEffect === true ? FX_DEFAULT_GAIN : SYNTH_DEFAULT_GAIN, 0, SLOT_GAIN_MAX);

function writeSource(slot: 0 | 1, source: string): void {
  try {
    localStorage.setItem(sourceKey(slot), source);
  } catch {
    /* persistence is best-effort */
  }
}

for (const slot of [0, 1] as const) {
  let saved: string | null = null;
  try {
    saved = localStorage.getItem(sourceKey(slot));
  } catch {
    /* unreadable: the default synth */
  }
  if (saved === 'off') setSlotOff((prev) => withAt(prev, slot, true));
  else if (saved !== null && SYNTHS.some((s) => s.id === saved)) setSlotIds((prev) => withAt(prev, slot, saved));
}

// Each slot's input level (what an Off slot's live input is heard and recorded at: the engine's slot
// gain while no plugin is loaded) and each built-in instrument's level.
const [inputGains, setInputGains] = createSignal<[number, number]>([
  readStoredNumber(inputGainKey(0), 1, 0, SLOT_GAIN_MAX),
  readStoredNumber(inputGainKey(1), 1, 0, SLOT_GAIN_MAX),
]);
const [synthGains, setSynthGains] = createSignal<Readonly<Record<string, number>>>(
  Object.fromEntries(SYNTHS.map((s) => [s.id, readStoredNumber(synthGainKey(s.id), 1, 0, SLOT_GAIN_MAX)])),
);

// A plugin swap in flight per slot, from its unload until the next plugin's load settled (a tone reload
// is one too): the slot reads empty meanwhile, yet its instrument must not play, so it routes nowhere.
const swapping: [boolean, boolean] = [false, false];
// A hot swap in flight per slot (`doSwapPlugin`): the outgoing plugin plays until the native commit, so
// the slot keeps showing and routing to it, and its gain sends wait until it and any recovery ended
// (`gainHeld`). `hotSwaps` ends at a failed answer: the recovery may empty the slot, so a GO LIVE press
// meanwhile waits behind it on the slot's chain.
const [hotSwaps, setHotSwaps] = createSignal<[boolean, boolean]>([false, false]);
/** Whether a hot swap's answer is pending on `slot` (reactive): its GO LIVE is not held behind it (`live.ts`). */
export const hotSwapPending = (slot: 0 | 1): boolean => hotSwaps()[slot];
const swapInFlight: [boolean, boolean] = [false, false];
const gainHeld: [boolean, boolean] = [false, false];
// A slot a failed swap left empty while its live-off did not reach the host (`adoptSwapFailure`): it may
// pass its input dry, so its GO LIVE stays (`live.ts`) until the feed says it is off or a plugin lands.
const [strandedLive, setStrandedLive] = createSignal<[boolean, boolean]>([false, false]);
/** Whether `slot` keeps GO LIVE as the stop for a live-off a failed swap's recovery could not send. */
export const liveStranded = (slot: 0 | 1): boolean => strandedLive()[slot];

/** Route the notes to slot `i`'s source: its plugin, its instrument or nowhere (Off, or a swap in
 * flight). Native MIDI's router releases what sounds on the target it leaves, then selects (a target
 * switch), and changes nothing for the same slot and target again, so a click on the active slot cuts
 * no chord. */
function routeEngine(i: 0 | 1): void {
  const target: NoteTarget = slotPlugins()[i]
    ? { Slot: i }
    : swapping[i] || slotOff()[i]
      ? 'Off'
      : { Builtin: slotIds()[i] as InstrumentId };
  input.selectTarget(i, target);
}

// Per-slot plugin gain; null = no plugin.
const [engineGains, setEngineGains] = createSignal<[number | null, number | null]>([null, null]);

/** The engine's gain for `slot`: its plugin's, else its input level (an unload must not leave the
 * outgoing plugin's gain on the input an Off slot records). */
const slotEngineGain = (slot: 0 | 1): number => engineGains()[slot] ?? inputGains()[slot];

/** While a hot swap is pending, a send is held: native's commit records the swap's own gain, so the UI
 * sends nothing for the slot until the answer (`doSwapPlugin`). */
function setEngineGain(slot: 0 | 1, gain: number | null, send = true): void {
  setEngineGains((prev) => withAt(prev, slot, gain));
  if (send && swapInFlight[slot]) gainHeld[slot] = true;
  else if (send) sendEngine({ SetSlotGain: [slot, slotEngineGain(slot)] });
}

/**
 * Send what this module and `native-io.ts` keep to an engine that may not have it (a new engine, a
 * WebView reload): the note target, the slot gains, the instrument levels, and on the page's `first`
 * reset no live slot (a reload's disarm). A later reset sends no live flag: the engine's settings
 * replay has them, and the feed's may be older than a live-off in flight (an unload's). A reload's held notes are native MIDI's to release (the document's epoch), and a new
 * engine gets the target and wheels from the settings replay. A slot with a hot swap pending gets no
 * gain: the replay has the commit's, and the UI's may still be the outgoing's (`setEngineGain`).
 */
export function engineResync(first: boolean): void {
  applyActiveRouting();
  for (const slot of [0, 1] as const) if (!swapInFlight[slot]) sendEngine({ SetSlotGain: [slot, slotEngineGain(slot)] });
  for (const { id } of SYNTHS) sendEngine({ SetInstrumentGain: [id as InstrumentId, synthGains()[id]] });
  if (first) disarmEngineLive();
}

/** Point the note target at the ACTIVE slot. Idempotent (native MIDI ignores the same slot and target),
 * so repeated calls — every keypress, via `ensureActive` — don't flush held notes. */
function applyActiveRouting(): void {
  routeEngine(activeSlot());
}

/** Ensure the active slot is routed. Called on every on-screen and PC keypress. */
export function ensureActive(): void {
  applyActiveRouting();
}

/**
 * Make slot `i` the active slot and route input to it (the player's choice: the slot card). Releases the
 * previous target's held notes.
 */
export function setActiveSlot(i: 0 | 1): void {
  chosenActive = true;
  activateSlot(i);
}

function activateSlot(i: 0 | 1): void {
  setActiveSlotSignal(i);
  applyActiveRouting();
}

/**
 * Assign built-in instrument `id` to slot `slotIndex` and make that slot the active (MIDI/keys) slot —
 * picking a source is picking what you play. If the slot was in plugin mode, the plugin is unloaded and
 * the slot reverts to this instrument; a failed unload keeps the plugin and leaves the active slot where
 * it was. An instrument takes no input: a slot that was live stops (every source change ends GO LIVE).
 */
export function selectSynth(slotIndex: 0 | 1, id: string): void {
  chosenSource[slotIndex] = true;
  setSlotIds((prev) => withAt(prev, slotIndex, id)); // immediate UI highlight
  setSlotOff((prev) => withAt(prev, slotIndex, false));
  // Serialize the routing work on the SAME per-slot chain as selectPlugin/clearPlugin, so a synth
  // pick made while a plugin load is in flight isn't clobbered by that load's continuation: the load
  // completes, then this op clears it and reverts to the chosen synth (last action wins).
  void serializePick(slotIndex, async () => {
    if (slotPlugins()[slotIndex]) {
      await doClearPlugin(slotIndex); // plugin → synth: unload + revert to the now-selected synth id
      // A failed unload restores the (now silent) plugin: leave MIDI where it is rather than move it there.
      if (slotPlugins()[slotIndex]) return;
    } else {
      // No plugin to unload, but the rig recall may still remember one for this slot (picked before
      // the recall reached it): the synth is the slot's source now. An Off slot's input stops.
      forgetSlotPlugin(slotIndex);
      void disarmInputInternal(slotIndex);
    }
    writeSource(slotIndex, id); // saved once it is the slot's source (a failed unload kept the plugin)
    activateSlot(slotIndex);
  });
}

/**
 * Set slot `slotIndex` to Off. Its notes go nowhere (the `Off` target while it is the
 * active slot) and GO LIVE passes its own input dry, heard and recorded at its input level. A plugin in
 * it unloads (a failed unload keeps it); the slot's GO LIVE ends, as at every source change. The active
 * slot stays where it is: nothing plays here to move MIDI to (as an effect load leaves it).
 */
export function selectOff(slotIndex: 0 | 1): void {
  chosenSource[slotIndex] = true;
  setSlotOff((prev) => withAt(prev, slotIndex, true));
  void serializePick(slotIndex, async () => {
    if (slotPlugins()[slotIndex]) {
      await doClearPlugin(slotIndex); // routes an active slot to Off once the plugin is gone
      if (!slotPlugins()[slotIndex]) writeSource(slotIndex, 'off'); // a failed unload kept the plugin
      return;
    }
    writeSource(slotIndex, 'off');
    forgetSlotPlugin(slotIndex);
    void disarmInputInternal(slotIndex);
    if (activeSlot() === slotIndex) applyActiveRouting();
  });
}

/**
 * Editor affinity per plugin FILE. The same plugin file loaded in BOTH slots shares ONE OS module →
 * one GUI runtime (JUCE `MessageManager`), whose message thread binds to the owner thread of the
 * module's FIRST load and stays bound while the module is loaded (measured on Archetype VST3: B's
 * first editor open wedged though only A had loaded before it and A never opened one). An editor from
 * any other owner thread wedges inside `attached()`/`gui.create` forever → `open_editor` timeout →
 * frozen half-window → closing it kills the app. Mixed formats of the same plugin (CLAP + VST3 = two
 * modules) are fine.
 *
 * The rule (D29): a path's editor belongs to the slot whose LOAD of it came first. A load that starts
 * while no slot holds the path owns it (`claimEditorAffinity`); the other slot's EDITOR is refused with a
 * toast (`PluginControls.toggleEditor`), whether or not the owner ever opened one. Rig recall loads slot
 * A, then slot B, each landed before the next starts (`recallRig`), so A owns a file both remember. A
 * load that starts while the other slot's load of the same path is still in flight cannot be ordered
 * here (each runs on its own owner thread, and either may open the module first), so the entry turns
 * `unordered` and BOTH slots are refused. Every load runs on its own new owner thread
 * (`src-tauri/src/host/engine_slot.rs`), so the owning slot's unload ends the bound thread: while the
 * other slot keeps the module loaded, the entry turns `orphaned` and BOTH slots are refused, the owner's
 * next load included. The entry dies when the module truly unloads, i.e. when the LAST slot holding the
 * path lets it go; the next load owns it afresh. A path's holders (`pathHeld`) are its playing, loading
 * and retiring instances: a slot holds a path (`nativePaths`) from the moment its load STARTS (the host
 * opens the module before it answers) until its native unload succeeded or its load failed; a hot swap's
 * incoming file is held from the swap's start (`loadingPaths`), and the plugin it replaced until its
 * retire ended (`retiring`). Any native error on a file's load or unload keeps that file a possible holder until
 * restart (`uncertainPaths`): the host may have left its module loaded (a setup past its bound, a
 * teardown that leaked the plugin), and no native answer says when it goes. Once no slot holds such a
 * file, it is refused in BOTH slots until restart, and a later load claims nothing: its module may be
 * bound to an owner thread this map never saw (a WebView reload empties the map, and the startup
 * resync's failed unload may leave the module behind, `resyncNativeSlots`). Real fix (one shared GUI
 * thread for all editors) is a fragile host rework, deliberately deferred.
 */
const editorAffinity = new Map<string, { slot: 0 | 1; state: 'owned' | 'orphaned' | 'unordered' }>();
// The file each slot's native host may hold (the rule above). Not `slotPlugins`: that reads empty while
// a load or an unload is in flight and comes back on a failed unload.
const nativePaths: [string | null, string | null] = [null, null];
// The file each slot's native load or hot swap in flight (started, not yet answered) brings in: a load of
// the same path starting meanwhile in the other slot cannot be ordered against it.
const loadingPaths: [string | null, string | null] = [null, null];
// Files a native load or unload error left possibly held for the rest of the run (the rule above); apart
// from `nativePaths`, so the slot's next load does not overwrite them.
const uncertainPaths = new Set<string>();
// The instances a hot swap replaced, until their `plugin:retired` (instance → path): the outgoing
// plugin's module may stay loaded while its retire runs, so its file's affinity waits for the event.
const retiring = new Map<number, string>();

/** Whether any holder of `path` is left: a slot's playing or loading plugin, or a replaced one still
 * retiring (the rule above). */
const pathHeld = (path: string): boolean =>
  nativePaths.includes(path) || loadingPaths.includes(path) || [...retiring.values()].includes(path);

/**
 * Non-null = refuse. `owner` names whose editor this file's GUI serves: the OTHER slot, a load of a slot
 * that has since unloaded the file (orphaned), or a slot this module cannot name (unordered), whichever
 * slot asks; `owner: null` = no entry, but a load this map never saw may still hold the file until
 * restart (the rule above).
 */
export function editorAffinityBlocker(slot: 0 | 1, path: string): { owner: string | null } | null {
  const entry = editorAffinity.get(path);
  if (!entry) return uncertainPaths.has(path) ? { owner: null } : null;
  const owner = `slot ${entry.slot === 0 ? 'A' : 'B'}`;
  if (entry.state === 'orphaned') return { owner: `${owner}'s previous load` };
  if (entry.state === 'unordered') return { owner: 'an unknown slot (both loaded it at once)' };
  return entry.slot !== slot ? { owner } : null;
}

/**
 * As `slot`'s native load of `path` starts (before `nativePaths` records it): the first load of a path
 * no slot holds owns its editor; a load overlapping the other slot's load of it in flight makes the
 * entry unordered; any other entry stands. A file a native error left uncertain gets no entry.
 */
function claimEditorAffinity(slot: 0 | 1, path: string): void {
  const other = slot === 0 ? 1 : 0;
  const entry = editorAffinity.get(path);
  if (entry) {
    const otherLoading = loadingPaths[other] === path;
    if (entry.state === 'owned' && entry.slot === other && otherLoading) entry.state = 'unordered';
    return;
  }
  if (uncertainPaths.has(path)) return;
  editorAffinity.set(path, { slot, state: pathHeld(path) ? 'unordered' : 'owned' });
}

/**
 * After `slot` let go of `outgoingPath` (its unload succeeded, its load failed or its retire ended):
 * drop the path's affinity when no holder of it is left (`pathHeld`: the module unloaded, unless the
 * path is uncertain: then the uncertain rule refuses it); else, if `slot` owned it, its owner thread is
 * gone with the module still bound to it, so the entry turns orphaned.
 */
function releaseEditorAffinity(slot: 0 | 1, outgoingPath: string | undefined): void {
  if (!outgoingPath) return;
  const entry = editorAffinity.get(outgoingPath);
  if (!entry) return;
  if (!pathHeld(outgoingPath)) editorAffinity.delete(outgoingPath);
  else if (entry.slot === slot && entry.state === 'owned') entry.state = 'orphaned';
}

/**
 * Load native plugin `desc` into `slot`: over a plugin of another file, a hot swap (`swapsHot`); else
 * any plugin already in the slot is unloaded first (clean swap), `desc` loads into the running engine
 * through the host, and — if the slot is active — notes route to it. On load failure the slot is left on
 * its instrument. Serialized per slot, the last pick winning (see `serializePick`).
 */
export function selectPlugin(slot: 0 | 1, desc: PluginDescriptor): Promise<void> {
  if (!nativeHostReady()) return Promise.resolve();
  chosenSource[slot] = true;
  return serializePick(slot, () => doSelectPlugin(slot, desc, () => true));
}

/**
 * The rig recall's load (`rig-recall.ts`): `selectPlugin` into a slot the player has not chosen a
 * source for this launch, else nothing. Checked and enqueued in one synchronous step, so a pick made
 * while the other slot restores wins its slot. A restored instrument takes the MIDI slot only if the
 * player has chosen nothing by the time its load lands.
 */
export function restorePlugin(slot: 0 | 1, desc: PluginDescriptor): Promise<void> {
  if (!nativeHostReady() || chosenSource[slot]) return Promise.resolve();
  return serializeSlot(slot, () => doSelectPlugin(slot, desc, nothingChosen));
}

/**
 * A load failure the plugin did not cause, in the native host's words: its setup ran past the host's
 * bound or its owner thread could not start (`src-tauri/src/host/engine_slot.rs`), the command's task
 * failed (`src-tauri/src/host/commands.rs`), the slot was busy (a load or an unload still clearing it,
 * a page a reload replaced, a poisoned slot lock: `src-tauri/src/engine_io/plugins.rs`; a unit still
 * in the engine: `src-tauri/src/engine_io/slot_host.rs`), no device was open, or this page's host had
 * not started (`src/platform/host.tauri.ts`). Any other failure is the plugin's own (it is missing,
 * or would not load, instantiate or activate). Pinned by `pnpm probe rig-recall`.
 */
const TRANSIENT_LOAD_ERROR =
  /plugin setup timed out|spawn owner thread|plugin_load task|already loading|already has a plugin loaded|still clearing|superseded|stale frontend epoch|poisoned|already holds a unit|port is full|no audio device is open|plugin host not initialized/;

/** `toneToken`: a session import's reload token when this load is that reload (`reloadPlugin`). */
async function doSelectPlugin(
  slot: 0 | 1,
  desc: PluginDescriptor,
  claimMidi: () => boolean,
  toneToken?: number,
): Promise<void> {
  const outgoing = slotPlugins()[slot];
  if (samePluginDescriptor(outgoing, desc)) return; // already loaded
  if (outgoing && swapsHot(slot, desc)) return doSwapPlugin(slot, outgoing, desc, claimMidi, toneToken);
  // Swap: tear down + unload any existing plugin in this slot before loading the new one. A failed
  // unload aborts the swap — the native host may still hold the old plugin, so a load would only fail
  // on "slot already loaded" while the UI showed the new one.
  if (outgoing && !(await unloadSlotPlugin(slot, outgoing, 'swap'))) return;
  // An Off slot's live input stops before a plugin takes the slot (an effect's pick goes live again).
  if (!outgoing) void disarmInputInternal(slot);
  let info: PluginInfo | undefined;
  sourceOps[slot]++;
  claimEditorAffinity(slot, desc.path);
  nativePaths[slot] = desc.path;
  loadingPaths[slot] = desc.path;
  try {
    // A browser probe's stand-in host may answer nothing, or no instance (`slotInstance`).
    info = await platform.pluginHost.loadPlugin(slot, desc.path, desc.id, toneToken);
  } catch (e) {
    loadingPaths[slot] = null;
    swapping[slot] = false; // the swap ends on the slot's instrument (or Off)
    console.error('[instrument] plugin load failed', e);
    uncertainPaths.add(desc.path);
    nativePaths[slot] = null;
    releaseEditorAffinity(slot, desc.path);
    notifyError('Could not load the plugin. Pick it again, or another source.', e);
    // The slot holds nothing now, so the next launch must not retry this load (`rig-recall.ts`) —
    // unless the plugin is not at fault: the engine has no device (a load goes into a running engine),
    // or the host was busy with the slot. The rig must come back then.
    const engineDown = (await platform.engine.status().catch(() => null)) === null;
    if (!engineDown && !TRANSIENT_LOAD_ERROR.test(String(e))) forgetSlotPlugin(slot);
    if (activeSlot() === slot) applyActiveRouting(); // ensure we're back on the instrument
    return;
  }
  loadingPaths[slot] = null;
  swapping[slot] = false;
  batch(() => {
    setSlotPlugins((prev) => withAt(prev, slot, desc));
    setSlotInstances((prev) => withAt(prev, slot, info?.instance ?? null));
    setStrandedLive((prev) => withAt(prev, slot, false));
  });
  rememberSlotPlugin(slot, desc);
  noteToneFailed(info, desc);
  // The level this plugin last had in this slot, else the scan's kind's default.
  setEngineGain(slot, savedPluginLevel(slot, desc));
  // An instrument plugin makes its slot the MIDI/keys slot (a restored one only while `claimMidi` says
  // so); an effect (amp sim on the guitar) leaves routing where it is.
  if (desc.isEffect !== true && claimMidi()) activateSlot(slot);
  else if (activeSlot() === slot) applyActiveRouting();
}

/** The host restored the plugin's stored tone inside the load or swap; one it could not restore (the
 * host logged why) leaves the plugin at its defaults, and the player should know. */
function noteToneFailed(info: PluginInfo | undefined, desc: PluginDescriptor): void {
  if (info?.tone === 'failed') notifyError(`Could not restore the saved settings of ${desc.name}. Set it up again from its defaults.`);
}

/** Whether a pick of `desc` swaps `slot`'s plugin hot: a plugin plays there, with a known instance, and
 * `desc` is another file (native's `same_file`: case-insensitive, `/` as `\`). The same file under
 * another id, an empty slot or a reload goes cold. */
export function swapsHot(slot: 0 | 1, desc: PluginDescriptor): boolean {
  const playing = slotPlugins()[slot];
  const folded = (path: string) => path.toLowerCase().replaceAll('/', '\\');
  return !!playing && slotInstances()[slot] !== null && folded(playing.path) !== folded(desc.path);
}

/**
 * Swap `slot`'s playing plugin `outgoing` for `desc` in one native step (`plugin_swap`): the outgoing
 * plays until the commit, so the slot never reads empty, its notes never route `'Off'` and its live
 * input stays as it is (the engine ends a kind change's live itself; the feed's `live` shows it). The
 * outgoing instance is registered as retiring before the call, so its `plugin:retired` finds it in either
 * order against the answer (`watchRetiredPlugins`). On failure the slot's truth comes from the host
 * (`adoptSwapFailure`). A gain change meanwhile is held (`setEngineGain`) and sent only if the old plugin
 * plays on.
 */
async function doSwapPlugin(
  slot: 0 | 1,
  outgoing: PluginDescriptor,
  desc: PluginDescriptor,
  claimMidi: () => boolean,
  toneToken?: number,
): Promise<void> {
  setHotSwaps((prev) => withAt(prev, slot, true));
  swapInFlight[slot] = true;
  sourceOps[slot]++;
  claimEditorAffinity(slot, desc.path);
  loadingPaths[slot] = desc.path; // until the incoming's holder moved to `nativePaths` or was let go
  const outgoingInstance = slotInstance(slot);
  retiring.set(outgoingInstance, outgoing.path);
  const gain = savedPluginLevel(slot, desc);
  let oldPlaysOn = false;
  try {
    const info = await platform.pluginHost.swapPlugin(slot, desc.path, desc.id, gain, toneToken);
    adoptSwapped(slot, desc, info, gain, claimMidi, outgoing.path, outgoingInstance);
  } catch (e) {
    setHotSwaps((prev) => withAt(prev, slot, false));
    console.error('[instrument] plugin swap failed', e);
    oldPlaysOn = await adoptSwapFailure(slot, outgoing, outgoingInstance, desc, gain, claimMidi, e);
  } finally {
    setHotSwaps((prev) => withAt(prev, slot, false));
    swapInFlight[slot] = false;
    // The held change was the outgoing's (`setSlotLevel` stored it): it is owed only while that plays on.
    if (gainHeld[slot] && oldPlaysOn) sendEngine({ SetSlotGain: [slot, slotEngineGain(slot)] });
    gainHeld[slot] = false;
  }
}

/** The slot plays `desc` as instance `info.instance` now (a swap's answer, or the host's listing after a
 * failed one). Native applied `gain` at the commit, so nothing is sent. The incoming's holder moves from
 * `loadingPaths` to `nativePaths` in one step. The outgoing's affinity waits for
 * its `plugin:retired`; one that came before this answer found the slot still holding the path, so it is
 * released again here. */
function adoptSwapped(
  slot: 0 | 1,
  desc: PluginDescriptor,
  info: PluginInfo,
  gain: number,
  claimMidi: () => boolean,
  outgoingPath: string,
  outgoingInstance: number,
): void {
  batch(() => {
    setSlotPlugins((prev) => withAt(prev, slot, desc));
    setSlotInstances((prev) => withAt(prev, slot, info.instance));
    setStrandedLive((prev) => withAt(prev, slot, false));
  });
  nativePaths[slot] = desc.path;
  loadingPaths[slot] = null;
  if (!retiring.has(outgoingInstance)) releaseEditorAffinity(slot, outgoingPath);
  rememberSlotPlugin(slot, desc);
  noteToneFailed(info, desc);
  setEngineGain(slot, gain, false);
  // The kind the plugin activated as decides the MIDI claim (`doSelectPlugin`'s rule).
  if (!info.effect && claimMidi()) activateSlot(slot);
  else if (activeSlot() === slot) applyActiveRouting();
}

const SWAP_BUSY = 'Could not swap the plugin now. Pick it again.';

/**
 * A failed swap, read from the host's listing, never from the error's text (only its `busy:` prefix
 * picks the toast): the outgoing instance still listed plays on; another instance listed is adopted; a
 * slot listed empty is cleared as an unload is, its live input stopped first (a refused live-off keeps
 * its GO LIVE as the stop, `strandedLive`); a listing that fails keeps the outgoing shown and both
 * files uncertain. The incoming file stays held (`loadingPaths`) until its branch adopted it or let it
 * go, before any affinity release. Returns whether the old plugin plays on.
 */
async function adoptSwapFailure(
  slot: 0 | 1,
  outgoing: PluginDescriptor,
  outgoingInstance: number,
  desc: PluginDescriptor,
  gain: number,
  claimMidi: () => boolean,
  e: unknown,
): Promise<boolean> {
  const busy = isBusyRefusal(e);
  let listed: PluginInfo | undefined;
  try {
    listed = (await platform.pluginHost.listLoaded()).find((p) => p.slot === slot);
  } catch (listError) {
    console.error('[instrument] plugin swap failed, and listing the slots failed', listError);
    loadingPaths[slot] = null;
    uncertainPaths.add(outgoing.path);
    uncertainPaths.add(desc.path);
    notifyError(busy ? SWAP_BUSY : `Could not load ${desc.name}.`, e);
    return false;
  }
  if (listed?.instance === outgoingInstance) {
    retiring.delete(outgoingInstance); // nothing was released
    loadingPaths[slot] = null;
    uncertainPaths.add(desc.path);
    releaseEditorAffinity(slot, desc.path);
    notifyError(busy ? SWAP_BUSY : `Could not load ${desc.name}. ${outgoing.name} plays on.`, e);
    return true;
  }
  if (listed) {
    adoptSwapped(slot, listed.descriptor, listed, gain, claimMidi, outgoing.path, outgoingInstance);
    notifyError(busy ? SWAP_BUSY : `Could not load ${desc.name}.`, e);
    return false;
  }
  retiring.delete(outgoingInstance);
  const disarmed = await disarmInputInternal(slot); // an empty live slot passes its input dry
  batch(() => {
    setSlotPlugins((prev) => withAt(prev, slot, null));
    setSlotInstances((prev) => withAt(prev, slot, null));
    setStrandedLive((prev) => withAt(prev, slot, !disarmed));
  });
  setEngineGain(slot, null, false);
  slotLetGo(slot);
  loadingPaths[slot] = null;
  for (const path of [outgoing.path, desc.path]) {
    uncertainPaths.add(path);
    releaseEditorAffinity(slot, path);
  }
  notifyError(`Could not load ${desc.name}, and the slot is empty now. Pick a source again.`, e);
  if (activeSlot() === slot) applyActiveRouting(); // back to the slot's instrument (or Off)
  return false;
}

/**
 * Watch the host's `plugin:retired`, once from boot: an instance a hot swap replaced has ended its retire,
 * so its file's affinity is released (`retiring`); with no proof its plugin was released, the file stays
 * uncertain. An instance this page did not register is ignored, so either order against the swap's
 * answer works. Not filtered by `slotInstances`: the retired instance is never the slot's current one.
 */
export function watchRetiredPlugins(): () => void {
  return platform.pluginHost.onRetired((e) => {
    const path = retiring.get(e.instance);
    if (path === undefined) return;
    retiring.delete(e.instance);
    releaseEditorAffinity(e.slot, path);
    if (!e.released) uncertainPaths.add(path);
  });
}

/**
 * Reload the plugin in `slot` in place, through the normal unload and load, so the load applies the
 * tone a session import just stored (`slot-tones.ts`), if the slot still holds the plugin instance the
 * import found there: `expected`, loaded no later than source generation `since`
 * (`slotSourceGeneration`, read before the import asked the host). A slot the player moved to another
 * plugin or none meanwhile, or loaded again, is left alone (`moved`). The load passes `toneToken`, the
 * import's reload token, so it restores the tone the host parked for it. The unload ends the slot's GO
 * LIVE (an empty live slot would pass the input dry), so a slot that was live goes live again once its
 * plugin is back (the other slot's live state is its own); its output level is kept, and MIDI stays
 * where it was. One op on the slot's chain, so nothing the player does to the slot interleaves with it.
 */
export function reloadPlugin(
  slot: 0 | 1,
  expected: Pick<PluginDescriptor, 'format' | 'path' | 'id'>,
  since: number,
  toneToken: number,
): Promise<'reloaded' | 'moved' | 'failed'> {
  return serializeSlot(slot, async () => {
    const desc = slotPlugins()[slot];
    if (!desc || sourceOps[slot] !== since || !samePluginDescriptor(desc, expected)) return 'moved';
    const wasLive = inputArmed()[slot];
    const gain = pluginGain()[slot];
    holdEngineLive(slot, wasLive); // live meanwhile, to a GO LIVE pressed during the reload
    try {
      if (!(await unloadSlotPlugin(slot, desc, 'swap'))) return 'failed';
      await doSelectPlugin(slot, desc, () => false, toneToken);
      if (!samePluginDescriptor(slotPlugins()[slot], desc)) return 'failed';
      if (gain !== null) setPluginGain(slot, gain);
      if (wasLive) resumeEngineLive(slot);
      return 'reloaded';
    } finally {
      swapping[slot] = false; // the load ends the swap; this covers a throw before it
      holdEngineLive(slot, false);
    }
  });
}

/**
 * Natively unload the plugin in `slot`. The slot's live input stops first, and the unload waits until
 * that reached the host: a slot still live once its plugin is gone passes the input dry, so a live-off
 * the host did not take aborts the unload (the plugin stays, as at a failed unload). The slot reads
 * empty while the unload is in flight: a clear routes an active slot to its instrument at once, a swap
 * (`swapping`) leaves it silent until the next plugin's load settles, whatever routes it meanwhile (a
 * keypress's `ensureActive`, a slot pick). On failure the OLD descriptor is restored and routing follows
 * it: the host may still hold that plugin, so the slot keeps saying so and a later swap/clear retries
 * the unload. Returns whether it unloaded.
 */
async function unloadSlotPlugin(slot: 0 | 1, outgoing: PluginDescriptor, path: 'swap' | 'clear'): Promise<boolean> {
  if (!(await disarmInputInternal(slot))) {
    console.error('[instrument] plugin unload skipped: the live-off did not reach the engine');
    notifyError('Could not unload the plugin. Choose Off or another source to try again.', 'Its input could not be stopped first.');
    return false;
  }
  sourceOps[slot]++;
  // The slot reads empty at once, but the engine keeps the outgoing gain until the unload is done: the
  // plugin plays out its removal fade (and any tail) at the level the player set, not at unity.
  const outgoingGain = engineGains()[slot];
  const outgoingInstance = slotInstances()[slot];
  setEngineGain(slot, null, false);
  batch(() => {
    setSlotPlugins((prev) => withAt(prev, slot, null));
    setSlotInstances((prev) => withAt(prev, slot, null));
  });
  // A swap routes nowhere (Off) until the next plugin lands, which releases held notes: the same slot
  // target comes back, so routing there again would not release a note held across the swap.
  swapping[slot] = path === 'swap';
  if (activeSlot() === slot) applyActiveRouting(); // a clear: back to the slot's instrument immediately
  try {
    await platform.pluginHost.unloadPlugin(slot);
  } catch (e) {
    swapping[slot] = false;
    console.error(`[instrument] plugin unload${path === 'swap' ? ' (swap)' : ''} failed`, e);
    uncertainPaths.add(outgoing.path);
    notifyError('Could not unload the plugin. Choose Off or another source to try again.', 'It stays in the slot, silent.');
    batch(() => {
      setSlotPlugins((prev) => withAt(prev, slot, outgoing));
      setSlotInstances((prev) => withAt(prev, slot, outgoingInstance));
    });
    setEngineGain(slot, outgoingGain ?? savedPluginLevel(slot, outgoing));
    if (activeSlot() === slot) applyActiveRouting();
    return false;
  }
  slotLetGo(slot);
  releaseEditorAffinity(slot, outgoing.path);
  return true;
}

/** The slot's host holds no plugin now (an unload done, or a swap that failed empty): the engine's slot
 * gain goes back to the input level, and the rig recall and `nativePaths` forget the plugin. */
function slotLetGo(slot: 0 | 1): void {
  if (engineGains()[slot] === null) sendEngine({ SetSlotGain: [slot, inputGains()[slot]] });
  forgetSlotPlugin(slot);
  nativePaths[slot] = null;
}

/**
 * Unload the plugin in `slot` and revert it to what it held before (its instrument, or Off). Reverts
 * routing optimistically, then awaits the native unload; a failed unload restores the plugin (see
 * `unloadSlotPlugin`). No-op if the slot holds no plugin. Serialized per slot (see `serializeSlot`).
 */
export function clearPlugin(slot: 0 | 1): Promise<void> {
  chosenSource[slot] = true;
  return serializePick(slot, () => doClearPlugin(slot));
}

async function doClearPlugin(slot: 0 | 1): Promise<void> {
  const outgoing = slotPlugins()[slot];
  if (outgoing) await unloadSlotPlugin(slot, outgoing, 'clear');
}

/**
 * Resync the native host with this module's slot state at startup (frontend-reload wedge). A WebView
 * reload (or crash-recovery) resets this module to its defaults while the Rust host keeps its
 * plugins loaded → every later load fails ("slot N already has a plugin loaded") and the
 * editor/GO-LIVE chrome never renders, with a full app restart the only exit.
 *
 * We query the host for the slots it still holds and UNLOAD each through the PRODUCTION unload path
 * (`platform.pluginHost.unloadPlugin`), exactly as a normal unload does. We do NOT route through
 * `doClearPlugin`: this module already thinks the slot is empty, so that path would early-return and
 * leave the native slot loaded. Adopting the loaded plugin instead (editor affinity + UI state) is
 * future work — unload-to-clean is the correct v1.
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
    notifyError('Could not check which plugins the host holds. Restart BleepLoop if a slot misbehaves.', e);
    return;
  }
  for (const { slot, descriptor, instance } of loaded) {
    // Log as an error so it reaches the release log (the codebase's log channel) — recovering a
    // stranded native slot is an abnormal-state event worth a field breadcrumb, not routine. The slot is
    // unloaded, not adopted, so its instance stays unset (`slotInstances`); the breadcrumb names it.
    console.error(`[instrument] resync: unloading stranded plugin in slot ${slot} (${descriptor.name}, instance ${instance})`);
    // Serialize on the slot's op chain so the unload can't interleave with any concurrent slot op.
    await serializeSlot(slot, async () => {
      try {
        await platform.pluginHost.unloadPlugin(slot);
      } catch (e) {
        console.error('[instrument] resync: stranded-slot unload failed', e);
        uncertainPaths.add(descriptor.path);
        notifyError('Could not clear a plugin left in a slot. Restart BleepLoop.', e);
      }
    });
  }
}

/**
 * Scan installed native plugins into `availablePlugins` (no-op / empty in the browser build).
 * One scan runs at a time. A request made while one is in flight is remembered, and ONE more scan
 * runs when that one ends (forced if any of the waiting requests was): what the running scan walks
 * was fixed when it started, so a folder added meanwhile (`plugin-folders.ts`) would otherwise stay
 * unscanned, and the picker ends on the scan that started after the last request. Such a request
 * resolves when that follow-up is done; `scanning` stays up in between. Only
 * `availablePlugins` is refreshed; loaded slots (`slotPlugins`), routing and audio are untouched, so a
 * rescan never disturbs playback. A plugin still loaded in a slot but MISSING from the fresh scan
 * (e.g. its `--scan-one` child hit the 20s timeout this pass) is merged back into the list, so its
 * picker button + active highlight survive the rescan instead of vanishing while it's still playing.
 */
export async function scanForPlugins(opts: { force?: boolean } = {}): Promise<void> {
  if (!platform.pluginHost.available) return;
  if (!nativeHostReady()) return;
  if (scanRun) {
    scanAgain = { force: (scanAgain?.force ?? false) || (opts.force ?? false) };
    return scanRun;
  }
  setScanning(true);
  scanRun = (async () => {
    try {
      for (let next: { force: boolean } | null = { force: opts.force ?? false }; next; next = takeScanAgain()) {
        try {
          const scanned = await platform.pluginHost.scanPlugins(next.force);
          setAvailablePlugins(reconcilePluginDescriptors(scanned, slotPlugins()));
        } catch (e) {
          console.error('[instrument] plugin scan failed', e);
          notifyError('Could not scan for plugins. Press rescan in the command bar.', e);
        }
      }
    } finally {
      // In the same step that found nothing waiting, so no request can land on a finished run.
      scanRun = null;
      setScanning(false);
    }
  })();
  return scanRun;
}

/** The scan asked for while one ran, taken (once). */
function takeScanAgain(): { force: boolean } | null {
  const again = scanAgain;
  scanAgain = null;
  return again;
}

// ---------------------------------------------------------------------------
// Plugin output gain — per-slot wet level, type-aware default + UI trim
// ---------------------------------------------------------------------------

/**
 * Set the output gain (0..1.5) of slot `slot`'s loaded plugin (the plugin panel's output slider, or
 * `__lf.setPluginGain`): the engine's slot gain. No-op if no plugin is loaded there.
 */
export function setPluginGain(slot: 0 | 1, value: number): void {
  if (engineGains()[slot] !== null) setEngineGain(slot, Math.max(0, value));
}

/** Reactive output gain for both slots; null means no plugin is loaded there. */
export const pluginGain = (): readonly [number | null, number | null] => engineGains();

/**
 * Slot `slot`'s level, the one volume its header shows (0..SLOT_GAIN_MAX): its plugin's output gain
 * while one is loaded (null until it is set), else an Off slot's input level or its instrument's level
 * (kept per instrument, so two slots holding one instrument share it).
 */
export function slotLevel(slot: 0 | 1): number | null {
  if (slotPlugins()[slot]) return pluginGain()[slot];
  return slotOff()[slot] ? inputGains()[slot] : (synthGains()[slotIds()[slot]] ?? 1);
}

/** Set slot `slot`'s level (`slotLevel`), heard and recorded; an Off slot's and a synth's are kept
 * for the next launch. */
export function setSlotLevel(slot: 0 | 1, value: number): void {
  const v = Math.max(0, Math.min(SLOT_GAIN_MAX, value));
  const plugin = slotPlugins()[slot];
  if (plugin) {
    setPluginGain(slot, v);
    writeStoredNumber(pluginGainKey(slot, plugin), v);
    return;
  }
  if (slotOff()[slot]) {
    setInputGains((prev) => withAt(prev, slot, v));
    writeStoredNumber(inputGainKey(slot), v);
    sendEngine({ SetSlotGain: [slot, v] });
    return;
  }
  const id = slotIds()[slot];
  setSynthGains((prev) => ({ ...prev, [id]: v }));
  writeStoredNumber(synthGainKey(id), v);
  sendEngine({ SetInstrumentGain: [id as InstrumentId, v] });
}

/** Whether slot `slot`'s source takes its input, so the slot offers GO LIVE: a plugin that is not a
 * known instrument (which takes none), or Off. */
export function slotTakesInput(slot: 0 | 1): boolean {
  const plugin = slotPlugins()[slot];
  if (plugin) return plugin.isEffect !== false;
  return slotOff()[slot];
}

/** Read-only reactive accessors: the instrument id per slot, whether a slot is Off, the loaded plugin per
 * slot (null = instrument or Off) and its native instance, the active slot index. (Owned by
 * `instrument-slots.ts`; re-exported as the UI's one path.) */
export { slotIds, slotInstances, slotOff, slotPendingCounts, slotPlugins, activeSlot };
export { isBusyRefusal, isStaleInstance, slotInstance } from './instrument-slots';

/**
 * Derived predicate: is the ACTIVE slot playing the built-in GM drum kit (synth id 'drum' AND not
 * overridden by a loaded plugin — a plugin always plays chromatically, never the pad grid)? The ONE
 * source for the "keyboard is a pad grid" decision, read by app.tsx (chrome noun + the digit-select
 * yield) and Keyboard.tsx (pad-vs-piano layout) so the two can never disagree.
 */
export function activeIsDrum(): boolean {
  const i = activeSlot();
  return !slotPlugins()[i] && !slotOff()[i] && slotIds()[i] === 'drum';
}

/** Read-only reactive accessors: scanned plugins + scan-in-progress flag (the picker). */
export { availablePlugins, scanning };
export { nativeHostReady };
