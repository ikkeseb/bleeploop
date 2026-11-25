import { Channel, invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type {
  AppUpdate,
  AppUpdates,
  AsioDeviceInfo,
  AsioStatusReport,
  AudioInputDevice,
  AudioOutputDevice,
  ChainHost,
  EngineHost,
  ExportFiles,
  InputHost,
  LogFolder,
  MidiHost,
  Platform,
  PluginDescriptor,
  PluginFolders,
  PluginHost,
  PluginInfo,
  PluginParamDesc,
  PluginSlot,
  ToneImport,
  UpdateProgress,
} from './host';
import { decodeDeviceStatus, decodeFeedFrame, isUnitCommand, type PluginRef, type UnitId } from './engine-wire';
import { decodeDropped, decodeEpoch, decodeImportReport, decodeMidiEvent } from './midi-wire';
// notify.ts is a ROOT-level module (src/notify.ts), not an app layer, so importing it from a platform/
// file is boundary-clean: verify/guards/boundary.mjs's leak regex flags only the app layers above platform/. It's the one user-visible error surface, imported here so a native transport failure below
// reaches the user (not just console.error → the release log).
import { notifyError } from '../notify';

/**
 * Bridge Tauri's async `listen()` (→ Promise<UnlistenFn>) to the synchronous `() => void` unsubscribe
 * the PluginHost API exposes: register in the background, and return a fn that unlistens once the
 * promise resolves — or, if called before resolution, flags so the listener is torn down on arrival.
 */
function subscribe<T>(event: string, handler: (payload: T) => void): () => void {
  let unlisten: UnlistenFn | null = null;
  let cancelled = false;
  void listen<T>(event, (e) => handler(e.payload)).then((u) => {
    if (cancelled) u();
    else unlisten = u;
  });
  return () => {
    cancelled = true;
    unlisten?.();
    unlisten = null;
  };
}

/** Encode a plugin identity as ASCII JSON (\u-escaped) for a tone-import request header. */
function asciiPluginHeader({ format, path, id }: PluginRef): string {
  return JSON.stringify({ format, path, id }).replace(
    /[\u007f-\uffff]/g,
    (c) => `\\u${c.charCodeAt(0).toString(16).padStart(4, '0')}`,
  );
}

/**
 * Native CLAP, VST3 and VST2 host over Tauri IPC. Each method maps to a `plugin_*` command in Rust.
 * `window`/`app` command args are injected by Tauri — JS passes only the domain args. Audio never
 * crosses as PCM here: a plugin plays inside the engine's callback. Every request that changes a slot
 * (load, unload, a parameter, the editor) carries this document's `frontendEpoch` (`host_init`), so the
 * host refuses one this page sent before a reload replaced it (`src-tauri/src/engine_io/plugins.rs`).
 */
let frontendEpoch = 0;

function requireEpoch(): number {
  if (frontendEpoch === 0) throw new Error('plugin host not initialized');
  return frontendEpoch;
}

const tauriPluginHost: PluginHost = {
  available: true,
  async init() {
    frontendEpoch = await invoke<number>('host_init');
  },
  scanPlugins(force = false) {
    return invoke<PluginDescriptor[]>('plugin_scan', { force });
  },
  scanPluginPaths(paths) {
    return invoke<PluginDescriptor[]>('plugin_scan_paths', { paths });
  },
  pluginFolders() {
    return invoke<PluginFolders>('plugin_folders');
  },
  addPluginFolder() {
    return invoke<PluginFolders | null>('plugin_folder_add');
  },
  removePluginFolder(path) {
    return invoke<PluginFolders>('plugin_folder_remove', { path });
  },
  loadPlugin(slot, path, id, toneToken) {
    if (frontendEpoch === 0) throw new Error('plugin host not initialized');
    return invoke<PluginInfo>('plugin_load', { slot, path, id, frontendEpoch, toneToken: toneToken ?? null });
  },
  async unloadPlugin(slot) {
    await invoke('plugin_unload', { slot, frontendEpoch });
  },
  listLoaded() {
    return invoke<PluginInfo[]>('plugin_list_loaded');
  },
  async openEditor(slot) {
    await invoke('plugin_open_editor', { slot, frontendEpoch });
  },
  async closeEditor(slot) {
    await invoke('plugin_close_editor', { slot, frontendEpoch });
  },
  async setParameter(slot, paramId, value) {
    await invoke('plugin_set_param', { slot, paramId, value, frontendEpoch });
  },
  listParams(slot) {
    return invoke<PluginParamDesc[]>('plugin_list_params', { slot });
  },
  onParamChanged(cb) {
    return subscribe<{ slot: PluginSlot; id: number; value: number }>('plugin:param-changed', cb);
  },
  onParamsChanged(cb) {
    return subscribe<PluginSlot>('plugin:params-changed', (slot) => cb(slot));
  },
  onEditorClosed(cb) {
    return subscribe<PluginSlot>('plugin:editor-closed', (slot) => cb(slot));
  },
  // A tone moves as raw bytes both ways, as a session does (`engine_snapshot`).
  async takeTone(slot) {
    const bytes = await invoke<ArrayBuffer>('plugin_tone_take', { slot });
    return bytes.byteLength > 0 ? new Uint8Array(bytes) : null;
  },
  importTone(slot, bytes, plugin) {
    return invoke<ToneImport>('plugin_tone_import', bytes, { headers: { slot: String(slot), plugin: asciiPluginHeader(plugin) } });
  },
  async forgetTone(slot, reloadToken) {
    await invoke('plugin_tone_forget', { slot, token: reloadToken });
  },
  listInputDevices() {
    return invoke<AudioInputDevice[]>('plugin_list_input_devices');
  },
  listOutputDevices() {
    return invoke<AudioOutputDevice[]>('plugin_list_output_devices');
  },
  asioDeviceInfo() {
    return invoke<AsioDeviceInfo | null>('plugin_asio_device_info');
  },
  asioStatus() {
    return invoke<AsioStatusReport>('plugin_asio_status');
  },
  asioProbe(explicit, driver) {
    return invoke<AsioStatusReport>('plugin_asio_probe', { explicit, driver: driver || null });
  },
  asioSwitch(driver) {
    return invoke<AsioStatusReport>('plugin_asio_switch', { driver: driver || null });
  },
  asioDrivers() {
    return invoke<string[]>('plugin_asio_drivers');
  },
};

// The native feed has one subscriber; replacing it retires this document's previous one too.
let stopEngineFeed: (() => void) | null = null;

function retireFeedChannel(channel: Channel<unknown> | null): void {
  if (channel === null) return;
  // Read against @tauri-apps/api 2.11.1: Channel's private cleanupCallback does this,
  // but an index gap prevents its end message from reaching that cleanup.
  const internals = (window as Window & {
    __TAURI_INTERNALS__?: { unregisterCallback?: (id: number) => void };
  }).__TAURI_INTERNALS__;
  internals?.unregisterCallback?.(channel.id);
}

/**
 * The native engine over Tauri IPC: each method maps to an `engine_*` command in Rust
 * (`src-tauri/src/engine_io`); the feed arrives on a Tauri Channel. Payloads: `engine-wire.ts`.
 */
const tauriEngineHost: EngineHost = {
  available: true,
  async open(request, force = false) {
    return decodeDeviceStatus(await invoke<unknown>('engine_open', { request, force }));
  },
  async close() {
    await invoke('engine_close');
  },
  async status() {
    const status = await invoke<unknown>('engine_status');
    return status == null ? null : decodeDeviceStatus(status);
  },
  async setSlotInputChannel(slot, pick) {
    await invoke('engine_set_slot_input_channel', { slot, channel: pick });
  },
  async setShare(endpoint) {
    await invoke('engine_set_share', { endpoint });
  },
  // The session moves as raw bytes both ways: a JSON number array of a minute of five lanes would be
  // tens of megabytes of text.
  snapshot(master) {
    return invoke<ArrayBuffer>('engine_snapshot', { master });
  },
  async loadSession(bytes) {
    await invoke('engine_load_session', bytes);
  },
  subscribe(onFrame) {
    stopEngineFeed?.();
    let live = true;
    let reported = false;
    let generation = 0;
    let newest: number | null = null;
    let ackTimer: ReturnType<typeof setTimeout> | null = null;
    let subscribing = false;
    let liveChannel: Channel<unknown> | null = null;

    const ack = () => {
      if (!live) return;
      const gen = generation;
      void invoke<boolean>('engine_feed_ack', { seq: newest }).then((stuck) => {
        if (live && gen === generation && stuck && !subscribing) connect();
      }).catch(() => {
        // The next frame's timer or heartbeat retries a failed ack.
      });
    };

    const connect = () => {
      // Retire the old handler and every pending ack answer before the replacement subscribes.
      const gen = ++generation;
      retireFeedChannel(liveChannel);
      newest = null;
      if (ackTimer !== null) clearTimeout(ackTimer);
      ackTimer = null;
      subscribing = true;
      const channel = new Channel<unknown>();
      liveChannel = channel;
      channel.onmessage = (raw) => {
        if (!live || gen !== generation) return;
        // Even a rejected frame was handed on by this channel: it must not fill the native window.
        if (typeof raw === 'object' && raw !== null && 'seq' in raw && typeof raw.seq === 'number' && Number.isSafeInteger(raw.seq) && raw.seq >= 0) {
          newest = raw.seq;
        }
        if (ackTimer === null) {
          ackTimer = setTimeout(() => {
            ackTimer = null;
            if (live && gen === generation) ack();
          }, 100);
        }
        try {
          onFrame(decodeFeedFrame(raw));
        } catch (err) {
          // One report per subscription: a wire drift would otherwise log 60 times a second.
          if (reported) return;
          reported = true;
          console.error('[host.tauri] engine feed frame rejected', err);
          notifyError('Could not read an update from the audio engine. Restart BleepLoop if the screen stops following it.', err);
        }
      };
      void invoke('engine_feed', { channel }).catch((err: unknown) => {
        retireFeedChannel(channel);
        if (liveChannel === channel) liveChannel = null;
        if (!live || gen !== generation) return;
        console.error('[host.tauri] engine feed subscribe failed', err);
        notifyError('Could not reach the audio engine. Restart BleepLoop.', err);
      }).finally(() => {
        if (gen === generation) subscribing = false;
      });
    };
    const heartbeat = setInterval(ack, 1000);
    const unsubscribe = () => {
      if (!live) return;
      live = false;
      generation++;
      retireFeedChannel(liveChannel);
      liveChannel = null;
      if (ackTimer !== null) clearTimeout(ackTimer);
      clearInterval(heartbeat);
      if (stopEngineFeed === unsubscribe) stopEngineFeed = null;
    };
    stopEngineFeed = unsubscribe;
    connect();
    return unsubscribe;
  },
};

/**
 * Native MIDI over Tauri IPC: each method maps to a `midi_*` command in Rust (`src-tauri/src/engine_io`);
 * the events arrive on a Tauri Channel, which a reload's subscription replaces natively. Payloads:
 * `midi-wire.ts`.
 */
const tauriMidi: MidiHost = {
  subscribe(onEvent) {
    const channel = new Channel<unknown>();
    let live = true;
    let reported = false;
    channel.onmessage = (raw) => {
      if (!live) return;
      try {
        onEvent(decodeMidiEvent(raw));
      } catch (err) {
        // One report per subscription, as the feed's.
        if (reported) return;
        reported = true;
        console.error('[host.tauri] MIDI event rejected', err);
        notifyError('Could not read an update from MIDI. Restart BleepLoop if MIDI stops responding.', err);
      }
    };
    // The page's age: a newer page's subscribe wins over an older page's late one, whatever the order
    // the two calls reach native code in. Under the DEV flag `lf.chainNative` the page subscribes through
    // `midi_subscribe_chain` (DEV builds only, beside the `chain_*` commands), which turns native MIDI's
    // chain mode on for this epoch in the same step, so a learned IN FX pedal switches its built-in units
    // (`Toggle.Units`) from the first event on; a plain subscribe (always so in a release build) turns it off.
    const command = chainNative ? 'midi_subscribe_chain' : 'midi_subscribe';
    const epoch = invoke<unknown>(command, { channel, origin: performance.timeOrigin }).then(decodeEpoch);
    epoch.catch((err: unknown) => {
      console.error('[host.tauri] MIDI subscribe failed', err);
      notifyError('Could not reach MIDI. Restart BleepLoop to get it back.', err);
    });
    return {
      epoch,
      stop() {
        live = false;
      },
    };
  },
  learn(epoch, action, target) {
    return invoke<boolean>('midi_learn', { epoch, action, target });
  },
  cancelLearn(epoch) {
    return invoke<boolean>('midi_cancel_learn', { epoch });
  },
  forget(epoch, revision, index) {
    return invoke<boolean>('midi_forget', { epoch, revision, index });
  },
  setMomentary(epoch, revision, index, on) {
    return invoke<boolean>('midi_set_momentary', { epoch, revision, index, on });
  },
  setHold(epoch, revision, index, on) {
    return invoke<boolean>('midi_set_hold', { epoch, revision, index, on });
  },
  assign(epoch, revision, index, portId) {
    return invoke<boolean>('midi_assign', { epoch, revision, index, portId });
  },
  async importLegacy(epoch, json) {
    const report = await invoke<unknown>('midi_import_legacy', { epoch, json });
    return report === null ? null : decodeImportReport(report);
  },
};

/** The outbox's batches over Tauri IPC (`input_send`, synchronous on the main thread natively). */
const tauriInput: InputHost = {
  async send(epoch, items) {
    // Unit contract §2: with `lf.chainNative` off (always so in a release build), no unit command reaches
    // `input_send` (`src/platform/index.ts` `enqueueEngine` drops them first); with it on, all go through.
    if (!chainNative && items.some((item) => 'engine' in item && isUnitCommand(item.engine))) throw new Error(NO_CHAIN);
    return decodeDropped(await invoke<unknown>('input_send', { epoch, items }));
  },
};

/** One `export_save_part` call's bytes: an export reaches ~691 MB at 48 kHz, more than one body should. */
const EXPORT_PART_BYTES = 8 * 1024 * 1024;

/** The export over Tauri IPC, in parts, each awaited before the next (`src-tauri/src/export_save.rs`). */
const tauriExports: ExportFiles = {
  async save(bytes, filename) {
    const total = bytes.byteLength;
    for (let offset = 0; ; offset += EXPORT_PART_BYTES) {
      const part = bytes.slice(offset, offset + EXPORT_PART_BYTES);
      const headers = { name: filename, offset: String(offset), total: String(total) };
      const landed = await invoke<string | null>('export_save_part', part, { headers });
      if (offset + part.byteLength < total) continue;
      if (landed === null) throw new Error('The last part of the export was not answered with its file.');
      return landed;
    }
  },
};

/** The release log's folder over Tauri IPC: `app_log_dir` / `app_open_log_dir` in `lib.rs`. */
const tauriLogFolder: LogFolder = {
  available: true,
  path() {
    return invoke<string>('app_log_dir');
  },
  async open() {
    await invoke('app_open_log_dir');
  },
};

/** The updater over Tauri IPC: `app_update_check` / `app_update_download` / `app_update_install` in `update.rs`. A `tauri dev`
 * build never checks: its version is the next release's, and the probes launch it many times. */
const tauriUpdates: AppUpdates = {
  available: !import.meta.env.DEV,
  check() {
    return invoke<AppUpdate | null>('app_update_check');
  },
  async download() {
    await invoke('app_update_download');
  },
  async install() {
    await invoke('app_update_install');
  },
  onProgress(cb) {
    // `update.rs` serializes its `Progress` enum with `tag = "stage"`, so the payload IS the union.
    void listen<UpdateProgress>('lf://update-progress', (event) => cb(event.payload));
  },
};

/** DEV opt-in for the native chain host (`localStorage['lf.chainNative'] === '1'`), read once per page. */
function readChainNative(): boolean {
  if (!import.meta.env.DEV) return false;
  try {
    return typeof localStorage !== 'undefined' && localStorage.getItem('lf.chainNative') === '1';
  } catch {
    return false;
  }
}

const chainNative = readChainNative();

const NO_CHAIN = 'The native chain host is off (enable lf.chainNative in DEV).';
const noChain = async (): Promise<never> => {
  throw new Error(NO_CHAIN);
};

// Tauri does not guarantee two async commands start in the order sent, so structural calls on one
// slot run one after another (mirroring `chain-fake.ts`'s `onSlot`).
const chainSlotTails: [Promise<unknown>, Promise<unknown>] = [Promise.resolve(), Promise.resolve()];
const chainUnitSlots = new Map<UnitId, PluginSlot>();
function onChainSlot<T>(slot: PluginSlot, op: () => Promise<T>): Promise<T> {
  const run = chainSlotTails[slot].then(op);
  chainSlotTails[slot] = run.catch(() => {});
  return run;
}
function onChainUnit<T>(unit: UnitId, op: () => Promise<T>): Promise<T> {
  const slot = chainUnitSlots.get(unit);
  return slot === undefined ? op() : onChainSlot(slot, op);
}

/**
 * The chain per slot over Tauri IPC (`src-tauri/src/engine_io/chain_cmds.rs`, unit contract §2).
 * Enabled only in DEV when `localStorage['lf.chainNative'] === '1'` (read once per page); otherwise
 * `available` is false and every call rejects before any IPC without logging (the caller reads the
 * rejection).
 */
const tauriChainHost: ChainHost = chainNative
  ? {
      available: true,
      async add(slot, unit, spec, at, toneToken) {
        const frontendEpoch = requireEpoch();
        return onChainSlot(slot, async () => {
          const res = await invoke<'seated' | 'waiting'>('chain_add', {
            slot,
            unit,
            spec,
            at,
            toneToken: toneToken ?? null,
            frontendEpoch,
          });
          chainUnitSlots.set(unit, slot);
          return res;
        });
      },
      async remove(unit) {
        const frontendEpoch = requireEpoch();
        const slot = chainUnitSlots.get(unit);
        return onChainUnit(unit, async () => {
          await invoke('chain_remove', { unit, frontendEpoch });
          if (slot !== undefined && chainUnitSlots.get(unit) === slot) chainUnitSlots.delete(unit);
        });
      },
      async reload(unit, plugin, since, toneToken) {
        const frontendEpoch = requireEpoch();
        return onChainUnit(unit, () =>
          invoke<'reloaded' | 'moved' | 'failed'>('chain_reload', {
            unit,
            plugin,
            since,
            toneToken,
            frontendEpoch,
          }),
        );
      },
      async move(unit, to) {
        const frontendEpoch = requireEpoch();
        return onChainUnit(unit, async () => {
          await invoke('chain_move', { unit, to, frontendEpoch });
        });
      },
      async openEditor(unit) {
        await invoke('chain_open_editor', { unit, frontendEpoch: requireEpoch() });
      },
      async closeEditor(unit) {
        await invoke('chain_close_editor', { unit, frontendEpoch: requireEpoch() });
      },
      onEditorClosed(cb) {
        return subscribe<{ unit: UnitId }>('chain:editor-closed', ({ unit }) => cb(unit));
      },
      async listParams(unit) {
        return invoke<PluginParamDesc[]>('chain_list_params', { unit, frontendEpoch: requireEpoch() });
      },
      async setParameter(unit, paramId, value) {
        await invoke('chain_set_param', { unit, paramId, value, frontendEpoch: requireEpoch() });
      },
      async takeTone(unit) {
        const bytes = await invoke<ArrayBuffer>('chain_tone_take', { unit, frontendEpoch: requireEpoch() });
        return bytes.byteLength > 0 ? new Uint8Array(bytes) : null;
      },
      async importTone(slot, bytes, plugin, unit) {
        const headers: Record<string, string> = {
          slot: String(slot),
          plugin: asciiPluginHeader(plugin),
          epoch: String(requireEpoch()),
        };
        if (unit !== undefined) headers.unit = String(unit);
        return invoke<{ token: number | null; name: string; matches: UnitId[] }>('chain_tone_import', bytes, { headers });
      },
      async forgetTone(token) {
        await invoke('chain_tone_forget', { token, frontendEpoch: requireEpoch() });
      },
    }
  : {
      available: false,
      add: noChain,
      remove: noChain,
      reload: noChain,
      move: noChain,
      openEditor: noChain,
      closeEditor: noChain,
      onEditorClosed: () => () => {},
      listParams: noChain,
      setParameter: noChain,
      takeTone: noChain,
      importTone: noChain,
      forgetTone: noChain,
    };

/**
 * Tauri platform: the native CLAP/VST3/VST2 `pluginHost`, the native `engine`, native `midi` and `input`,
 * the export's save (`exports`), the release log's folder (`logs`) and the updater (`updates`).
 */
export const tauriPlatform: Platform = {
  kind: 'tauri',
  pluginHost: tauriPluginHost,
  engine: tauriEngineHost,
  chain: tauriChainHost,
  midi: tauriMidi,
  input: tauriInput,
  exports: tauriExports,
  logs: tauriLogFolder,
  updates: tauriUpdates,
};

// ── Close guard: Rust vetoes CloseRequested and forwards it as an event ────────────────────────

/**
 * Fires when the OS close button was pressed and Rust vetoed it. The app decides (a jam in progress
 * warrants a confirm) and calls `tauriConfirmClose` to actually close. Registered once for the app's
 * lifetime — the window closing IS the teardown, so no unlisten is kept.
 */
export function tauriOnCloseRequested(cb: () => void): void {
  void listen('lf://close-requested', () => cb());
}

/** Allow the close and close the window (the Rust side flips its guard and calls `window.close()`). */
export function tauriConfirmClose(): Promise<void> {
  return invoke('app_confirm_close');
}

// ── DEV diagnostics (Tauri only; reported to `tauri dev` stdout — no Playwright into WebView2) ────

/**
 * One-shot DEV startup diagnostic: report WebView2-internal facts to the Rust side (printed to
 * `tauri dev` stdout) so headless verification can read them — there is no Playwright into
 * WebView2. Best-effort; never throws into app startup. Informational only — it does NOT load any
 * plugin (the plugin-picker UI drives load/editor/param).
 */
export async function reportTauriDiagnostics(): Promise<void> {
  const report: Record<string, unknown> = {
    host: 'tauri',
    secureContext: self.isSecureContext === true,
    userAgent: navigator.userAgent,
  };
  void runBenchStalls();
  await emitDiag(report);
}

/**
 * DEV: the MIDI latency benchmark's UI stalls (`src-tauri/src/engine_io/midi_bench.rs`; how to run it:
 * `docs/VERIFY.md` § MIDI latency benchmark). When the benchmark asks for them, the page first measures
 * its clock's offset to the Rust side over a few round trips, then keeps the main thread busy for `ms`
 * every `everyMs` and reports each window in `performance.now()` time, which Rust places through that
 * offset (no IPC delay moves a window). Stops once the benchmark has ended. Nothing runs when no
 * benchmark asked.
 */
async function runBenchStalls(): Promise<void> {
  if (!import.meta.env.DEV) return;
  let plan: { everyMs: number; ms: number } | null;
  try {
    plan = await invoke<{ everyMs: number; ms: number } | null>('midi_bench_stall_plan');
    if (!plan) return;
    const samples: [number, number, number][] = [];
    for (let i = 0; i < 20; i++) {
      const sent = performance.now();
      const rust = await invoke<number>('midi_bench_clock');
      samples.push([sent, rust, performance.now()]);
    }
    if (!(await invoke<unknown>('midi_bench_clock_sync', { samples }))) return;
  } catch {
    return;
  }
  const { everyMs, ms } = plan;
  const timer = setInterval(() => {
    const start = performance.now();
    while (performance.now() - start < ms) {
      // A long main-thread task, on purpose: what a UI event waits behind.
    }
    invoke<boolean>('midi_bench_stall', { startMs: start, endMs: performance.now() })
      .then((more) => {
        if (!more) clearInterval(timer);
      })
      .catch(() => clearInterval(timer));
  }, everyMs);
}

async function emitDiag(report: Record<string, unknown>): Promise<void> {
  try {
    await invoke('diag', { report: JSON.stringify(report) });
  } catch {
    /* diag is best-effort — never block startup */
  }
}
