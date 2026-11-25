/**
 * THE CAPABILITY BOUNDARY.
 *
 * `src/platform/` is the ONLY place in the app permitted to import `@tauri-apps/*`
 * (enforced by `verify/guards/boundary.mjs`). Everything in `ui/`, `session/` and `app/` depends
 * on these interfaces, never on Tauri directly — so the entire frontend builds and renders
 * standalone in a browser via `pnpm dev` (silent: the browser build has no engine).
 *
 * Every capability differs per platform (web: a stub or a probe's scriptable fake; tauri: invoke/listen):
 *   - PluginHost   — native CLAP, VST3 and VST2 hosting.
 *   - EngineHost   — the native audio engine (`docs/ARCHITECTURE.md`).
 *   - ChainHost    — a slot's chain of units (`docs/unit-contract.md` §2): add, remove, move, reload, by unit.
 *                    Only the browser fake speaks it yet; the Tauri and production hosts refuse every call.
 *   - RigHost      — chain mode's rig record, which the native host owns: the page's one handover. Only
 *                    the DEV browser fake (`__lfRigFake`) and, natively, a DEV build with `lf.chainNative`
 *                    speak it; the native side's commands land with the record's writer.
 *   - MidiHost     — native MIDI (`src-tauri/src/engine_io/midi`): its ports, MIDI learn and the stored
 *                    bindings, and what it tells the UI. The app never opens Web MIDI (the WebView's
 *                    permission is denied, and `verify/guards/web-midi.mjs` keeps the Web MIDI API out
 *                    of `src/`): WinMM input ports are exclusive, and native MIDI keeps them.
 *   - InputHost    — the UI's one ordered path to the engine's commands and native MIDI's one note
 *                    router (`input_send`), which `index.ts`'s outbox alone calls.
 *   - ExportFiles  — where a session export's .zip lands (tauri: written natively; web: the rig's download).
 *   - LogFolder    — the release log's folder, for Help's diagnostics (web: none).
 *   - AppUpdates   — the app updater, release builds only (web: none, or a probe's script).
 *
 * Live audio NEVER crosses this boundary as PCM; a session save's snapshot does, once, off the RT path
 * (`EngineHost.snapshot` / `loadSession`).
 */
import type { DeviceRequest, DeviceStatus, FeedFrame, InputPick, PluginRef, UnitId, UnitSpec } from './engine-wire';
import type { Dropped, ImportReport, InputItem, MidiActionId, MidiEvent } from './midi-wire';

export type PluginSlot = 0 | 1;
export type PluginFormat = 'clap' | 'vst3' | 'vst2';

export interface PluginDescriptor {
  id: string;
  name: string;
  format: PluginFormat;
  path: string;
  /** Output-gain kind: `true` = audio effect (amp-sim/FX), `false` = instrument (synth), `null` =
   * unclassified. Read at scan from CLAP `features()` / VST3 `subCategories` / VST2's synth flag. The plugin bridge uses
   * it to pick the per-slot output-gain default (falling back to the input-bus count when null). */
  isEffect: boolean | null;
}

export interface PluginInfo {
  slot: PluginSlot;
  descriptor: PluginDescriptor;
  /** The load's answer: what it did with the plugin's stored tone (`src-tauri/src/host/tone.rs`).
   * Absent: nothing was stored. */
  tone?: 'restored' | 'failed';
}

/** A session import's tone, stored by the native host (`PluginHost.importTone`): the plugin it
 * belongs to, and whether the slot holds that plugin now. */
export interface ToneImport {
  /** Non-null: the slot holds that plugin, and its load stopped saving over the stored tone. The
   * imported tone is parked for the reload that passes this token to its `loadPlugin`; a reload that
   * does not happen hands it back (`forgetTone`). */
  reloadToken: number | null;
  name: string;
  format: PluginFormat;
  path: string;
  id: string;
}

/** One plugin parameter's metadata. `id` is the stable CLAP param id `setParameter` takes. */
export interface PluginParamDesc {
  id: number;
  name: string;
  minValue: number;
  maxValue: number;
  defaultValue: number;
  /** The LIVE value at enumeration time (same units as min/max) — what a freshly mounted drawer shows. */
  value: number;
}

