// src/audio/export/import.ts
// SESSION-IMPORT coordinator: the read-back mate of export.ts. Takes the bytes of an exported
// BleepLoop .zip, finds + validates its session.json, decodes each referenced stem WAV, and hands
// the assembled payload to looper.loadSession (the grid-critical core, which re-validates its own
// preconditions and establishes the master grid). Like export.ts this coordinator THROWS and never
// toasts — the UI catches + notifies. Unlike export.ts, the one-download-per-gesture constraint does
// NOT apply here: import consumes bytes the UI hands in (file input / drag-drop), no downloads.
//
// DELIBERATE v0 CONSTRAINT — no resampling: a session exported at a different sample rate than the
// running engine is rejected with a friendly error naming both rates. Loading 44.1k PCM into a 48k
// context (or vice versa) would silently detune + shift tempo AND break the integer-frame grid math.
//
// The session.json schema + validateSession live in the PURE session-schema.ts (no engine/looper/Web
// Audio) so verify/fs-import-verify.mjs can import the validator under Node. This coordinator is the
// browser-only half, so it imports engine/looper STATICALLY (they're statically imported app-wide
// anyway — a lazy import() here bought nothing but two INEFFECTIVE_DYNAMIC_IMPORT build warnings).
import { engine } from '../engine';
import { looper } from '../looper/looper';
import { MAX_LOOP_SECONDS, TRACK_COUNT } from '../looper/state';
import { validateSession } from './session-schema.ts';
import { parseZip } from './unzip.ts';
import { decodeWav } from './wav.ts';

const ZIP_OVERHEAD_BYTES = 1 << 20;

/** Allow five Float32 editable stems plus a PCM16 stereo master and metadata. */
export function maxImportArchiveBytes(sampleRate: number): number {
  return (
    (TRACK_COUNT * Float32Array.BYTES_PER_ELEMENT + 2 * Int16Array.BYTES_PER_ELEMENT) *
      MAX_LOOP_SECONDS *
    sampleRate +
    ZIP_OVERHEAD_BYTES
  );
}

/**
 * Import a BleepLoop export archive into an all-EMPTY looper: parse the zip, validate its single
 * session.json, decode each stem WAV it references (found BY NAME via the per-track `file` field),
 * and hand the payload to looper.loadSession — which starts every track PLAYING on one shared grid
 * anchor. Throws a descriptive Error on any problem (the UI catches + notifies; nothing is mutated
 * unless every stem validated). Browser-only: this is the path that touches engine/looper.
 */
export async function importSession(bytes: Uint8Array | ArrayBuffer): Promise<void> {
  const u8 = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
  const limit = maxImportArchiveBytes(engine.ctx.sampleRate);
  if (u8.byteLength > limit) {
    throw new Error(`archive is ${u8.byteLength} bytes; maximum is ${limit}`);
  }
  const entries = parseZip(u8, TRACK_COUNT + 2); // Five stems, one master and one session.json.
  const byName = new Map<string, (typeof entries)[number]>();
  for (const entry of entries) {
    if (byName.has(entry.name)) {
      throw new Error(`ambiguous archive: duplicate entry "${entry.name}"`);
    }
    byName.set(entry.name, entry);
  }

  const sessions = entries.filter((e) => e.name.endsWith('-session.json'));
  if (sessions.length !== 1) {
    throw new Error(
      sessions.length === 0
        ? 'not a BleepLoop export: no *-session.json entry in the archive'
        : `ambiguous archive: ${sessions.length} *-session.json entries, expected exactly one`,
    );
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(new TextDecoder().decode(sessions[0].data));
  } catch (err) {
    throw new Error(`could not parse ${sessions[0].name}: ${err instanceof Error ? err.message : String(err)}`);
  }
  const session = validateSession(parsed);

  const engineRate = engine.ctx.sampleRate;
  if (session.sampleRate !== engineRate) {
    // Deliberate v0 constraint: no resampling (see the header comment).
    throw new Error(
      `This session was exported at ${session.sampleRate} Hz, but the audio engine is running at ` +
        `${engineRate} Hz. Import can't resample yet — load it on a setup running at ${session.sampleRate} Hz.`,
    );
  }

  const tracks = session.tracks.map((st) => {
    const entry = byName.get(st.file);
    if (!entry) throw new Error(`session.json lists "${st.file}" but the archive has no entry with that name`);
    const wav = decodeWav(entry.data);
    if (wav.channels.length !== 1) {
      throw new Error(`"${st.file}": expected a mono stem, got ${wav.channels.length} channels`);
    }
    if (wav.sampleRate !== session.sampleRate) {
      throw new Error(`"${st.file}": WAV sample rate ${wav.sampleRate} !== session sampleRate ${session.sampleRate}`);
    }
    if (wav.channels[0].length !== session.masterLengthFrames) {
      throw new Error(`"${st.file}": ${wav.channels[0].length} frames, expected masterLengthFrames ${session.masterLengthFrames}`);
    }
    return {
      index: st.track - 1,
      pcm: wav.channels[0],
      volume: st.volume,
      muted: st.muted,
      reversed: st.reversed,
      fx: st.fx,
    };
  });

  await looper.loadSession({
    bpm: session.bpm,
    bars: session.bars,
    masterLengthFrames: session.masterLengthFrames,
    tracks,
  });
}
