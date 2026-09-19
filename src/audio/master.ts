import { createSignal } from 'solid-js';
import { notifyError } from '../notify';
import { platform } from '../platform';
import { engine } from './engine';
import { readStoredNumber, writeStoredNumber } from './persist';

/**
 * Master output level — the single user-facing volume on `engine.masterGain` (the node downstream of
 * the looper's record tap on looperInputBus, upstream of the safety limiter). The whole app summed to
 * one bus with no way to turn it down; this is that control. Persisted to localStorage so the level
 * survives a reload.
 *
 * Mute is layered on top (the DEV single-clock gate uses it via `__lf.setMasterMute`): the applied
 * gain is `muted ? 0 : volume`, so toggling mute restores the user's level instead of snapping to 1.
 *
 * Conventions mirror `clock.ts`: module-level Solid signals exposed as accessor (`volume`) + action
 * (`setVolume`). Gain changes use `setTargetAtTime` for a click-free ramp.
 */

const STORAGE_KEY = 'lf.masterVolume';
/** Keep the default at unity so first-run loudness is unchanged; the slider only lets you turn down. */
const DEFAULT_VOLUME = 1.0;
/** Ramp time constant for click-free level changes. */
const RAMP_TC = 0.012;

const [volume, setVolumeSignal] = createSignal(readStoredNumber(STORAGE_KEY, DEFAULT_VOLUME, 0, 1));
const [muted, setMutedSignal] = createSignal(false);

/** Drive both audible paths to the effective level (muted ? 0 : volume). */
function apply(): void {
  const g = engine.masterGain.gain;
  const target = muted() ? 0 : volume();
  g.setTargetAtTime(target, engine.ctx.currentTime, RAMP_TC);
  void platform.pluginHost.setMasterGain(target).catch((e) => {
    console.error('[master] set native master gain failed', e);
    notifyError('Master volume change failed', e);
  });
}

/** Sync the gain node to the current (possibly persisted) volume. Call once when the app mounts. */
function init(): void {
  apply();
}

/** Set master volume (0..1). Clamped, persisted, applied (unless currently muted, then on unmute). */
function setVolume(v: number): void {
  const clamped = Math.max(0, Math.min(1, v));
  setVolumeSignal(clamped);
  writeStoredNumber(STORAGE_KEY, clamped);
  apply();
}

/** Mute / unmute. Unmuting restores the user's volume (see module note). */
function setMuted(on: boolean): void {
  setMutedSignal(on);
  apply();
}

export const master = {
  /** Reactive: current master volume (0..1). */
  volume,
  /** Set master volume, clamped to [0, 1] and persisted. */
  setVolume,
  /** Reactive: whether master is muted. */
  muted,
  /** Mute / unmute (restores volume on unmute). */
  setMuted,
  /** Apply the persisted volume to the gain node — call once on app mount. */
  init,
} as const;
