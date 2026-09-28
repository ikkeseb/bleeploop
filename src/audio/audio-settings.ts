/**
 * Persisted audio device selections — the native plugin-input device, the shared input channel
 * (native plugin GO LIVE + web MIC LIVE), and the monitor (cpal-out) output device. Survives a
 * reload/restart in BOTH the browser build and WebView2 (both have localStorage), so you don't re-pick
 * your interface every launch. These are global, last-used preferences. Best-effort — localStorage can
 * throw (private mode / disabled) → fall back to defaults.
 *
 * Mirrors the conventions in `master.ts` (try/catch around every localStorage call). NOT persisted:
 * armed state — auto-arming a hardware stream on launch is surprising (and the device may be gone),
 * so arming input / the monitor is always an explicit user action.
 */

const STORAGE_KEY = 'lf.audioDevices';

/** The selectable RT block sizes (frames). Smaller = lower latency, higher CPU/underrun risk. The
 * native host activates plugins with headroom above the max so the live control can sweep these
 * without a plugin reload. */
export const BUFFER_FRAMES_OPTIONS = [64, 128, 256, 512, 1024] as const;
export type BufferFrames = (typeof BUFFER_FRAMES_OPTIONS)[number];
export const DEFAULT_BUFFER_FRAMES: BufferFrames = 256;

export interface AudioDeviceSettings {
  /** Native cpal input device id; '' = default input. Not a getUserMedia MediaDeviceInfo.deviceId. */
  inputDeviceId: string;
  /** 0-based input channel shared by native plugin input and MIC LIVE; '' = auto-pick/sum. */
  inputChannel: string;
  /** cpal output (monitor) device id; '' = default output. */
  outputDeviceId: string;
  /** RT block size in frames; one of BUFFER_FRAMES_OPTIONS (live buffer control). */
  bufferFrames: BufferFrames;
  /** Prefer the ASIO low-latency tier (vs WASAPI-shared). Default true; ignored
   * unless the native build offers ASIO and a device is present. Applies on the next arm. */
  asioEnabled: boolean;
  /** The ASIO driver to open, by its registered name; '' = automatic (the system's first). */
  asioDriver: string;
  /** Engine mode's Share output: the cpal output id the master is mirrored to; '' = off. */
  shareDeviceId: string;
}

const DEFAULTS: AudioDeviceSettings = {
  inputDeviceId: '',
  inputChannel: '',
  outputDeviceId: '',
  bufferFrames: DEFAULT_BUFFER_FRAMES,
  asioEnabled: true,
  asioDriver: '',
  shareDeviceId: '',
};

/** Read the persisted device settings, falling back to defaults for any missing/invalid field. */
export function readAudioDeviceSettings(): AudioDeviceSettings {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return { ...DEFAULTS };
    const p = JSON.parse(raw) as Partial<AudioDeviceSettings>;
    return {
      inputDeviceId: typeof p.inputDeviceId === 'string' ? p.inputDeviceId : '',
      inputChannel: typeof p.inputChannel === 'string' ? p.inputChannel : '',
      outputDeviceId: typeof p.outputDeviceId === 'string' ? p.outputDeviceId : '',
      bufferFrames: (BUFFER_FRAMES_OPTIONS as readonly number[]).includes(p.bufferFrames as number)
        ? (p.bufferFrames as BufferFrames)
        : DEFAULT_BUFFER_FRAMES,
      asioEnabled: typeof p.asioEnabled === 'boolean' ? p.asioEnabled : true,
      asioDriver: typeof p.asioDriver === 'string' ? p.asioDriver : '',
      shareDeviceId: typeof p.shareDeviceId === 'string' ? p.shareDeviceId : '',
    };
  } catch {
    return { ...DEFAULTS };
  }
}

/** Merge a partial update into the persisted device settings. Best-effort. */
export function writeAudioDeviceSettings(patch: Partial<AudioDeviceSettings>): void {
  try {
    const next = { ...readAudioDeviceSettings(), ...patch };
    localStorage.setItem(STORAGE_KEY, JSON.stringify(next));
  } catch {
    /* persistence is best-effort */
  }
}

/**
 * The buffer an ASIO open asks the driver for: `requested` when the driver takes it (`min..max`), else
 * the power of two inside the range nearest to it, else `min`. Mirrors `asio_block` in
 * `src-tauri/src/engine_io/transition.rs`, which decides it; this copy only names it before a device runs.
 */
export function asioBlock(requested: number, min: number, max: number): number {
  if (requested >= min && requested <= max) return requested;
  let best: number | null = null;
  for (let b = 1; b <= max; b *= 2) {
    if (b >= min && (best === null || Math.abs(b - requested) < Math.abs(best - requested))) best = b;
  }
  return best ?? min;
}

/**
 * The Buffer select under ASIO (engine mode): the sizes on offer, the one it shows, and whether the driver
 * takes one size only (set in its own control panel). On offer: `BUFFER_FRAMES_OPTIONS` inside the
 * driver's range, plus the block the device runs at (else would open at) when it is not one of them.
 * Shown: that block. `saved` stays the player's pick: a fallback never overwrites it, so a driver that
 * takes it again later gets it back. No range known: every option, the running block shown.
 */
export function asioBufferChoice(
  saved: number,
  range: { min: number; max: number } | null,
  running: number | null,
): { options: number[]; shown: number; fixed: boolean } {
  if (!range) {
    const shown = running ?? saved;
    const options: number[] = [...BUFFER_FRAMES_OPTIONS];
    return { options: options.includes(shown) ? options : [...options, shown].sort((a, b) => a - b), shown, fixed: false };
  }
  const shown = running ?? asioBlock(saved, range.min, range.max);
  const options: number[] = BUFFER_FRAMES_OPTIONS.filter((f) => f >= range.min && f <= range.max);
  if (!options.includes(shown)) options.push(shown);
  return { options: options.sort((a, b) => a - b), shown, fixed: range.min === range.max };
}
