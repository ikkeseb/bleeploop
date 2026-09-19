import { getTransport } from 'tone';
import * as audioDeviceSettings from '../audio/audio-settings';
import { setAsioEnabled, setBufferSize } from '../audio/audio-devices';
import { autosave } from '../audio/autosave';
import { clock } from '../audio/clock';
import { engine } from '../audio/engine';
import { buildExportBundle } from '../audio/export/export';
import { importSession } from '../audio/export/import';
import { inputRouter } from '../audio/input-router';
import {
  activeSlot,
  availablePlugins,
  ensureActive,
  scanForPlugins,
  selectPlugin,
  selectSynth,
  setActiveSlot,
  slotIds,
  slotPlugins,
} from '../audio/instrument';
import { looper } from '../audio/looper/looper';
import { master } from '../audio/master';
import * as midi from '../audio/midi';
import { injectRecordLossForTest, pluginBridge } from '../audio/plugin-bridge';
import { recordLatency } from '../audio/record-latency';
import { dismissToast, notifyError, toasts } from '../notify';
import { platform, reportDiagnostics } from '../platform';
import * as layoutStore from '../ui/layout/layout-store';

/**
 * The DEV debug surface (`window.__lf`) for automated (Playwright) verification + by-ear/by-eye
 * gates — `verify/golden-jam.mjs` and every probe in `docs/VERIFY.md` drive the app through it, so
 * its shape is a CONTRACT: add keys freely, never rename or drop one without updating those.
 * Installed only under `import.meta.env.DEV` (app.tsx); a release build never carries it.
 */
export interface LfDebug {
  engine: typeof engine;
  inputRouter: typeof inputRouter;
  ensureActive: typeof ensureActive;
  selectSynth: typeof selectSynth;
  selectPlugin: typeof selectPlugin;
  setActiveSlot: typeof setActiveSlot;
  slotIds: typeof slotIds;
  slotPlugins: typeof slotPlugins;
  activeSlot: typeof activeSlot;
  availablePlugins: typeof availablePlugins;
  scanForPlugins: typeof scanForPlugins;
  clock: typeof clock;
  looper: typeof looper;
  master: typeof master;
  layoutStore: typeof layoutStore;
  audioDeviceSettings: typeof audioDeviceSettings;
  setBufferSize: typeof setBufferSize;
  setAsioEnabled: typeof setAsioEnabled;
  midi: typeof midi;
  /** Raw Tone transport handle for timing diagnostics (engine.ctx is already touched by the time
   * audio is running, so getTransport() is safe here). */
  transport: () => ReturnType<typeof getTransport>;
  /** Native bridge handles + the record-loss fault injector the golden jam uses. */
  pluginBridge: typeof pluginBridge & { injectRecordLossForTest: typeof injectRecordLossForTest };
  platform: typeof platform;
  /** Record-latency compensation levers: `lastCompensation()` shows the last C breakdown;
   * `setEnabled(false)` A/Bs the whole thing off by ear; `setOffsetMs(ms)` is the hidden trim;
   * `setFloorEnabled(false)` A/Bs just the clickOut floor. */
  recordLatency: typeof recordLatency;
  /** Single-clock test helper: setMasterMute(true) must silence the speakers (proves no second
   * cpal clock); (false) restores. Routes through `master` so the UI mute + slider stay consistent. */
  setMasterMute: (on: boolean) => void;
  /** Live-tune a slot's plugin output level (it starts conservative to spare your ears). */
  setPluginGain: (v: number, slot?: number) => void;
  pluginNoteOn: (note: number, velocity?: number, slot?: number) => Promise<void>;
  pluginNoteOff: (note: number, slot?: number) => Promise<void>;
  pluginSetParam: (paramId: number, value: number, slot?: number) => Promise<void>;
  pluginListParams: (slot?: number) => ReturnType<typeof platform.pluginHost.listParams>;
  /** Subscribe to editor-originated param changes (returns an unsubscribe fn). By-ear:
   * `__lf.onPluginParam(e => console.log(e))` then drag a knob. */
  onPluginParam: (cb: (e: { slot: 0 | 1; id: number; value: number }) => void) => () => void;
  onPluginEditorClosed: (cb: (slot: 0 | 1) => void) => () => void;
  pluginSaveState: (slot?: number) => ReturnType<typeof platform.pluginHost.saveState>;
  pluginLoadState: (bytes: Uint8Array, slot?: number) => Promise<void>;
  pluginPanic: () => void;
  /** Session import/export headless: a probe builds the zip bytes, feeds them back through import,
   * and byte-asserts the round trip without touching <a download> or a file input. */
  importSession: typeof importSession;
  buildExportBundle: typeof buildExportBundle;
  autosave: typeof autosave;
  /** The error-toast store, so a probe can drive/inspect toasts (notify.ts is the store). */
  notify: { notifyError: typeof notifyError; dismissToast: typeof dismissToast; toasts: typeof toasts };
  /** Popover drivers. The Audio-settings gear is Tauri-gated (no button in the web build), but the
   * popover MOUNT is platform-agnostic — flipping the signal renders it — so a probe can open +
   * screenshot the settings/help popovers headless. */
  ui: {
    openSettings: () => void;
    closeSettings: () => void;
    openHelp: () => void;
    closeHelp: () => void;
  };
}

declare global {
  interface Window {
    __lf?: LfDebug;
  }
}

/** Build + attach `window.__lf`. `ui` is supplied by app.tsx (the popover signals live there). */
export function installLfDebug(ui: LfDebug['ui']): void {
  // Report WebView2-internal facts to `tauri dev` stdout (no Playwright into WebView2).
  if (platform.kind === 'tauri') void reportDiagnostics();
  window.__lf = {
    engine,
    inputRouter,
    ensureActive,
    selectSynth,
    selectPlugin,
    setActiveSlot,
    slotIds,
    slotPlugins,
    activeSlot,
    availablePlugins,
    scanForPlugins,
    clock,
    looper,
    master,
    layoutStore,
    audioDeviceSettings,
    setBufferSize,
    setAsioEnabled,
    midi,
    transport: () => getTransport(),
    pluginBridge: { ...pluginBridge, injectRecordLossForTest },
    platform,
    recordLatency,
    setMasterMute: (on) => {
      master.setMuted(on);
    },
    setPluginGain: (v, slot = 0) => pluginBridge.setGain(slot, v),
    pluginNoteOn: (note, velocity = 0.7, slot = 0) =>
      platform.pluginHost.noteOn(slot as 0 | 1, note, velocity),
    pluginNoteOff: (note, slot = 0) => platform.pluginHost.noteOff(slot as 0 | 1, note),
    pluginSetParam: (paramId, value, slot = 0) =>
      platform.pluginHost.setParameter(slot as 0 | 1, paramId, value),
    pluginListParams: (slot = 0) => platform.pluginHost.listParams(slot as 0 | 1),
    onPluginParam: (cb) => platform.pluginHost.onParamChanged(cb),
    onPluginEditorClosed: (cb) => platform.pluginHost.onEditorClosed(cb),
    pluginSaveState: (slot = 0) => platform.pluginHost.saveState(slot as 0 | 1),
    pluginLoadState: (bytes, slot = 0) => platform.pluginHost.loadState(slot as 0 | 1, bytes),
    pluginPanic: () => inputRouter.allNotesOff(),
    importSession,
    buildExportBundle,
    autosave,
    notify: { notifyError, dismissToast, toasts },
    ui,
  };
}
