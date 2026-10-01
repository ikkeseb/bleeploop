/**
 * Persisted audio device selections — the engine's input and output device, each slot's input channel,
 * the buffer size, the ASIO tier and driver and Share output. Survives a
 * reload/restart in BOTH the browser build and WebView2 (both have localStorage), so you don't re-pick
 * your interface every launch. These are global, last-used preferences. Best-effort — localStorage can
 * throw (private mode / disabled) → fall back to defaults.
 *
 * try/catch around every localStorage call. NOT persisted: GO LIVE — going live on launch is
 * surprising (and the device may be gone), so it is always an explicit user action.
 */

const STORAGE_KEY = 'lf.audioDevices';

/** The selectable RT block sizes (frames). Smaller = lower latency, higher CPU/underrun risk. The
 * native host activates plugins with headroom above the max so the live control can sweep these
 * without a plugin reload. */
export const BUFFER_FRAMES_OPTIONS = [64, 128, 256, 512, 1024] as const;
export type BufferFrames = (typeof BUFFER_FRAMES_OPTIONS)[number];
export const DEFAULT_BUFFER_FRAMES: BufferFrames = 256;

/** The engine rates a player can pick (Hz); Rust `engine_io::SAMPLE_RATES`. */
export const SAMPLE_RATE_OPTIONS = [44100, 48000] as const;
export type SampleRate = (typeof SAMPLE_RATE_OPTIONS)[number];

export interface AudioDeviceSettings {
  /** Native cpal input device id; '' = default input. Not a getUserMedia MediaDeviceInfo.deviceId. */
  inputDeviceId: string;
  /** The pre-per-slot input channel (0-based; '' = auto): seeds `slotInputChannels` from old settings. */
  inputChannel: string;
  /** Each slot's 0-based capture channel, in the same form. Saved settings from before
   * the per-slot pick start both slots on `inputChannel`. */
  slotInputChannels: [string, string];
  /** Native output device id; '' = default output. */
  outputDeviceId: string;
  /** RT block size in frames; one of BUFFER_FRAMES_OPTIONS (live buffer control). */
  bufferFrames: BufferFrames;
  /** The engine's rate pick, one of SAMPLE_RATE_OPTIONS; null = the device's own. */
  sampleRate: SampleRate | null;
  /** Prefer the ASIO low-latency tier (vs WASAPI-shared). Default true; ignored
   * unless the native build offers ASIO and a device is present. Applies on the next arm. */
  asioEnabled: boolean;
  /** The ASIO driver to open, by its registered name; '' = automatic (the system's first). */
  asioDriver: string;
  /** Share output: the cpal output id the master is mirrored to; '' = off. */
  shareDeviceId: string;
}

/** A saved channel: '' (auto) or a 0-based index. */
const isChannel = (v: unknown): v is string => typeof v === 'string' && (v === '' || /^\d{1,3}$/.test(v));

const DEFAULTS: AudioDeviceSettings = {
  inputDeviceId: '',
  inputChannel: '',
  slotInputChannels: ['', ''],
  outputDeviceId: '',
  bufferFrames: DEFAULT_BUFFER_FRAMES,
  sampleRate: null,
  asioEnabled: true,
  asioDriver: '',
  shareDeviceId: '',
};

/** Read the persisted device settings, falling back to defaults for any missing/invalid field. */
export function readAudioDeviceSettings(): AudioDeviceSettings {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return { ...DEFAULTS, slotInputChannels: ['', ''] };
    const p = JSON.parse(raw) as Partial<AudioDeviceSettings>;
    const inputChannel = typeof p.inputChannel === 'string' ? p.inputChannel : '';
    const slots = p.slotInputChannels;
    const seed = isChannel(inputChannel) ? inputChannel : '';
    return {
      inputDeviceId: typeof p.inputDeviceId === 'string' ? p.inputDeviceId : '',
      inputChannel,
      slotInputChannels:
        Array.isArray(slots) && slots.length === 2 && slots.every(isChannel) ? [slots[0], slots[1]] : [seed, seed],
      outputDeviceId: typeof p.outputDeviceId === 'string' ? p.outputDeviceId : '',
      bufferFrames: (BUFFER_FRAMES_OPTIONS as readonly number[]).includes(p.bufferFrames as number)
        ? (p.bufferFrames as BufferFrames)
        : DEFAULT_BUFFER_FRAMES,
      sampleRate: (SAMPLE_RATE_OPTIONS as readonly number[]).includes(p.sampleRate as number) ? (p.sampleRate as SampleRate) : null,
      asioEnabled: typeof p.asioEnabled === 'boolean' ? p.asioEnabled : true,
      asioDriver: typeof p.asioDriver === 'string' ? p.asioDriver : '',
      shareDeviceId: typeof p.shareDeviceId === 'string' ? p.shareDeviceId : '',
    };
  } catch {
    return { ...DEFAULTS, slotInputChannels: ['', ''] };
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
 * The Buffer select under ASIO: the sizes on offer, the one it shows, and whether the driver
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

/**
 * The Sample rate select: the picks on offer (null = the device's own rate), the one it shows, and
 * whether the device sets the rate alone. `runs`: the pickable rates the device runs (under ASIO the
 * driver's, `AsioDeviceInfo.sampleRates`; under WASAPI none: the endpoint's own rate runs, set in
 * Windows' Sound settings). Shown: `saved` when the device runs it, else the device's own, which is
 * what an open runs (`transition::open_rate` in `src-tauri/src/engine_io/transition.rs`). `saved` stays
 * the player's pick, so a driver that runs it later gets it.
 */
export function rateChoice(
  saved: SampleRate | null,
  runs: readonly number[],
): { options: (SampleRate | null)[]; shown: SampleRate | null; fixed: boolean } {
  const offered = SAMPLE_RATE_OPTIONS.filter((r) => runs.includes(r));
  return { options: [null, ...offered], shown: saved !== null && offered.includes(saved) ? saved : null, fixed: offered.length === 0 };
}
