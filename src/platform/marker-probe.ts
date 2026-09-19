import { invoke, isTauri } from '@tauri-apps/api/core';

/** DEV commands return timings only. Native captured PCM stays inside Rust. */
function command<T>(name: string, args?: Record<string, unknown>): Promise<T> {
  if (!import.meta.env.DEV || !isTauri()) return Promise.reject(Error('Marker probe requires a native DEV app'));
  return invoke<T>(name, args);
}

export const markerProbeNative = {
  clock: () => command<number>('marker_probe_clock'),
  begin: (slot: number) => command<void>('marker_probe_begin', { slot }),
  cancel: () => command<void>('marker_probe_cancel'),
  result: () => command<{ frame: number; score: number; presentationMs: number;
    injectionTimeMs: number; injectionOffsetFrames: number; injectionSampleRate: number;
    callbackEntryTimeMs: number; reportedDriverDelayMs: number; callbackOffsetFrames: number;
    nativeSampleRate: number }[]>('marker_probe_result'),
  report: (report: unknown) => command<void>('diag', { report: JSON.stringify({ markerProbe: report }) }),
};