/** A native audio capture device — what the plugin-input device picker lists. */
export interface AudioInputDevice {
  id: string;
  name: string;
  channels: number;
}

/** A native audio OUTPUT device — what the low-latency monitor device picker lists. */
export interface AudioOutputDevice {
  id: string;
  name: string;
  channels: number;
}

/** One folder the plugin scan walks, and whether it is there now (a missing one is skipped). */
export interface PluginFolder {
  path: string;
  exists: boolean;
}

/** A plugin file the last scan found and this build cannot host (a 32-bit VST2, a shell): its full
 * path and why, as the native host words it. */
export interface UnsupportedPlugin {
  path: string;
  reason: string;
}

/** A plugin bundle whose last scan failed (a crash, a timeout, output that did not parse): its full path
 * and a short category in the host's words ("timed out after 20 s"), never the plugin's own output. A
 * forced `scanPlugins` retries it. */
export interface FailedPlugin {
  path: string;
  reason: string;
}

/** The plugin scan's folders (`src-tauri/src/host/folders.rs`): `builtin` is what the scan walks by
 * itself (the standard CLAP, VST3 and VST2 folders, then `CLAP_PATH`/`VST3_PATH`), read-only; `user` is
 * the player's own list, kept in `plugin-folders.json`; `unsupported` and `failed` are what the last scan
 * in this run found and did not list: plugins it cannot host, and bundles whose scan failed. */
export interface PluginFolders {
  builtin: PluginFolder[];
  user: PluginFolder[];
  unsupported: UnsupportedPlugin[];
  failed: FailedPlugin[];
}

