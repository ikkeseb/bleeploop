import { createSignal } from 'solid-js';
import {
  platform,
  type AsioDeviceInfo,
  type AsioStatusReport,
  type AudioInputDevice,
  type AudioOutputDevice,
} from '../../platform';
import { readAudioDeviceSettings, writeAudioDeviceSettings, type BufferFrames } from './audio-settings';
import { notifyError } from '../../notify';

/**
 * OWNS: the process-wide native audio configuration the Audio Settings panel edits — the enumerated
 * capture/output devices (+ pruning of a persisted id that vanished), the buffer size, and the ASIO
 * tier preference and driver. None of it is per-slot, so nothing here rides the per-slot op chain.
 * The buffer and the tier are saved choices the engine's device open reads (`engine-store.ts`); the
 * ASIO probe and driver switch are serialized here. Everything is a no-op / empty / persisted-default
 * in the browser build.
 */

// Native capture devices the host enumerates.
const [inputDevices, setInputDevices] = createSignal<AudioInputDevice[]>([]);
// Native OUTPUT devices the host enumerates (the output and Share pickers).
const [outputDevices, setOutputDevices] = createSignal<AudioOutputDevice[]>([]);

// The buffer size: the chosen block in frames, init from the persisted setting. ONE process-wide value
// (not per-slot); the engine's device open reads it.
const [bufferFrames, setBufferFramesSig] = createSignal<BufferFrames>(
  readAudioDeviceSettings().bufferFrames,
);

// ASIO low-latency tier: whether the native build offers an ASIO device (drives the Audio Settings
// toggle's enabled state) and whether the tier is preferred (init from persisted). The driver is contacted ONLY through `probeAsio` — at boot
// when the saved preference is on, or from an explicit user action — never by the native startup
// path (`src-tauri/src/asio_startup.rs` owns the probe rules; `asioStatus` mirrors them).
const [asioAvailable, setAsioAvailableSig] = createSignal(false);
const [asioStatus, setAsioStatus] = createSignal<AsioStatusReport>({ status: 'not-compiled', detail: '' });
const [asioDeviceInfo, setAsioDeviceInfo] = createSignal<AsioDeviceInfo | null>(null);
// The installed ASIO drivers' names for the driver picker (read from the registry; no driver loads).
const [asioDrivers, setAsioDrivers] = createSignal<string[]>([]);
const [asioEnabled, setAsioEnabledSig] = createSignal<boolean>(readAudioDeviceSettings().asioEnabled);
export const usingAsio = () => asioAvailable() && asioEnabled();

// Each slot's capture channel pick ('' = auto), saved with the device picks. A pick is
// device-bound: an input device change or prune resets both, and a device without it resets that one.
const [slotInputChannels, setSlotInputChannelsSig] = createSignal(readAudioDeviceSettings().slotInputChannels);

/** Save and show each slot's capture channel pick. */
export function saveSlotInputChannels(picks: readonly [string, string]): void {
  if (picks[0] === slotInputChannels()[0] && picks[1] === slotInputChannels()[1]) return;
  setSlotInputChannelsSig([picks[0], picks[1]]);
  writeAudioDeviceSettings({ slotInputChannels: [picks[0], picks[1]] });
}

/** The input channels the saved (or cached ASIO) input device has; 0 = unknown (the default WASAPI
 * device, or a list not read yet). */
function savedInputChannelCount(): number {
  if (usingAsio()) return asioDeviceInfo()?.inputChannels ?? 0;
  const id = readAudioDeviceSettings().inputDeviceId;
  return id ? (inputDevices().find((d) => d.id === id)?.channels ?? 0) : 0;
}

let configurationTail: Promise<void> = Promise.resolve();
function configure(op: () => Promise<void>): Promise<void> {
  const run = configurationTail.then(op);
  configurationTail = run.catch(() => {});
  return run;
}

/**
 * Refresh the list of native capture devices (no-op / empty in the browser build). Cheap native
 * enumeration; the plugin-input device picker calls this when it mounts so a freshly-plugged
 * interface shows up without a full app restart.
 */
