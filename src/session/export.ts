// src/session/export.ts
// WAV-export coordinator: pulls a read-only snapshot of the committed looper tracks and drops
// ONE .zip (per-track mono WAVs + a stereo master mix + session.json, and each loaded plugin slot's
// tone, `src/ui/state/slot-tones.ts`) via blob + <a download>. The master is the engine's: the same
// snapshot carries it, rendered offline by lf-engine from those loops and the engine's mix (no Web Audio
// render remains); a dry mixdown stands in when that render fails (`buildExportBundle`).
// Everything is bundled into a single archive on purpose: browsers (and WebView2) gate more than one
// programmatic download per user gesture, so firing a separate <a>.click() per file silently delivers
// only the first — one zip is one gesture, so the whole export always reaches the user (see zip.ts).
// Boundary-clean (no @tauri-apps/* import, no src/platform/ seam) so it builds and verifies on the Mac
// half; the actual file-drop behavior under WebView2 is a PC gate.
import { notifyError } from '../notify';
import type { SessionMasterKind } from './session-schema';
import type { SessionSource } from './session-source';
import { encodeWav, mixMono } from './wav';
import { makeZip } from './zip';
import { prepareStemArchive, exportBase } from './stem-archive';
import { slotLetter, takeSlotTones } from '../ui/state/slot-tones';
import { framesPerBar } from '../ui/state/quantize';

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
  /** False for recovery snapshots: stems + session.json only; the snapshot never asks the engine for
   *  its (expensive) master render. */
  includeMaster?: boolean;
  /** Float32 for lossless local recovery; downloads also preserve editable stem headroom. */
  stemFormat?: 'pcm16' | 'float32';
}

/**
 * Build the export bundle: every committed track as a mono WAV + session.json, optionally with the
 * stereo master mix, packed into a single timestamped .zip — everything exportLoops does EXCEPT the
 * download itself (the seam that lets a headless probe byte-parse the archive without touching
 * <a download>).
 * session.json's tempo and bars are the snapshot's own (the engine's device-frame clock is the
 * authority): a grid that changed while the export waited cannot pair with its loops. Per-track WAVs
 * are the RAW capture (unity, pre-volume/pre-mute/pre-limiter/pre-FX): EVERY committed track (incl.
 * STOPPED) exports its stem, so no audio is ever lost. When included, the master (session.json `master.kind`
 * 'wet-engine') is the engine's own: the snapshot asks for it, and lf-engine renders it offline from the
 * same loops with the mix the engine holds (lane volume, mute and FX, the reverb bus, master volume and
 * mute, the limiter: `src-tauri/crates/lf-engine/src/render.rs`), every track playing, frame 0 lined up
 * with the stems. A STOPPED track is IN the master (the owner exported a stopped session and got
 * silence, tester report F26); only MUTE leaves a track out. If the engine's render fails, the export
 * still completes with the DRY volume/mute dual-mono mixdown (master.kind 'dry-fallback') — a degraded
 * master beats a lost take. Returns null when nothing is committed.
 * An export with the master requires a finished take so its snapshot cannot contain an unfinished layer.
 * Recovery snapshots pass `includeMaster:false`: they never ask the engine for a master and remain
 * available during capture. `source` is the looper to read (`session-source.ts`).
 */
export async function buildExportBundle(
  source: SessionSource,
  options: BuildExportOptions = {},
): Promise<{ zipBytes: Uint8Array<ArrayBuffer>; base: string } | null> {
  if (options.includeMaster !== false) {
    for (let i = 0; i < source.trackCount; i++) {
      const state = source.stateOf(i);
      if (state === 'RECORDING' || state === 'OVERDUBBING') {
        throw new Error('Finish the active recording before exporting');
      }
    }
  }
  // Each track carries the state it had as its PCM was read (the engine reads both in one snapshot), so
  // they can't disagree; session.json keeps it for the import.
  const withMaster = options.includeMaster !== false;
  // The master level as the snapshot is asked for, as the lanes' mix is (`exportSnapshot`): the moment
  // the engine renders the master from, not after its render.
  const masterLevel = source.masterLevel();
  const snap = await source.exportSnapshot({ master: withMaster });
  if (snap.masterLengthFrames <= 0 || snap.tracks.length === 0) return null; // button should already guard this
  const base = exportBase();
  const sr = snap.sampleRate;
  const meta = { bpm: snap.bpm, bars: Math.max(1, Math.round(snap.masterLengthFrames / framesPerBar(snap.bpm, sr))) };
  const { entries, session } = prepareStemArchive(snap, meta, base, options.stemFormat ?? 'float32');

  if (withMaster) {
    // The engine's wet master; the dry mixdown as the fallback so one render failure can't lose the
    // whole export. Both mix every committed track, STOPPED included; mute and volume apply as heard.
    // Recovery snapshots skip this whole branch: their job is preserving editable stems, not a mix.
    let masterChannels: Float32Array[];
    let masterKind: Exclude<SessionMasterKind, 'wet-v1'>;
    if (snap.master) {
      masterChannels = [snap.master.left, snap.master.right];
      masterKind = 'wet-engine';
    } else {
      console.error('[export] the engine rendered no wet master — falling back to the dry mixdown', snap.masterError ?? 'no reason given');
      notifyError('Wet master render failed — exported a dry mixdown instead');
      const mono = mixMono(snap.tracks, snap.masterLengthFrames, masterLevel);
      masterChannels = [mono, mono];
      masterKind = 'dry-fallback';
    }
    const masterFile = `${base}-master.wav`;
    entries.push({ name: masterFile, data: encodeWav(masterChannels, sr) });
    Object.assign(session, {
      master: {
        file: masterFile,
        // 'wet-engine' or 'dry-fallback' (the render failed; see the error toast/log):
        // `SessionMasterKind` documents them.
        kind: masterKind,
        // Effective live master gain (mute is recorded as 0): in the wet master already; the fallback
        // applies it to its mixdown.
        level: masterLevel,
      },
    });
  }
  // Each loaded slot's tone, taken fresh (`slot-tones.ts`), as a tone file per slot, and
  // session.json names the plugin it belongs to (`validateSessionPlugins` reads it back).
  const plugins = (await takeSlotTones()).map(({ slot, plugin, bytes }) => {
    const file = `${base}-tone-slot-${slotLetter(slot).toLowerCase()}.bin`;
    entries.push({ name: file, data: bytes });
    return { slot: slotLetter(slot), ...plugin, file };
  });
  if (plugins.length > 0) Object.assign(session, { plugins });
  entries.push({ name: `${base}-session.json`, data: new TextEncoder().encode(JSON.stringify(session, null, 2)) });

  return { zipBytes: makeZip(entries), base };
}

/** Export = build the bundle + drop it as ONE .zip download (one file, one gesture — see header).
 * Returns the archive's filename so the caller can name it to the user, or null when nothing was
 * exported. The download is handed to the WebView; where it lands is the WebView's call. */
export async function exportLoops(source: SessionSource): Promise<string | null> {
  const bundle = await buildExportBundle(source);
  if (!bundle) return null; // nothing committed — button should already guard this
  const filename = `${bundle.base}.zip`;
  download(bundle.zipBytes, filename, 'application/zip');
  return filename;
}