export interface PluginHost {
  /** False in the browser build — UI then surfaces the six built-in synths in both slots. */
  readonly available: boolean;
  /**
   * Begin this document's session with the native host, which answers the `frontendEpoch` every
   * slot-changing request carries. Call once per document, before the first `loadPlugin`. No rate
   * crosses here: each plugin's owner activates it at the engine's running rate (`SlotHost::rate`,
   * `src-tauri/src/engine_io/slot_host.rs`), and again when the engine is rebuilt at another rate
   * (`SlotHost::take_evicted`). No-op in the web build.
   */
  init(): Promise<void>;
  /**
   * List installed plugins. The native side remembers each bundle's result under a size + mtime
   * fingerprint, so a launch spawns no scan child for unchanged plugins; `force` (the picker's
   * rescan button) scans everything again, which is also how a previously failed bundle is retried.
   */
  scanPlugins(force?: boolean): Promise<PluginDescriptor[]>;
  /**
   * The plugins in the bundles at `paths` alone (rig recall's saved plugins, at launch), found and
   * cached as `scanPlugins` finds them: an unchanged bundle spawns no scan child, and a path the scan's
   * folders do not hold yields nothing. Quick where the full scan waits on every new plugin's child.
   */
  scanPluginPaths(paths: readonly string[]): Promise<PluginDescriptor[]>;
  /** The folders the scan walks. Rejects when the stored list cannot be read (the host leaves that
   * file as it is). Empty in the web build. */
  pluginFolders(): Promise<PluginFolders>;
  /**
   * Add a folder to the player's own. The native host opens its folder dialog itself and takes the
   * path from it: none is passed from here. Resolves with the new lists, or null when the player
   * cancels (and in the web build). The next `scanPlugins` walks it.
   */
  addPluginFolder(): Promise<PluginFolders | null>;
  /** Remove one of the player's folders, spelled as `pluginFolders` listed it; the new lists. */
  removePluginFolder(path: string): Promise<PluginFolders>;
  /**
   * `id` is required, not optional: a single `.clap`/`.vst3` bundle can export multiple plugin
   * descriptors, so `(slot, path)` alone would silently load `descriptor[0]`. Pass the
   * `PluginDescriptor.id` from `scanPlugins()` to pick the exact one. `toneToken`: the session import's
   * reload token (`ToneImport.reloadToken`) when this load is that reload.
   */
  loadPlugin(slot: PluginSlot, path: string, id: string, toneToken?: number): Promise<PluginInfo>;
  unloadPlugin(slot: PluginSlot): Promise<void>;
  /**
   * List the plugins currently loaded in the native slots (frontend-reload wedge resync). A WebView
   * reload resets the frontend's slot state to synth defaults while the Rust host keeps its plugins
   * loaded; the frontend queries this at init to find + unload the strays (else the next load hits
   * "slot N already has a plugin loaded" and the editor/GO-LIVE chrome never shows). Empty in the web
   * build.
   */
  listLoaded(): Promise<PluginInfo[]>;
  openEditor(slot: PluginSlot): Promise<void>;
  closeEditor(slot: PluginSlot): Promise<void>;
  setParameter(slot: PluginSlot, paramId: number, value: number): Promise<void>;
  /** Enumerate the loaded plugin's parameters (stable ids + ranges). Empty in the web build. */
  listParams(slot: PluginSlot): Promise<PluginParamDesc[]>;
  /**
   * Subscribe to param changes ORIGINATED BY THE PLUGIN'S OWN EDITOR — a knob drag in the
   * hosted GUI. Fires `{slot, id, value}` where `id` is the same stable param id `setParameter`/
   * `listParams` use and `value` is the new normalised 0..1 value. Returns an unsubscribe fn. No-op
   * (returns a no-op unsubscribe) in the web build.
   */
  onParamChanged(cb: (e: { slot: PluginSlot; id: number; value: number }) => void): () => void;
  /**
   * Subscribe to a WHOLESALE parameter change reported by the plugin itself — a preset loaded in its
   * own GUI, a program change (CLAP `params.rescan`, VST3 `restartComponent(kParamValuesChanged)`, VST2 `audioMasterUpdateDisplay`).
   * The values the UI holds are stale after this; re-run `listParams`. Returns an unsubscribe fn. No-op
   * in the web build.
   */
  onParamsChanged(cb: (slot: PluginSlot) => void): () => void;
  /**
   * Subscribe to editor-closed events emitted when the user closes a hosted/floating editor (its own
   * close box), so the UI can re-sync an "editor open" toggle. Returns an unsubscribe fn. No-op in the
   * web build.
   */
  onEditorClosed(cb: (slot: PluginSlot) => void): () => void;
  /**
   * Tone recall (`src-tauri/src/host/tone.rs`): every load restores the plugin's
   * stored tone by itself (`PluginInfo.tone`). `takeTone` saves the slot's tone now, through its
   * owner (the store gets it as from any save), and hands back the tone file's bytes (a session
   * export's), or null when the plugin keeps no state. `importTone` stores a session's tone under
   * `plugin`, the plugin session.json names for it (the host refuses a tone file of any other plugin),
   * and says whether `slot` holds that plugin now; it loads and swaps nothing. `forgetTone` drops the
   * tone an import parked under `reloadToken` for a reload that did not happen. All three reject in the
   * browser build.
   */
  takeTone(slot: PluginSlot): Promise<Uint8Array | null>;
  importTone(
    slot: PluginSlot,
    bytes: Uint8Array,
    plugin: Pick<PluginDescriptor, 'format' | 'path' | 'id'>,
  ): Promise<ToneImport>;
  forgetTone(slot: PluginSlot, reloadToken: number): Promise<void>;

  // ── Devices: the engine opens one input and one output (`EngineHost.open`); these list them ─────────
  /** Enumerate native capture devices. Empty in the web build. */
  listInputDevices(): Promise<AudioInputDevice[]>;
  /** Enumerate native output devices (the output and Share output pickers). Empty in the web build. */
  listOutputDevices(): Promise<AudioOutputDevice[]>;

