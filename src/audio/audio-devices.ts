import { createSignal } from 'solid-js';
import {
  platform,
  type AsioStatusReport,
  type AudioInputDevice,
  type AudioOutputDevice,
} from '../platform';
import { readAudioDeviceSettings, writeAudioDeviceSettings, type BufferFrames } from './audio-settings';
import { notifyError } from '../notify';
import { monitorArmed, refreshMonitorLatency } from './native-io';
import { engine } from './engine';
import { outputDeviceChanged } from './record-latency';
import { matchWebOutput, outputLabelsHidden } from './output-match';

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
// at startup so a saved "off" is honored). The driver is contacted ONLY through `probeAsio` — at boot
// when the saved preference is on, or from an explicit user action — never by the native startup
// path (`src-tauri/src/asio_startup.rs` owns the one-per-process rules; `asioStatus` mirrors them).
const [asioAvailable, setAsioAvailableSig] = createSignal(false);
const [asioStatus, setAsioStatus] = createSignal<AsioStatusReport>({ status: 'not-compiled', detail: '' });
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
  await applyWebOutput();
}

// Chromium's AudioContext sink API (WebView2 has it); not yet in TypeScript's DOM lib.
type SinkContext = AudioContext & { readonly sinkId: string; setSinkId(sinkId: string): Promise<void> };

/**
 * Point the WebView's AudioContext (loops, synths, click) at the picked output, so one pick routes
 * everything, as in a DAW. The pick is a cpal id; the browser's device ids are its own, so the match is
 * by name (`output-match.ts`). The names need a mic grant, which a keys player who never armed a mic
 * lacks: a mic opened and closed at once gets it (auto-granted, `src-tauri/src/lib.rs`). Under ASIO the
 * picker is off and the WebView stays on the system default. No-op when the context is already there,
 * and in the web build.
 */
export async function applyWebOutput(): Promise<void> {
  if (!platform.pluginHost.available) return;
  const id = usingAsio() ? '' : readAudioDeviceSettings().outputDeviceId;
  const name = outputDevices().find((d) => d.id === id)?.name;
  const ctx = engine.ctx as SinkContext;
  try {
    let sinkId = '';
    if (name) {
      let devices = await navigator.mediaDevices.enumerateDevices();
      if (outputLabelsHidden(devices)) {
        const mic = await navigator.mediaDevices.getUserMedia({ audio: true });
        mic.getTracks().forEach((t) => t.stop());
        devices = await navigator.mediaDevices.enumerateDevices();
      }
      const match = matchWebOutput(name, devices);
      if (!match) throw new Error(`no browser output named like "${name}"`);
      sinkId = match;
    }
    if (ctx.sinkId === sinkId) return;
    await ctx.setSinkId(sinkId);
    outputDeviceChanged();
  } catch (e) {
    console.error('[instrument] web output switch failed', e);
    notifyError('Loops and synths stay on the previous output', e);
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
      report = await platform.pluginHost.asioProbe(explicit);
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

/** Whether the ASIO row should offer a control at all (the binary can do ASIO and it was not disabled at launch). */
export const asioOffered = () =>
  asioStatus().status !== 'not-compiled' && asioStatus().status !== 'disabled-by-flag';

/** Whether an explicit user retry can do anything (see `AsioStartupStatus`). */
export const asioRetryable = () => {
  const s = asioStatus().status;
  return s === 'unprobed' || s === 'failed' || s === 'blocked';
};

/**
 * Set the ASIO-tier preference after host acknowledgement. Existing streams retain their driver.
 * Uses the same process-wide queue as buffer writes; the web host accepts without opening a device.
 * Turning it ON when the driver has not been probed yet (or the last attempt failed / was blocked) is
 * the explicit user action that runs the probe; turning it OFF never touches the driver.
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
    return;
  }
  if (enabled && asioRetryable()) await probeAsio(true);
  await applyWebOutput();
}

/**
 * Apply saved buffer and driver settings before publishing plugins. Configuration failure rejects
 * boot so a selectable plugin cannot silently use different settings. The ASIO driver is probed here
 * — after the window is up — and only when the saved preference is on; a saved "off" never contacts
 * the driver. The probe is awaited (bounded by the native deadline) so `asioAvailable()` is fixed
 * before any plugin can load (the load-time block cap keys on it).
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
    const status = await platform.pluginHost.asioStatus();
    await applyAsioReport(status);
    const saved = readAudioDeviceSettings();
    if (saved.asioEnabled && status.status === 'unprobed') await probeAsio(false);
  } catch (e) {
    console.error('[instrument] ASIO status query failed', e);
    notifyError('Could not check ASIO availability', e);
  }
}

/** Read-only reactive accessors: native capture + output devices. */
export { inputDevices, outputDevices };

/** Read-only reactive accessor: the global RT buffer size (frames) for the settings readout. */
export { bufferFrames };

/** Read-only reactive accessors: ASIO tier availability, startup status + preference (the settings toggle). */
export { asioAvailable, asioEnabled, asioDeviceInfo, asioStatus };
