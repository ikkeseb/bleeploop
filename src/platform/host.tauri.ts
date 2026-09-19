import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type {
  AudioInputDevice,
  AudioOutputDevice,
  Platform,
  PluginDescriptor,
  PluginHost,
  PluginInfo,
  PluginParamDesc,
  PluginSlot,
} from './host';
import { webPlatform } from './host.web';
// notify.ts is a ROOT-level module (src/notify.ts), NOT under ../audio or ../ui, so importing it from
// a platform/ file is boundary-clean: check-boundary.mjs's leak regex only flags ../audio/ + ../ui/
// imports. It's the one user-visible error surface, imported here so a native transport failure below
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
 * Native CLAP + VST3 host over Tauri IPC. Each method maps to a `plugin_host::*` command in
 * Rust. `window`/`state` command args are injected by Tauri — JS passes only the domain args. Audio
 * never crosses as PCM here: the plugin's samples arrive out-of-band via a WebView2 SharedBuffer that
 * the `sharedbufferreceived` listener (below) forwards to `src/audio/plugin-bridge.ts` as an AudioNode.
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
  loadPlugin(slot, path, id) {
    if (frontendEpoch === 0) throw new Error('plugin host not initialized');
    return invoke<PluginInfo>('plugin_load', { slot, path, id, frontendEpoch });
  },
  async unloadPlugin(slot) {
    await invoke('plugin_unload', { slot });
  },
  listLoaded() {
    return invoke<PluginInfo[]>('plugin_list_loaded');
  },
  async noteOn(slot, note, velocity) {
    await invoke('plugin_note_on', { slot, note, velocity });
  },
  async noteOff(slot, note) {
    await invoke('plugin_note_off', { slot, note });
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
  onStreamFault(cb) {
    return subscribe<{ slot: PluginSlot; kind: 'input' | 'output' }>('plugin:stream-fault', cb);
  },
  async saveState(slot) {
    return new Uint8Array(await invoke<number[]>('plugin_save_state', { slot }));
  },
  async loadState(slot, bytes) {
    await invoke('plugin_load_state', { slot, bytes: Array.from(bytes) });
  },
  listInputDevices() {
    return invoke<AudioInputDevice[]>('plugin_list_input_devices');
  },
  async armInput(slot, deviceId, channel) {
    await invoke('plugin_arm_input', {
      slot,
      deviceId: deviceId ?? null,
      channel: channel ?? null,
    });
  },
  async disarmInput(slot) {
    await invoke('plugin_disarm_input', { slot });
  },
  listOutputDevices() {
    return invoke<AudioOutputDevice[]>('plugin_list_output_devices');
  },
  async armMonitor(slot, deviceId) {
    await invoke('plugin_arm_monitor', { slot, deviceId: deviceId ?? null });
  },
  async disarmMonitor(slot) {
    await invoke('plugin_disarm_monitor', { slot });
  },
  async setMonitorGain(slot, gain) {
    await invoke('plugin_set_monitor_gain', { slot, gain });
  },
  async setMasterGain(gain) {
    await invoke('plugin_set_master_gain', { gain });
  },
  async monitorLatencySeconds(slot) {
    return invoke<number>('plugin_monitor_latency', { slot });
  },
  async setBufferSize(frames) {
    await invoke('plugin_set_buffer_size', { frames });
  },
  asioAvailable() {
    return invoke<boolean>('plugin_asio_available');
  },
  asioDeviceInfo() {
    return invoke('plugin_asio_device_info');
  },
  async setAsioEnabled(enabled) {
    await invoke('plugin_set_asio_enabled', { enabled });
  },
};

/**
 * Tauri platform (P7+). Reuses the web getUserMedia/Web-MIDI capabilities (both work inside
 * WebView2 v149) and swaps in the native CLAP/VST3 `pluginHost`. Only `kind` + `pluginHost` differ.
 */
export const tauriPlatform: Platform = {
  ...webPlatform,
  kind: 'tauri',
  pluginHost: tauriPluginHost,
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

// ── Plugin audio transport: the WebView2 SharedBuffer ↔ audio bridge glue ───────────────────────

let bufferSink: ((ab: ArrayBuffer, meta: unknown) => void) | null = null;
let sinkRegistered = false;

/**
 * Register the audio sink for posted plugin SharedBuffers. The `chrome.webview` event stays in the
 * platform layer (WebView2-specific); the sink (`src/audio/plugin-bridge.ts`) receives only a plain
 * ArrayBuffer + the parsed `additionalData` meta, keeping `src/audio/` WebView2-agnostic.
 */
export function setPluginBufferSink(sink: (ab: ArrayBuffer, meta: unknown) => void): void {
  bufferSink = sink;
  if (sinkRegistered) return;
  const wv = window.chrome?.webview;
  if (!wv) return; // plain browser / Playwright: no native host, nothing to receive
  wv.addEventListener('sharedbufferreceived', (e) => {
    try {
      const ab = e.getBuffer();
      const meta = e.additionalData as { frontendEpoch?: unknown };
      if (meta.frontendEpoch !== frontendEpoch) {
        releasePluginBuffer(ab);
        return;
      }
      bufferSink?.(ab, meta);
    } catch (err) {
      console.error('[host.tauri] sharedbufferreceived handler failed', err);
      notifyError('Plugin audio hit a problem — reload the plugin if sound stops', err);
    }
  });
  sinkRegistered = true;
}

/** Detach + free a plugin SharedBuffer's JS view (the JS counterpart to the host's `Close()`). */
export function releasePluginBuffer(ab: ArrayBuffer): void {
  try {
    window.chrome?.webview?.releaseBuffer(ab);
  } catch {
    /* best-effort */
  }
}

// ── DEV diagnostics (Tauri only; reported to `tauri dev` stdout — no Playwright into WebView2) ────

/**
 * One-shot DEV startup diagnostic: report WebView2-internal facts to the Rust side (printed to
 * `tauri dev` stdout) so headless verification can read them — there is no Playwright into
 * WebView2. Best-effort; never throws into app startup. Informational only — it does NOT load any
 * plugin (the plugin-picker UI drives load/editor/param now).
 */
export async function reportTauriDiagnostics(): Promise<void> {
  const report: Record<string, unknown> = {
    host: 'tauri',
    crossOriginIsolated: self.crossOriginIsolated === true,
    sharedArrayBuffer: typeof SharedArrayBuffer !== 'undefined',
    getUserMedia: !!navigator.mediaDevices?.getUserMedia,
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