  // ── ASIO low-latency tier ───────────────────────────────────────────────────────────────────────
  /** The startup coordinator's status (`src-tauri/src/asio_startup.rs`). Never touches the driver. */
  asioStatus(): Promise<AsioStatusReport>;
  /**
   * Request the ASIO driver probe (`src-tauri/src/asio_startup.rs`). The frontend calls this AFTER the
   * window is up, at boot only when the saved preference is on (`explicit=false`), and from the Audio
   * Settings toggle / Retry (`explicit=true`, which may proceed past a blocked or failed earlier attempt).
   * `driver` is the saved driver pick ('' = automatic; a name no longer installed falls back to it).
   * Resolves with the resulting status; `ready` means an ASIO device is available. Bounded by the
   * native probe deadline (a hung driver yields `timed-out`, never a hang here).
   */
  asioProbe(explicit: boolean, driver: string): Promise<AsioStatusReport>;
  /**
   * Switch to another ASIO driver without a restart ('' = automatic): the host drops the cached driver
   * and probes `driver` in its place, as `asioProbe(true, driver)` would. The device owner runs it: a
   * device on ASIO closes first and opens again after, on the new driver (unless that one runs at
   * another rate while the engine holds loops: the next `open` asks). A timed-out probe answers
   * `timed-out` and needs a restart, as at startup.
   */
  asioSwitch(driver: string): Promise<AsioStatusReport>;
  /** The installed ASIO drivers' names, read from the registry without loading any. Empty without ASIO. */
  asioDrivers(): Promise<string[]>;
  /** The cached ASIO driver, its actual channel counts, buffer range and rates; null without an ASIO device. */
  asioDeviceInfo(): Promise<AsioDeviceInfo | null>;
}

/**
 * ASIO startup status (mirrors `AsioStartupStatus` in `src-tauri/src/asio_startup.rs`). `not-compiled`
 * = no ASIO in this binary (and the web build); `disabled-by-flag` = launched with `--disable-asio`;
 * `unprobed` = nothing asked yet (saved preference off); `probing`; `ready`; `failed` (explicitly
 * retryable); `blocked` (an earlier launch's probe never completed, explicit retry needed);
 * `timed-out` (restart required).
 */
export type AsioStartupStatus =
  | 'not-compiled'
  | 'disabled-by-flag'
  | 'unprobed'
  | 'probing'
  | 'ready'
  | 'failed'
  | 'blocked'
  | 'timed-out';
export interface AsioStatusReport {
  status: AsioStartupStatus;
  /** Human-readable reason for `failed` / `blocked` / `timed-out`; empty otherwise. */
  detail: string;
}

/** The cached ASIO driver (`plugin_asio_device_info`). */
export interface AsioDeviceInfo {
  name: string;
  inputChannels: number;
  outputChannels: number;
  /** The buffer sizes the driver takes, in frames; null when it did not say. Equal: one size only, set
   * in the driver's own control panel. */
  bufferMin: number | null;
  bufferMax: number | null;
  /** The pickable rates (44100, 48000) the driver runs, read by the startup probe. The native host
   * always sends it; a scripted probe may leave it out (read as none). */
  sampleRates?: number[];
}

/**
 * Native MIDI (`src-tauri/src/engine_io/midi/mod.rs`, `MidiHost`): the input ports, MIDI learn, the stored
 * bindings and the note router for every source. The UI keeps the learn row, the device list and the toasts,
 * driven by the events (`midi-wire.ts`) on their own channel, never the feed. Each call rejects with the
 * native error (an index no binding has, a refused edit, no engine this launch). Every call but the
 * subscribe presents the page's input epoch: one of a page that is not the current one changes nothing and
 * answers false (null for the import), so a replaced page's late call never acts.
 */
