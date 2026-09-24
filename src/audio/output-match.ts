/**
 * Find the WebView's own output for a picked cpal output. The two sides share no id, only a name:
 * cpal's is the Windows endpoint name plus ` [kind] via bus`; Chromium's label is the same endpoint name
 * plus ` (vid:pid)` for a USB-class device or ` (Bluetooth)`, and is '' until the session holds a mic
 * grant. Import-free, so `verify/guards/output-match.mjs` runs the real code.
 */

const CHROMIUM_SUFFIX = /\s*\((?:[0-9a-f]{4}:[0-9a-f]{4}|Bluetooth)\)$/;

type WebDevice = Pick<MediaDeviceInfo, 'kind' | 'deviceId' | 'label'>;

/** The browser deviceId whose endpoint name `name` carries; the longest label wins; undefined when none. */
export function matchWebOutput(name: string, devices: readonly WebDevice[]): string | undefined {
  return devices
    .filter((d) => d.kind === 'audiooutput' && d.deviceId !== 'default' && d.deviceId !== 'communications')
    .map((d) => ({ id: d.deviceId, label: d.label.replace(CHROMIUM_SUFFIX, '') }))
    .filter((d) => d.label && (name === d.label || name.startsWith(`${d.label} `)))
    .sort((a, b) => b.label.length - a.label.length)[0]?.id;
}

/** True when the list names no output: WebView2 withholds labels until the session has a mic grant. */
export function outputLabelsHidden(devices: readonly WebDevice[]): boolean {
  return !devices.some((d) => d.kind === 'audiooutput' && d.label);
}
