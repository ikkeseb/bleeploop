/**
 * The production browser build's platform: nothing native, nothing scriptable. The release ships under
 * Tauri, so this runs only when someone opens the built `dist` in a plain browser; it must boot, and every
 * capability says it is absent. The scriptable rig (`host.web.ts`) is DEV-only (`index.ts`).
 */
import type { LogFolder, MidiHost, Platform, PluginHost } from './host';

const ABSENT = 'Not available in the browser build.';
const absent = async (): Promise<never> => {
  throw new Error(ABSENT);
};
const nothing = async (): Promise<void> => {};
const none = async (): Promise<[]> => [];
const noFolders = async () => ({ builtin: [], user: [], unsupported: [], failed: [] });
const notCompiled = async () => ({ status: 'not-compiled' as const, detail: '' });

const pluginHost: PluginHost = {
  available: false,
  init: nothing,
  scanPlugins: none,
  scanPluginPaths: none,
  pluginFolders: noFolders,
  addPluginFolder: async () => null,
  removePluginFolder: noFolders,
  loadPlugin: absent,
  unloadPlugin: nothing,
  listLoaded: none,
  openEditor: absent,
  closeEditor: nothing,
  setParameter: nothing,
  listParams: none,
  onParamChanged: () => () => {},
  onParamsChanged: () => () => {},
  onEditorClosed: () => () => {},
  takeTone: async () => null,
  importTone: absent,
  forgetTone: absent,
  listInputDevices: none,
  listOutputDevices: none,
  asioStatus: notCompiled,
  asioProbe: notCompiled,
  asioSwitch: notCompiled,
  asioDrivers: none,
  asioDeviceInfo: async () => null,
};

const logs: LogFolder = { available: false, path: absent, open: absent };

const midi: MidiHost = {
  // Rejected as native MIDI that does not run; `subscribeMidi` reads it as epoch 0.
  subscribe: () => ({ epoch: absent(), stop() {} }),
  learn: absent,
  cancelLearn: absent,
  forget: absent,
  setMomentary: absent,
  setHold: absent,
  assign: absent,
  importLegacy: absent,
};

export const inertPlatform: Platform = {
  kind: 'web',
  pluginHost,
  engine: {
    available: false,
    open: absent,
    close: nothing,
    status: async () => null,
    setSlotInputChannel: absent,
    setShare: absent,
    snapshot: absent,
    loadSession: absent,
    subscribe: () => () => {},
  },
  exports: { save: absent },
  logs,
  updates: { available: false, check: async () => null, download: absent, install: absent, onProgress() {} },
  midi,
  input: { send: absent },
};