export interface MidiHost {
  /** Subscribe to native MIDI's events, first thing in the document's boot; the first events are the
   * resync. The page names its age (`performance.timeOrigin`, the Tauri side reads it), so natively a
   * newer page's subscribe always wins over an older page's late one. `epoch` resolves with this page's
   * input epoch: natively the subscribe released the older pages' holds, cancelled a pending learn and
   * replaced the last subscriber (one at a time). It rejects when native MIDI does not run. Native MIDI
   * starts once per launch and counts its subscribes from 1 (`engine_io/midi/mod.rs` `Core::subscribe`), so an
   * epoch of 1 is the launch's first page and a larger one a WebView reload: the chain rig's crash marker
   * reads it (`src/ui/state/rig-recall.ts`). */
  subscribe(onEvent: (event: MidiEvent) => void): MidiSubscription;
  /** Learn the next CC or note-on, from any port, onto `action` (a lane action on track `target`, null the
   * selected track). Answered by `learning` and, once it captures, `learned`. */
  learn(epoch: number, action: MidiActionId, target: number | null): Promise<boolean>;
  /** Stop listening (a learned pedal's wait for its release goes on). True when a learn was pending. */
  cancelLearn(epoch: number): Promise<boolean>;
  /** Drop listed binding `index` of the list that came with store revision `revision` (a `bindings`
   * event's). False, and nothing done, when the list changed since: the fresh list is on its way (the same
   * for every edit below). */
  forget(epoch: number, revision: number, index: number): Promise<boolean>;
  /** Read listed binding `index`'s pedal as momentary (true) or latching. */
  setMomentary(epoch: number, revision: number, index: number, momentary: boolean): Promise<boolean>;
  /** HOLD on or off for listed binding `index` (a momentary REC/DUB pedal only). */
  setHold(epoch: number, revision: number, index: number, hold: boolean): Promise<boolean>;
  /** Assign listed binding `index` to the present port `portId` (`MidiPort.id`). */
  assign(epoch: number, revision: number, index: number, portId: string): Promise<boolean>;
  /** Import the web's bindings once (`lf.midiLearn` verbatim, `"[]"` when absent); later calls change
   * nothing and answer `already`. */
  importLegacy(epoch: number, json: string): Promise<ImportReport | null>;
}

/** A subscription to native MIDI's events: the document's input epoch, and the stop. */
export interface MidiSubscription {
  epoch: Promise<number>;
  stop(): void;
}

/**
 * The UI's one ordered path into native code (`input_send`): the engine's commands and the note sources'
 * input events (the on-screen and PC keyboard's notes by their physical owner, the window's blur, the note
 * target, the panic), in the order they were queued, for the document of `epoch` (its subscribe's: an
 * input event of a replaced document is refused natively). Send through `index.ts` (`sendEngine`, `input`),
 * whose outbox sends one batch at a time, the next once this one settled, so the order holds.
 */
export interface InputHost {
  /** One batch, run in order. Resolves with what native MIDI dropped of it (no device, a rebuild, no room;
   * the rest ran), or null. A rejection means none of it ran. */
  send(epoch: number, items: readonly InputItem[]): Promise<Dropped | null>;
}

/**
 * The native audio engine (`src-tauri/src/engine_io`): one device, the looper, synths, FX, mixer and
 * plugin slots in the audio callback. The UI sends commands (through `InputHost`, in one order with its
 * notes) and reads the feed; no PCM crosses. The payloads are `engine-wire.ts`.
 */
export interface EngineHost {
  /** False in the browser build (unless a DEV probe forces the web fake on, `host.web.ts`). */
  readonly available: boolean;
  /** Open the device, or switch to another; resolves with the device that runs. Rejects with the wire's
   * `OpenError` (`decodeOpenError`): a switch to another rate while the engine holds audio is refused
   * unless `force` (the player confirmed dropping the loops from the engine). */
  open(request: DeviceRequest, force?: boolean): Promise<DeviceStatus>;
  close(): Promise<void>;
  /** The device that runs, or null. */
  status(): Promise<DeviceStatus | null>;
  /** A plugin slot's input pick (unit contract §7): auto (null: input 2 on a device with two or more), one
   * channel (mono) or a `[left, right]` pair, 0-based; switched without reopening the device, and the
   * running device keeps it for its own reopens. Rejects for a pick the device lacks, either side of a
   * pair (nothing changes), and while no device runs. */
  setSlotInputChannel(slot: number, pick: InputPick): Promise<void>;
  /** Share output's endpoint (a WASAPI render id, as `listOutputDevices` lists them), or null for off.
   * The mirror runs while the device is ASIO. */
  setShare(endpoint: string | null): Promise<void>;
  /** The committed lanes' PCM and what they are (`engine-wire.ts` § Session bytes); with `master` (an
   * export's), the engine's wet master rendered from them too, or the render's error. */
  snapshot(master: boolean): Promise<ArrayBuffer>;
  /** Load a session into an all-empty engine (`engine-wire.ts` § Session bytes). */
  loadSession(bytes: Uint8Array<ArrayBuffer>): Promise<void>;
  /** Subscribe to the feed (~60 frames/s while something changes); the first frame has `reset`.
   * Returns the unsubscribe. */
  subscribe(onFrame: (frame: FeedFrame) => void): () => void;
}