export async function refreshInputDevices(): Promise<boolean> {
  if (!platform.pluginHost.available) return false;
  try {
    setInputDevices(await platform.pluginHost.listInputDevices());
    return true;
  } catch (e) {
    console.error('[instrument] list input devices failed', e);
    notifyError('Could not list audio inputs', e);
    return false;
  }
}

/**
 * Refresh the list of native OUTPUT devices (no-op / empty in the browser build). Cheap native
 * enumeration.
 */
export async function refreshOutputDevices(): Promise<boolean> {
  if (!platform.pluginHost.available) return false;
  try {
    setOutputDevices(await platform.pluginHost.listOutputDevices());
    return true;
  } catch (e) {
    console.error('[instrument] list output devices failed', e);
    notifyError('Could not list audio outputs', e);
    return false;
  }
}

/**
 * Refresh BOTH native device lists and prune any persisted device id no longer present (an interface
 * unplugged since it was last picked), resetting it to '' so a later arm falls back to the default
 * device instead of failing on a stale id; and each slot's channel pick the device lacks, or both with
 * a pruned input device. Called once at startup — so pruning happens before the user
 * can arm — and again whenever the Audio Settings popover opens (to catch a mid-session unplug). No-op
 * in the web build.
 */
export async function refreshAndPruneDevices(): Promise<void> {
  if (!platform.pluginHost.available) return;
  const inputRefreshSucceeded = await refreshInputDevices();
  const outputRefreshSucceeded = await refreshOutputDevices();
  const s = readAudioDeviceSettings();
  if (
    inputRefreshSucceeded &&
    s.inputDeviceId &&
    !inputDevices().some((d) => d.id === s.inputDeviceId)
  ) {
    // This endpoint belongs to WASAPI. Its disappearance cannot invalidate the cached ASIO channel.
    writeAudioDeviceSettings({ inputDeviceId: '', ...(usingAsio() ? {} : { inputChannel: '' }) });
    if (!usingAsio()) saveSlotInputChannels(['', '']);
  }
  // A slot pick past the device's inputs (saved on a device with more) is not what the slot reads.
  const inputs = savedInputChannelCount();
  if (inputs > 0) {
    const picks = slotInputChannels();
    saveSlotInputChannels([
      picks[0] !== '' && Number(picks[0]) >= inputs ? '' : picks[0],
      picks[1] !== '' && Number(picks[1]) >= inputs ? '' : picks[1],
    ]);
  }
  if (
    outputRefreshSucceeded &&
    s.outputDeviceId &&
    !outputDevices().some((d) => d.id === s.outputDeviceId)
  ) {
    writeAudioDeviceSettings({ outputDeviceId: '' });
  }
}

/** Save and show the buffer size; the engine's next device open takes it (Audio Settings reopens). */
export async function setBufferSize(frames: BufferFrames): Promise<void> {
  setBufferFramesSig(frames);
  writeAudioDeviceSettings({ bufferFrames: frames });
}

// ---------------------------------------------------------------------------
// ASIO low-latency tier — runtime host preference
// ---------------------------------------------------------------------------

/** Publish a probe/status report: availability + device metadata follow `ready` together. */
async function applyAsioReport(report: AsioStatusReport): Promise<void> {
  setAsioStatus(report);
  const ready = report.status === 'ready';
  setAsioAvailableSig(ready);
  setAsioDeviceInfo(ready ? await platform.pluginHost.asioDeviceInfo() : null);
}

/**
 * Ask the native host for the ASIO driver probe (see `asioProbe` in `host.ts`). `explicit` marks a
 * user action (toggle / Retry), which may proceed past a blocked or failed earlier attempt; boot passes
 * false. Serialized with the other host writes so it can never interleave with a driver flip.
 */
