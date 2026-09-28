import * as audioDeviceSettings from '../ui/state/audio-settings';
import { asioStatus, probeAsio, setAsioEnabled, setBufferSize } from '../ui/state/audio-devices';
import { autosave } from '../session/autosave';
import { buildExportBundle } from '../session/export';
import { importSession } from '../session/import';
import { inputRouter } from '../ui/state/input-router';
import {
  activeSlot,
  availablePlugins,
  ensureActive,
  scanForPlugins,
  selectPlugin,
  selectSynth,
  setActiveSlot,
  setPluginGain,
  slotIds,
  slotPlugins,
} from '../ui/state/instrument';
import * as midi from '../ui/state/midi';
import { clock, looper, master } from '../ui/state/audio';
import { dismissToast, notifyError, toasts } from '../notify';
import { engineFake, platform, reportDiagnostics } from '../platform';
import * as layoutStore from '../ui/layout/layout-store';

/**
 * The DEV debug surface (`window.__lf`) for automated (Playwright) verification + by-ear/by-eye
 * gates — the browser probes in `verify/probes/` and `docs/VERIFY.md` drive the app through it, so
 * its shape is a CONTRACT: add keys freely, never rename or drop one without updating those.
 * Installed only under `import.meta.env.DEV` (app.tsx); a release build never carries it.
 */
export interface LfDebug {
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
  /** The engine store's `looper`, `clock` and `master` (`src/ui/state/audio.ts`). */
  clock: typeof clock;
  looper: typeof looper;
  master: typeof master;
  layoutStore: typeof layoutStore;
  audioDeviceSettings: typeof audioDeviceSettings;
  setBufferSize: typeof setBufferSize;
  setAsioEnabled: typeof setAsioEnabled;
  /** ASIO startup coordinator readout + explicit probe (the Audio Settings RETRY path). */
  asioStatus: typeof asioStatus;
  probeAsio: typeof probeAsio;
  midi: typeof midi;
  platform: typeof platform;
  /** The browser's engine fake (`src/platform/host.web.ts`): the commands the UI sent and `emit(frame)` to
   * script the feed. The browser build has an engine only with `window.__lfEngineFake = true` set
   * before the app loads (`verify/probes/engine-seam.mjs`). Null under Tauri. */
  native: typeof engineFake;
  /** Mute the master (true) or unmute it; routes through `master` so the UI mute + slider stay
   * consistent. */
  setMasterMute: (on: boolean) => void;
  /** Live-tune a slot's plugin output level, the engine's slot gain (it starts conservative to spare
   * your ears). */
  setPluginGain: (v: number, slot?: number) => void;
  pluginSetParam: (paramId: number, value: number, slot?: number) => Promise<void>;
  pluginListParams: (slot?: number) => ReturnType<typeof platform.pluginHost.listParams>;
  /** Subscribe to editor-originated param changes (returns an unsubscribe fn). By-ear:
   * `__lf.onPluginParam(e => console.log(e))` then drag a knob. */
  onPluginParam: (cb: (e: { slot: 0 | 1; id: number; value: number }) => void) => () => void;
  onPluginEditorClosed: (cb: (slot: 0 | 1) => void) => () => void;
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
    asioStatus,
    probeAsio,
    midi,
    platform,
    native: engineFake,
    setMasterMute: (on) => {
      master.setMuted(on);
    },
    setPluginGain: (v, slot = 0) => setPluginGain(slot as 0 | 1, v),
    pluginSetParam: (paramId, value, slot = 0) =>
      platform.pluginHost.setParameter(slot as 0 | 1, paramId, value),
    pluginListParams: (slot = 0) => platform.pluginHost.listParams(slot as 0 | 1),
    onPluginParam: (cb) => platform.pluginHost.onParamChanged(cb),
    onPluginEditorClosed: (cb) => platform.pluginHost.onEditorClosed(cb),
    pluginPanic: () => inputRouter.allNotesOff(),
    importSession,
    buildExportBundle,
    autosave,
    notify: { notifyError, dismissToast, toasts },
    ui,
  };
}