/**
 * A slot's chain of units (`docs/unit-contract.md` §2, §3): structure is the host's, on its own path, one
 * call at a time per slot, never items of the `input_send` batch. Each change is answered on the feed's
 * `chains` (the UI draws the chain from that alone, invariant 3); a call settles on the engine's answer.
 * Bypass and built-in params ride the batch (`SetUnitBypass`, `ToggleUnit`, `Toggle.Units`, `SetUnitParam`).
 * A refusal rejects with the reason. Under Tauri the native chain host answers only in a DEV build with the
 * flag `lf.chainNative` on (`host.tauri.ts`); otherwise, and in a production browser build, every call
 * rejects (`available` false); the DEV browser fake answers it.
 */
export interface ChainHost {
  /** False until the native engine speaks the chain; true on the DEV browser fake when a probe asks for the chain (`__lfChainFake`, with the engine fake on).
   * Natively true only in a DEV build with `lf.chainNative`; for players, only with the activation prerequisites in `src/ui/AGENTS.md` § Open threads. */
  readonly available: boolean;
  /** Reserve `unit` across both slots, build it, hand it over at index `at` (a source goes to the head,
   * replacing a seated one); settles on the engine's answer: `seated`, or `waiting` while a take holds
   * the alignment. Rejects at once for a reserved id or a full chain; rejects on a failed load. */
  add(slot: PluginSlot, unit: UnitId, spec: UnitSpec, at: number, toneToken?: number): Promise<'seated' | 'waiting'>;
  remove(unit: UnitId): Promise<void>;
  /** A fresh instance under the same id, place and bypass, if `unit` is still `plugin` and was not re-added
   * since `since` (a `chains` frame's `seq`); the load applies `toneToken`. */
  reload(unit: UnitId, plugin: PluginRef, since: number, toneToken: number): Promise<'reloaded' | 'moved' | 'failed'>;
  /** `to`: its index in its own chain; no move across slots. */
  move(unit: UnitId, to: number): Promise<void>;
  openEditor(unit: UnitId): Promise<void>;
  closeEditor(unit: UnitId): Promise<void>;
  /** The player closed a unit's editor window by its own close, or the window went with its unit (unit
   * contract §2: `plugin:editor-closed` carries `unit`). Returns the unsubscribe. */
  onEditorClosed(cb: (unit: UnitId) => void): () => void;
  listParams(unit: UnitId): Promise<PluginParamDesc[]>;
  setParameter(unit: UnitId, paramId: number, value: number): Promise<void>;
  takeTone(unit: UnitId): Promise<Uint8Array | null>;
  /** Park a session's tone, identity-checked as today; `matches`: the units it may go to (unit contract §8). */
  importTone(slot: PluginSlot, bytes: Uint8Array, plugin: PluginRef, unit?: UnitId): Promise<{ token: number | null; name: string; matches: UnitId[] }>;
  forgetTone(token: number): Promise<void>;
}

/**
 * What the native rig record is for this process (`src-tauri/crates/lf-rig/src/answer.rs` `RigStatus`):
 * `present` (read, or stored by this import), `absent` (no record file: run the upgrade and import it),
 * `unreadable` (kept, never written over; nothing restored), `unavailable` (no data folder, or another
 * process owns the record).
 */
export type RigStatus = 'present' | 'absent' | 'unreadable' | 'unavailable';

/** What the restore forgot or held back; every `open` answers it until `seen(id)`. */
export interface RigNotice {
  id: number;
  /** The display names of the plugin units the crash marker named, now forgotten. */
  forgotten: string[];
  /** The crash marker could not be read: no plugin was restored this launch (built-ins were). */
  pluginsHeld: boolean;
}

/** `rig_open`'s and `rig_import`'s answer (`answer.rs` `RigAnswer`). */
export interface RigAnswer {
  status: RigStatus;
  /** Each slot's volume for the fader, A then B (the player's level, else the record's); only with `present`. */
  volumes: [number, number] | null;
  notice: RigNotice | null;
}