export async function probeAsio(explicit: boolean): Promise<AsioStatusReport> {
  let report = asioStatus();
  try {
    await configure(async () => {
      setAsioStatus({ status: 'probing', detail: '' });
      report = await platform.pluginHost.asioProbe(explicit, readAudioDeviceSettings().asioDriver);
      await applyAsioReport(report);
    });
  } catch (e) {
    console.error('[instrument] ASIO probe failed', e);
    notifyError('Could not start the ASIO driver', e);
    report = { status: 'failed', detail: e instanceof Error ? e.message : String(e) };
    setAsioStatus(report);
  }
  return report;
}

/**
 * Replace the ASIO driver with `driver` ('' = automatic) without a restart: the host drops the cached
 * driver and probes this one. The pick is saved once the host took it. Nothing may hold the driver: in
 * the engine the host's device owner closes its ASIO run first and reopens it after
 * (`switchEngineAsioDriver` in `src/ui/state/engine-store.ts` runs the whole switch). Rejects when the
 * host refuses, with the status as it was.
 */
export async function switchAsioDriver(driver: string): Promise<AsioStatusReport> {
  let report = asioStatus();
  await configure(async () => {
    const before = asioStatus();
    setAsioStatus({ status: 'probing', detail: '' });
    try {
      report = await platform.pluginHost.asioSwitch(driver);
    } catch (e) {
      setAsioStatus(before);
      throw e;
    }
    writeAudioDeviceSettings({ asioDriver: driver });
    await applyAsioReport(report);
  });
  return report;
}

/** Refresh the installed ASIO drivers' names (the driver picker, when Audio Settings opens). */
export async function refreshAsioDrivers(): Promise<void> {
  try {
    setAsioDrivers(await platform.pluginHost.asioDrivers());
  } catch (e) {
    console.error('[instrument] list ASIO drivers failed', e);
    notifyError('Could not list the ASIO drivers', e);
  }
}

/** Whether the ASIO row should offer a control at all (the binary can do ASIO and it was not disabled at launch). */
export const asioOffered = () =>
  asioStatus().status !== 'not-compiled' && asioStatus().status !== 'disabled-by-flag';

/** Whether an explicit user retry can do anything (see `AsioStartupStatus`). */
export const asioRetryable = () => {
  const s = asioStatus().status;
  return s === 'unprobed' || s === 'failed' || s === 'blocked';
};

/**
 * Save the ASIO-tier preference; the engine's next device open takes it. Turning it ON when the driver
 * has not been probed yet (or the last attempt failed / was blocked) is the explicit user action that
 * runs the probe; turning it OFF never touches the driver.
 */
export async function setAsioEnabled(enabled: boolean): Promise<void> {
  setAsioEnabledSig(enabled);
  writeAudioDeviceSettings({ asioEnabled: enabled });
  if (enabled && asioRetryable()) await probeAsio(true);
}

/**
 * Read the saved buffer and driver settings and the ASIO status before the engine's first device open.
 * The ASIO driver is probed here — after the window is up — and only when the saved preference is on; a
 * saved "off" never contacts the driver. The probe is awaited (bounded by the native deadline) so
 * `asioAvailable()` is fixed before the device opens.
 */
export async function initAudioDeviceSettings(): Promise<void> {
  if (!platform.pluginHost.available) return;
  const saved = readAudioDeviceSettings();
  setBufferFramesSig(saved.bufferFrames);
  setAsioEnabledSig(saved.asioEnabled);
  try {
    const status = await platform.pluginHost.asioStatus();
    await applyAsioReport(status);
    const saved = readAudioDeviceSettings();
    if (saved.asioEnabled && status.status === 'unprobed') await probeAsio(false);
  } catch (e) {
    console.error('[instrument] ASIO status query failed', e);
    notifyError('Could not check ASIO availability', e);
  }
}

/** Read-only reactive accessors: native capture + output devices, each slot's capture channel pick. */
export { inputDevices, outputDevices, slotInputChannels };

/** Read-only reactive accessor: the buffer size (frames) for the settings readout. */
export { bufferFrames };

/** Read-only reactive accessors: ASIO tier availability, startup status + preference (the settings toggle),
 * the cached driver and the installed ones (the driver picker). */
export { asioAvailable, asioEnabled, asioDeviceInfo, asioDrivers, asioStatus };
