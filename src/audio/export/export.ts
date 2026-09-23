// src/audio/export/export.ts
// WAV-export coordinator: pulls a read-only snapshot of the committed looper tracks and drops
// ONE .zip (per-track mono WAVs + a stereo master mix + session.json) via blob + <a download>.
// Everything is bundled into a single archive on purpose: browsers (and WebView2) gate more than one
// programmatic download per user gesture, so firing a separate <a>.click() per file silently delivers
// only the first — one zip is one gesture, so the whole export always reaches the user (see zip.ts).
// Boundary-clean (no @tauri-apps/* import, no src/platform/ seam) so it builds and verifies on the Mac
// half; the actual file-drop behavior under WebView2 is a PC gate.
import { notifyError } from '../../notify';
import { looper } from '../looper/looper';
import { master } from '../master';
import { renderWetMaster } from './render';
import { encodeWav, mixMono } from './wav';
import { makeZip } from './zip';
import { prepareStemArchive, exportBase } from './stem-archive';

function download(bytes: Uint8Array<ArrayBuffer> | string, filename: string, mime: string): void {
  const blob = new Blob([bytes], { type: mime });
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  a.remove();
  // Revoke on the next tick so the click's navigation has consumed the URL.
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

export interface BuildExportOptions {
  /** False for recovery snapshots: stems + session.json only, with no expensive offline render. */
  includeMaster?: boolean;
  /** Float32 for lossless local recovery; downloads also preserve editable stem headroom. */
  stemFormat?: 'pcm16' | 'float32';
}

/**
 * Build the export bundle: every committed track as a mono WAV + session.json, optionally with the
 * stereo master mix, packed into a single timestamped .zip — everything exportLoops does EXCEPT the
 * download itself (the seam that lets a headless probe byte-parse the archive without touching
 * <a download>).
 * bpm/bars are passed in from the UI (the clock authority lives there). Per-track WAVs are the RAW
 * capture (unity, pre-volume/pre-mute/pre-limiter/pre-FX) — EVERY committed track (incl. STOPPED)
 * exports its stem, so no audio is ever lost. When included, the master (v1) is a WET stereo render:
 * each track through its real FX chain + the shared reverb + the master limiter in an
 * OfflineAudioContext (render.ts), i.e. what you hear. STOPPED tracks are therefore EXCLUDED from the
 * master (mute honoured in the mix as before). If that render fails, the export still completes with
 * the v0 DRY volume/mute
 * dual-mono mixdown (flagged in session.json as master.kind = 'dry-fallback') — a degraded master
 * beats a lost take. Returns null when nothing is committed; a normal all-STOPPED export still
 * includes a silent master alongside real stems.
 * A wet export requires a finished take so its snapshot cannot contain an unfinished layer.
 * Recovery snapshots pass `includeMaster:false` and remain available during capture.
 */
export async function buildExportBundle(
  meta: { bpm: number; bars: number },
  options: BuildExportOptions = {},
): Promise<{ zipBytes: Uint8Array<ArrayBuffer>; base: string } | null> {
  if (options.includeMaster !== false) {
    for (let i = 0; i < looper.trackCount; i++) {
      const state = looper.stateOf(i);
      if (state === 'RECORDING' || state === 'OVERDUBBING') {
        throw new Error('Finish the active recording before exporting');
      }
    }
  }
  const snap = looper.exportSnapshot();
  if (snap.masterLengthFrames <= 0 || snap.tracks.length === 0) return null; // button should already guard this
  const base = exportBase();
  const sr = snap.sampleRate;

  // Per-track live state, read synchronously with the snapshot (both are plain engineState reads on the
  // same tick, before any await, so they can't disagree). STOPPED tracks still export their raw stem but
  // are EXCLUDED from the master mix, so the exported master is exactly the audible mix. All-STOPPED is
  // allowed (masterTracks empty ⇒ silent master alongside real stems — nothing is lost).
  const withState = snap.tracks.map((t) => ({ t, state: looper.stateOf(t.index) }));
  const { entries, session } = prepareStemArchive(
    { ...snap, tracks: withState.map(({ t, state }) => ({ ...t, state })) },
    meta, base, options.stemFormat ?? 'float32',
  );

  if (options.includeMaster !== false) {
    // v1 wet master; v0 dry mixdown as the fallback so one render bug can't lose the whole export.
    // Both mix ONLY audible tracks (STOPPED excluded) so the master == what you hear. Recovery
    // snapshots skip this whole branch: their job is preserving editable stems, not rendering a mix.
    const masterTracks = withState.filter((x) => x.state !== 'STOPPED').map((x) => x.t);
    const masterLevel = master.muted() ? 0 : master.volume();
    let masterChannels: Float32Array[];
    let masterKind: 'wet-v1' | 'dry-fallback';
    try {
      const wet = await renderWetMaster({ ...snap, tracks: masterTracks }, meta.bpm, masterLevel);
      masterChannels = [wet.left, wet.right];
      masterKind = 'wet-v1';
    } catch (err) {
      console.error('[export] wet master render failed — falling back to the dry mixdown', err);
      notifyError('Wet master render failed — exported a dry mixdown instead');
      const mono = mixMono(masterTracks, snap.masterLengthFrames, masterLevel);
      masterChannels = [mono, mono];
      masterKind = 'dry-fallback';
    }
    const masterFile = `${base}-master.wav`;
    entries.push({ name: masterFile, data: encodeWav(masterChannels, sr) });
    Object.assign(session, {
      master: {
        file: masterFile,
        // 'wet-v1': stereo render through per-track FX + reverb + master limiter (as heard).
        // 'dry-fallback': v0 volume/mute dual-mono mixdown (render failed; see the error toast/log).
        kind: masterKind,
        // Effective live master gain applied before the limiter/clamp (mute is recorded as 0).
        level: masterLevel,
      },
    });
  }
  entries.push({ name: `${base}-session.json`, data: new TextEncoder().encode(JSON.stringify(session, null, 2)) });

  return { zipBytes: makeZip(entries), base };
}

/** Export = build the bundle + drop it as ONE .zip download (one file, one gesture — see header).
 * Returns the archive's filename so the caller can name it to the user, or null when nothing was
 * exported. The download is handed to the WebView; where it lands is the WebView's call. */
export async function exportLoops(meta: { bpm: number; bars: number }): Promise<string | null> {
  const bundle = await buildExportBundle(meta);
  if (!bundle) return null; // nothing committed — button should already guard this
  const filename = `${bundle.base}.zip`;
  download(bundle.zipBytes, filename, 'application/zip');
  return filename;
}
