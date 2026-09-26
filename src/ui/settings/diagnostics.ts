import {
  asioDeviceInfo,
  asioStatus,
  bufferFrames,
  inputDevices,
  outputDevices,
  usingAsio,
} from '../../audio/audio-devices';
import { readAudioDeviceSettings } from '../../audio/audio-settings';
import { slotIds, slotPlugins } from '../../audio/instrument';
import { engineMode, platform } from '../../platform';
import { notifyError, notifyInfo } from '../../notify';
import { sampleRate } from '../state/audio';
import { engineDevice } from '../state/engine-store';

/**
 * OWNS: what a tester's report carries — the build line Help's "About this build" shows, the plain-text
 * block its Copy diagnostics button puts on the clipboard, and Open log folder. The block names the
 * build, the audio path and device, the slots' plugins, the WebView and the release log's folder: no
 * file contents, nothing personal beyond what a device name carries (the log path reads
 * `%LOCALAPPDATA%`, never the user's profile). Read at click time, so it shows the device that runs now.
 */

/** "BleepLoop 0.1.0 · 1a2b3c4": package.json's version and the commit the bundle was built from. */
export const BUILD_LABEL = `BleepLoop ${__APP_VERSION__} · ${__APP_COMMIT__}`;

let logFolderPath: Promise<string | null> | undefined;

/** The release log's folder, asked once (it never moves while the app runs); null in the browser build
 * or when the host could not say. */
export function logFolder(): Promise<string | null> {
  logFolderPath ??= platform.logs.available
    ? platform.logs.path().catch((err: unknown) => {
        console.error('[help] log folder path failed', err);
        return null;
      })
    : Promise.resolve(null);
  return logFolderPath;
}

function slotLine(slot: 0 | 1): string {
  const plugin = slotPlugins()[slot];
  const what = plugin ? `${plugin.name} (${plugin.format.toUpperCase()})` : `built-in synth (${slotIds()[slot]})`;
  return `Slot ${slot === 0 ? 'A' : 'B'}: ${what}`;
}

/** Engine mode reads the device that runs; the web path reads the saved picks its next GO LIVE opens. */
function audioLines(): string[] {
  const saved = readAudioDeviceSettings();
  const channelLine = `Input channel: ${saved.inputChannel === '' ? 'auto' : Number(saved.inputChannel) + 1}`;
  if (engineMode()) {
    const d = engineDevice();
    if (!d) return ['Audio: native engine', 'Device: none open', channelLine];
    return [
      'Audio: native engine',
      `Backend: ${d.backend === 'Asio' ? 'ASIO' : 'WASAPI'}`,
      `Device: ${d.inputOpen ? d.inputName : 'no input'} → ${d.outputName}`,
      `Sample rate: ${d.sampleRate} Hz`,
      `Buffer: ${d.block} frames`,
      channelLine,
    ];
  }
  if (!platform.pluginHost.available) return ['Audio: web path (browser build)', `Sample rate: ${sampleRate()} Hz`];
  const named = (list: readonly { id: string; name: string }[], id: string): string =>
    id === '' ? 'system default' : (list.find((d) => d.id === id)?.name ?? id);
  const device = usingAsio()
    ? (asioDeviceInfo()?.name ?? 'ASIO driver')
    : `${named(inputDevices(), saved.inputDeviceId)} → ${named(outputDevices(), saved.outputDeviceId)}`;
  return [
    'Audio: web path',
    `Backend: ${usingAsio() ? 'ASIO' : 'WASAPI'}`,
    `Device: ${device}`,
    `Sample rate: ${sampleRate()} Hz`,
    `Buffer: ${bufferFrames()} frames`,
    channelLine,
  ];
}

/** The block Copy diagnostics puts on the clipboard. `logDir` null leaves the log line out. */
function diagnosticsText(logDir: string | null): string {
  const asio = asioStatus();
  const lines = [
    `${BUILD_LABEL} (${import.meta.env.DEV ? 'dev' : 'release'} build)`,
    `App: ${platform.kind === 'tauri' ? 'Windows app' : 'browser build'}`,
    ...audioLines(),
    ...(asio.status === 'not-compiled' ? [] : [`ASIO status: ${asio.status}${asio.detail ? ` (${asio.detail})` : ''}`]),
    slotLine(0),
    slotLine(1),
    `OS/WebView: ${navigator.userAgent}`,
  ];
  if (logDir) lines.push(`Log folder: ${logDir}`);
  return lines.join('\n');
}

/** Copy diagnostics: the block to the clipboard, then a toast either way. */
export async function copyDiagnostics(): Promise<void> {
  try {
    await navigator.clipboard.writeText(diagnosticsText(await logFolder()));
    notifyInfo('Diagnostics copied');
  } catch (err) {
    console.error('[help] copy diagnostics failed', err);
    notifyError("Couldn't copy the diagnostics", err);
  }
}

/** Open log folder: Explorer on the release log's folder (the Windows app only). */
export function openLogFolder(): void {
  platform.logs.open().catch((err: unknown) => {
    console.error('[help] open log folder failed', err);
    notifyError("Couldn't open the log folder", err);
  });
}
