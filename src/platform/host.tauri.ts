import { Channel, invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type {
  AppUpdate,
  AppUpdates,
  AsioDeviceInfo,
  AsioStatusReport,
  AudioInputDevice,
  AudioOutputDevice,
  EngineHost,
  LogFolder,
  Platform,
  PluginDescriptor,
  PluginFolders,
  PluginHost,
  PluginInfo,
  PluginParamDesc,
  PluginSlot,
  ToneImport,
} from './host';
import { webPlatform } from './host.web';
import { decodeDeviceStatus, decodeFeedFrame } from './engine-wire';
// notify.ts is a ROOT-level module (src/notify.ts), not an app layer, so importing it from a platform/
// file is boundary-clean: check-boundary.mjs's leak regex flags only the app layers above platform/. It's the one user-visible error surface, imported here so a native transport failure below
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

/**
 * Native CLAP, VST3 and VST2 host over Tauri IPC. Each method maps to a `plugin_*` command in Rust.
 * `window`/`state` command args are injected by Tauri — JS passes only the domain args. Audio never
 * crosses as PCM here: a plugin plays inside the engine's callback.
 */
let frontendEpoch = 0;

const tauriPluginHost: PluginHost = {
  available: true,
  async init(sampleRate) {
    frontendEpoch = await invoke<number>('host_init', { sampleRate });
  },
  scanPlugins(force = false) {
    return invoke<PluginDescriptor[]>('plugin_scan', { force });
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
    await invoke('plugin_unload', { slot });
  },
  listLoaded() {
    return invoke<PluginInfo[]>('plugin_list_loaded');
  },
  async openEditor(slot, mode) {
    await invoke('plugin_open_editor', { slot, mode });
  },
  async closeEditor(slot) {
    await invoke('plugin_close_editor', { slot });
  },
  async setParameter(slot, paramId, value) {
    await invoke('plugin_set_param', { slot, paramId, value });
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
  importTone(slot, bytes, { format, path, id }) {
    // A header value must be ASCII: the plugin goes as JSON with every other character \u-escaped.
    const plugin = JSON.stringify({ format, path, id }).replace(
      /[\u007f-\uffff]/g,
      (c) => `\\u${c.charCodeAt(0).toString(16).padStart(4, '0')}`,
    );
    return invoke<ToneImport>('plugin_tone_import', bytes, { headers: { slot: String(slot), plugin } });
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
  async setSlotInputChannel(slot, channel) {
    await invoke('engine_set_slot_input_channel', { slot, channel });
  },
  async send(commands) {
    await invoke('engine_send', { commands });
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
    const channel = new Channel<unknown>();
    let live = true;
    let reported = false;
    channel.onmessage = (raw) => {
      if (!live) return;
      try {
        onFrame(decodeFeedFrame(raw));
      } catch (err) {
        // One report per subscription: a wire drift would otherwise log 60 times a second.
        if (reported) return;
        reported = true;
        console.error('[host.tauri] engine feed frame rejected', err);
        notifyError('The audio engine sent something the app cannot read', err);
      }
    };
    invoke('engine_feed', { channel }).catch((err: unknown) => {
      console.error('[host.tauri] engine feed subscribe failed', err);
      notifyError('The app lost contact with the audio engine', err);
    });
    return () => {
      live = false;
    };
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

/** The updater over Tauri IPC: `app_update_check` / `app_update_install` in `update.rs`. A `tauri dev`
 * build never checks: its version is the next release's, and the probes launch it many times. */
const tauriUpdates: AppUpdates = {
  available: !import.meta.env.DEV,
  check() {
    return invoke<AppUpdate | null>('app_update_check');
  },
  async install() {
    await invoke('app_update_install');
  },
};

/**
 * Tauri platform. Reuses the web Web-MIDI capability (it works inside WebView2 v149) and swaps in the
 * native CLAP/VST3/VST2 `pluginHost`, the native `engine`, the release log's folder (`logs`) and the
 * updater (`updates`).
 */
export const tauriPlatform: Platform = {
  ...webPlatform,
  kind: 'tauri',
  pluginHost: tauriPluginHost,
  engine: tauriEngineHost,
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
  report.midi = await probeMidi();
  await emitDiag(report);
}

async function emitDiag(report: Record<string, unknown>): Promise<void> {
  try {
    await invoke('diag', { report: JSON.stringify(report) });
  } catch {
    /* diag is best-effort — never block startup */
  }
}

/**
 * Probe WebView2's native Web MIDI: does navigator.requestMIDIAccess resolve (and enumerate),
 * or is it absent / blocked behind the permission prompt? 5s timeout so an unanswered permission
 * prompt reports `pending`.
 */
async function probeMidi(): Promise<unknown> {
  const nav = navigator as Navigator & {
    requestMIDIAccess?: (opts?: { sysex?: boolean }) => Promise<MIDIAccess>;
  };
  if (!nav.requestMIDIAccess) return { supported: false };
  try {
    const access = await Promise.race([
      nav.requestMIDIAccess({ sysex: false }),
      new Promise<never>((_, rej) => setTimeout(() => rej(new Error('pending')), 5000)),
    ]);
    const inputs = [...access.inputs.values()].map((i) => i.name ?? '(unnamed)');
    return { supported: true, resolved: true, inputCount: inputs.length, inputs };
  } catch (e) {
    return { supported: true, resolved: false, reason: String((e as Error)?.message ?? e) };
  }
}
