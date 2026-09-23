/**
 * OWNS: per-track mixer state — volume (0..1.5), mute, and the FX-state edits (bypass / param) applied to
 * the live chain. Store-and-apply: values live on the Track record and reach the lazy gain/FX nodes when
 * they exist.
 */
import { engine } from '../engine';
import type { FxState } from '../fx/fx';
import { engineState, fxVersion, muteSignals, volumeSignals } from './state';

// ── Per-track mixer (mute + volume) ──────────────────────────────────────────────────────
/** Push the effective gain (muted ? 0 : volume) to the live node with a short click-free ramp. */
export function applyTrackGain(i: number): void {
  const t = engineState.tracks[i];
  if (!t?.gain) return; // node is lazy; value is applied on creation in startPlayback
  const target = t.muted ? 0 : t.volume;
  t.gain.gain.setTargetAtTime(target, engine.ctx.currentTime, 0.01);
}

/** Set track `i` output volume (0..1.5). Stored; applied live if the gain node exists. */
export function setVolume(i: number, v: number): void {
  const t = engineState.tracks[i];
  if (!t) return;
  t.volume = Math.max(0, Math.min(1.5, v));
  applyTrackGain(i);
  volumeSignals[i][1](t.volume);
}

/** Mute/unmute track `i` in-sync (effective gain 0; the loop keeps running). */
export function setMute(i: number, on: boolean): void {
  const t = engineState.tracks[i];
  if (!t) return;
  t.muted = on;
  applyTrackGain(i);
  muteSignals[i][1](on);
}

/** Reactive current volume of track `i` (0..1.5). */
export function trackVolume(i: number): number {
  return volumeSignals[i]?.[0]() ?? 1;
}

/** Reactive current mute state of track `i`. */
export function trackMuted(i: number): boolean {
  return muteSignals[i]?.[0]() ?? false;
}

// ── Per-track FX control ──────────────────────────────────────────────────────────────────
/** Reactive per-track FX state array (five entries, chain order). Empty before init. */
export function fxState(i: number): FxState[] {
  fxVersion[i]?.[0](); // subscribe to FX edits
  return engineState.tracks[i]?.fxState ?? [];
}

/** Toggle bypass for FX `fxIndex` of track `i` (click-free; applied live if the chain exists). */
export function setFxBypass(i: number, fxIndex: number, bypassed: boolean): void {
  const t = engineState.tracks[i];
  if (!t || !t.fxState[fxIndex]) return;
  t.fxState[fxIndex] = { ...t.fxState[fxIndex], bypassed };
  t.fx?.nodes[fxIndex]?.setBypass(bypassed);
  fxVersion[i][1]((v) => v + 1);
}

/** Set param `key` for FX `fxIndex` of track `i` (applied live if the chain exists). */
export function setFxParam(i: number, fxIndex: number, key: string, value: number): void {
  const t = engineState.tracks[i];
  if (!t || !t.fxState[fxIndex]) return;
  const node = t.fx?.nodes[fxIndex];
  node?.setParam(key, value);
  // Store the value the node actually applied (it may clamp, e.g. delay feedback).
  const applied = node ? node.getParam(key) : value;
  const prev = t.fxState[fxIndex];
  t.fxState[fxIndex] = { ...prev, params: { ...prev.params, [key]: applied } };
  fxVersion[i][1]((v) => v + 1);
}
