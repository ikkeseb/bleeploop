import { createSignal } from 'solid-js';
import { platform, type AudioInputDevice, type AudioOutputDevice } from '../platform';
import { readAudioDeviceSettings, writeAudioDeviceSettings, type BufferFrames } from './audio-settings';
import { notifyError } from '../notify';
import { monitorArmed, refreshMonitorLatency } from './native-io';

/**
 * OWNS: the process-wide native audio configuration the Audio Settings panel edits — the enumerated cpal
 * capture/output devices (+ pruning of a persisted id that vanished), the global RT buffer size, and
 * the ASIO tier preference. None of it is per-slot, so nothing here rides the per-slot op chain.
 * Host writes are serialized here and published/persisted only after acknowledgement.
 * Everything is a no-op / empty / persisted-default in the browser build.
 */

// Native capture devices the host enumerates.
const [inputDevices, setInputDevices] = createSignal<AudioInputDevice[]>([]);
// Native cpal OUTPUT devices the host enumerates (the monitor picker).
const [outputDevices, setOutputDevices] = createSignal<AudioOutputDevice[]>([]);

// Global RT buffer size: the chosen block in frames, init from the persisted setting. ONE
// process-wide value (not per-slot) — the native host re-paces both producer loops on change.
const [bufferFrames, setBufferFramesSig] = createSignal<BufferFrames>(
  readAudioDeviceSettings().bufferFrames,
);

// ASIO low-latency tier: whether the native build offers an ASIO device (drives the Audio Settings
// toggle's enabled state) and whether the tier is preferred (init from persisted; pushed to the host
// at startup so a saved "off" is honored).
const [asioAvailable, setAsioAvailableSig] = createSignal(false);
const [asioDeviceInfo, setAsioDeviceInfo] = createSignal<Awaited<ReturnType<typeof platform.pluginHost.asioDeviceInfo>>>(null);
const [asioEnabled, setAsioEnabledSig] = createSignal<boolean>(readAudioDeviceSettings().asioEnabled);
export const usingAsio = () => asioAvailable() && asioEnabled();

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
 * Refresh the list of native cpal OUTPUT devices for the monitor picker (no-op / empty in the browser
 * build). Cheap native enumeration; the monitor device picker calls this on mount.
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
 * device instead of failing on a stale id. Called once at startup — so pruning happens before the user
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
  }
  if (
    outputRefreshSucceeded &&
    s.outputDeviceId &&
    !outputDevices().some((d) => d.id === s.outputDeviceId)
  ) {
    writeAudioDeviceSettings({ outputDeviceId: '' });
  }
}

// ---------------------------------------------------------------------------
// Global plugin processing block; device streams retain the driver's buffer choice.
// ---------------------------------------------------------------------------

/**
 * Set the global RT buffer size. Serialize host writes so rapid choices cannot finish in reverse
 * order. Publish and persist only accepted values; a rejected write must not reset compensation.
 */
export async function setBufferSize(frames: BufferFrames): Promise<void> {
  try {
    await configure(async () => {
      await platform.pluginHost.setBufferSize(frames);
      setBufferFramesSig(frames);
      writeAudioDeviceSettings({ bufferFrames: frames });
      await refreshBufferLatency();
    });
  } catch (e) {
    console.error('[instrument] setBufferSize failed', e);
    notifyError('Buffer size change failed', e);
  }
}

async function refreshBufferLatency(): Promise<void> {
  // The block change starts a new compensation generation immediately for every armed monitor; its delayed
  // fetch may settle cpal_out only until the first take freezes that generation.
  await Promise.all(
    monitorArmed().map((armed, slot) =>
      armed ? refreshMonitorLatency(slot as 0 | 1, 'generation') : Promise.resolve(),
    ),
  );
}

// ---------------------------------------------------------------------------
// ASIO low-latency tier — runtime host preference
// ---------------------------------------------------------------------------

/**
 * Set the ASIO-tier preference after host acknowledgement. Existing streams retain their driver.
 * Uses the same process-wide queue as buffer writes; the web host accepts without opening a device.
 */
export async function setAsioEnabled(enabled: boolean): Promise<void> {
  try {
    await configure(async () => {
      await platform.pluginHost.setAsioEnabled(enabled);
      setAsioEnabledSig(enabled);
      writeAudioDeviceSettings({ asioEnabled: enabled });
    });
  } catch (e) {
    console.error('[instrument] setAsioEnabled failed', e);
    notifyError("Couldn't switch audio driver (ASIO/WASAPI)", e);
  }
}

/**
 * Apply saved buffer and driver settings before publishing plugins. Configuration failure rejects
 * boot so a selectable plugin cannot silently use different settings. Availability is informational.
 */
export async function initAudioDeviceSettings(): Promise<void> {
  if (!platform.pluginHost.available) return;
  await configure(async () => {
    const saved = readAudioDeviceSettings();
    await platform.pluginHost.setBufferSize(saved.bufferFrames);
    setBufferFramesSig(saved.bufferFrames);
    await platform.pluginHost.setAsioEnabled(saved.asioEnabled);
    setAsioEnabledSig(saved.asioEnabled);
  });
  try {
    setAsioAvailableSig(await platform.pluginHost.asioAvailable());
    setAsioDeviceInfo(await platform.pluginHost.asioDeviceInfo());
  } catch (e) {
    console.error('[instrument] ASIO availability query failed', e);
    notifyError('Could not check ASIO availability', e);
  }
}

/** Read-only reactive accessors: native capture + output devices. */
export { inputDevices, outputDevices };

/** Read-only reactive accessor: the global RT buffer size (frames) for the settings readout. */
export { bufferFrames };

/** Read-only reactive accessors: ASIO tier availability + preference (the settings toggle). */
export { asioAvailable, asioEnabled, asioDeviceInfo };
