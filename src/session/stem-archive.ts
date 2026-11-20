// Pure archive assembly shared by downloads and the recovery worker. No live audio imports; explicit
// .ts: Node guards import this file.
import type { ExportSnapshot, TrackState } from '../ui/state/looper-types';
import { framesPerBar } from '../ui/state/quantize.ts';
import { encodeWav } from './wav.ts';
import type { ZipEntry } from './zip';

export interface StemSnapshot extends Omit<ExportSnapshot, 'tracks'> {
  /** `dubFeedback`: the lane's DUB FEEDBACK (0..1); saved as 1 when absent. */
  tracks: (ExportSnapshot['tracks'][number] & { state: TrackState; dubFeedback?: number })[];
  /** The engine's wet master of these tracks, frame 0 at loop position 0 (an export's snapshot). */
  master?: { left: Float32Array; right: Float32Array };
  /** Why a snapshot asked with the master carries none: the engine's render error. */
  masterError?: string;
}

/** The bars `snap`'s loop spans in its meter (not rounded: a recovery refuses a loop of part bars, an
 * export rounds it). */
export function snapshotBars(snap: Pick<ExportSnapshot, 'bpm' | 'meter' | 'sampleRate' | 'masterLengthFrames'>): number {
  return snap.masterLengthFrames / framesPerBar(snap.bpm, snap.sampleRate, snap.meter.beats);
}

/** Local timestamp yyyy-MM-dd-HHmm for the archive and its entries. */
export function exportBase(d = new Date()): string {
  const p = (n: number) => String(n).padStart(2, '0');
  return `bleeploop-${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}-${p(d.getHours())}${p(d.getMinutes())}`;
}

export function prepareStemArchive(
  snap: StemSnapshot,
  meta: { bpm: number; bars: number },
  base: string,
  format: 'pcm16' | 'float32' = 'pcm16',
) {
  const entries: ZipEntry[] = [];
  const tracks = snap.tracks.map((t) => {
    const track = t.index + 1;
    const file = `${base}-track${track}.wav`;
    // Both sides, as the engine holds them: a stereo stem (never their mean, which saves `[x, -x]` as
    // silence).
    entries.push({ name: file, data: encodeWav(t.pcm, snap.sampleRate, format) });
    return {
      track, file, channels: 2, volume: t.volume, muted: t.muted, reversed: t.reversed,
      frames: t.pcm[0].length, state: t.state, fx: t.fx, dubFeedback: t.dubFeedback ?? 1, pan: t.pan ?? 0,
    };
  });
  const { num, den, beats } = snap.meter;
  return {
    entries,
    session: {
      app: 'BleepLoop',
      // Missing versions remain legacy v1; increment only for incompatible schema changes. Format 3
      // (`docs/unit-contract.md` §8) holds stereo stems and names its meter, whatever it is, so every
      // older build refuses it as newer instead of reading two channels as one.
      formatVersion: 3,
      exported: new Date().toISOString(),
      bpm: meta.bpm,
      meter: { num, den, beats },
      bars: meta.bars,
      masterLengthFrames: snap.masterLengthFrames,
      sampleRate: snap.sampleRate,
      tracks,
      // `rig` goes here with the project files (`docs/unit-contract.md` §8: a recovery may leave it out).
    },
  };
}