/**
 * Chain mode's rig record, owned by the native host (`src-tauri/crates/lf-rig`): the page never writes it,
 * and hands over the legacy keys' upgrade once. Natively only in a DEV build with `lf.chainNative`
 * (the `rig_*` commands are DEV-only until chains reach players); otherwise every call rejects
 * (`available` false). A chain-mode page's boot is the one caller (`src/ui/state/rig-recall.ts` `openRig`).
 */
export interface RigHost {
  readonly available: boolean;
  /** `rig_open`: called before the device opens; the process's first call starts the restore, a later
   * one (a reload) answers the same and starts nothing. */
  open(): Promise<RigAnswer>;
  /** `rig_import` with this document's epoch: `text` is `serializeRig(upgradeRig(…))`, stored only while no
   * record exists (else it answers `present`). Rejects with `'stale'` for a replaced page, or with the
   * reason the record could not be written (then nothing is restored). */
  import(text: string): Promise<RigAnswer>;
  /** `rig_seen`: the page showed notice `id`; native clears it. */
  seen(id: number): Promise<void>;
}

/** A session export's one .zip, handed over for keeping. */
export interface ExportFiles {
  /**
   * Natively, write the archive into the player's Downloads folder (`src-tauri/src/export_save.rs`) under
   * `filename` or the next free name; it resolves with the full path once the file is on disk and rejects
   * when it could not be written. The browser rig hands it to an `<a download>` and resolves with `filename`.
   */
  save(bytes: Uint8Array<ArrayBuffer>, filename: string): Promise<string>;
}

/**
 * The release log's folder (tauri-plugin-log's rotated file, `src-tauri/src/lib.rs`). Help's "About
 * this build" names it in the copied diagnostics and opens it, so a tester can attach the log to a
 * report.
 */
export interface LogFolder {
  /** False in the browser build: it writes no log file, and Help hides Open log folder. */
  readonly available: boolean;
  /** The folder's path, `%LOCALAPPDATA%` standing in for the user's profile. */
  path(): Promise<string>;
  /** Open the folder in Explorer. */
  open(): Promise<void>;
}

/** A newer release the updater offers. */
export interface AppUpdate {
  version: string;
  /** The release's "What's new": one `- ` bullet per change, as the release tag's notes carry them. */
  notes: string;
}

/**
 * How far an install has got (`src-tauri/src/update.rs`'s `Progress`, over `lf://update-progress`).
 * The three stages are the three waits a player sits through; only the download carries a number,
 * and only when the server declared the package's size.
 */
export type UpdateProgress =
  | { stage: 'downloading'; downloaded: number; total: number | null }
  | { stage: 'verifying' }
  | { stage: 'installing' };

/**
 * The app updater (`src-tauri/src/update.rs`): the signed installer and `latest.json` the release
 * workflow publishes on GitHub Releases.
 */
export interface AppUpdates {
  /** False in the browser build and in DEV (`tauri dev` never checks). */
  readonly available: boolean;
  /** A newer release, or null when this build is the latest. Rejects when the check fails (offline). */
  check(): Promise<AppUpdate | null>;
  /** Download the offered update and verify its signature, the engine playing on; one already
   * downloaded this launch answers at once. Rejects when a step fails. */
  download(): Promise<void>;
  /**
   * Install what `download` fetched: shut the engine down and start the installer, which quits the app
   * and opens the new version. Never resolves once the installer runs; rejects when it fails.
   */
  install(): Promise<void>;
  /**
   * Watch an install's stages. Registered once for the app's lifetime (`src/app/update.ts`): the
   * install ends in the installer quitting the app, so nothing ever unsubscribes.
   */
  onProgress(cb: (progress: UpdateProgress) => void): void;
}

export type PlatformKind = 'web' | 'tauri';

export interface Platform {
  readonly kind: PlatformKind;
  readonly pluginHost: PluginHost;
  readonly engine: EngineHost;
  readonly chain: ChainHost;
  readonly rig: RigHost;
  readonly exports: ExportFiles;
  readonly logs: LogFolder;
  readonly updates: AppUpdates;
  readonly midi: MidiHost;
  readonly input: InputHost;
}
