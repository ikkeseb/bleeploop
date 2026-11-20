// src/session/archive-read.ts
// The session import's reading half, PURE (explicit .ts imports, no engine, looper or platform runtime):
// an exported BleepLoop .zip's bytes to the payload the engine loads and the slot tones it carries, every
// limit and check applied first. `import.ts` hands the payload to the looper; `verify/guards/import.mjs`
// runs this under Node.
//
// DELIBERATE v0 CONSTRAINT — no resampling: a session exported at a different sample rate than the
// running engine is rejected with a friendly error naming both rates. Loading 44.1k PCM into a 48k
// context (or vice versa) would silently detune + shift tempo AND break the integer-frame grid math.
import type { TimeSignature } from '../platform';
import type { SlotTone } from '../ui/state/slot-tones';
import type { LoadSessionPayload } from '../ui/state/looper-types';
import { MAX_ARCHIVE_ENTRIES, MAX_SESSION_JSON_BYTES, MAX_TONE_BYTES, maxImportArchiveBytes } from './archive-limits.ts';
import { validateSession, validateSessionPlugins } from './session-schema.ts';
import { parseZip } from './unzip.ts';
import { decodeWav, readWavHeader } from './wav.ts';

/**
 * Read a BleepLoop export archive for an engine running at `engineRate` that offers `meters`: parse the
 * zip, validate its single session.json, decode each stem WAV it references (found BY NAME via the
 * per-track `file` field) once its header holds the dimensions session.json gives it, and check the slot
 * tones it carries against session.json. Throws a descriptive Error on any problem; nothing is decoded
 * past a check that failed. Returns the payload for `loadSession` and the tones, not yet stored anywhere.
 */
export function readSessionArchive(
  bytes: Uint8Array | ArrayBuffer,
  engineRate: number,
  meters: readonly TimeSignature[],
): { payload: LoadSessionPayload; tones: SlotTone[] } {
  const u8 = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
  const limit = maxImportArchiveBytes(engineRate);
  if (u8.byteLength > limit) {
    throw new Error(`archive is ${u8.byteLength} bytes; maximum is ${limit}`);
  }
  const entries = parseZip(u8, MAX_ARCHIVE_ENTRIES);
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
  // Bounded before it is decoded: the archive limit carries the tones' allowance, which must not fund a
  // parse of session.json.
  if (sessions[0].data.byteLength > MAX_SESSION_JSON_BYTES) {
    throw new Error(`${sessions[0].name} is ${sessions[0].data.byteLength} bytes; the largest session.json is ${MAX_SESSION_JSON_BYTES}`);
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(new TextDecoder().decode(sessions[0].data));
  } catch (err) {
    throw new Error(`could not parse ${sessions[0].name}: ${err instanceof Error ? err.message : String(err)}`);
  }
  const session = validateSession(parsed, meters);
  const tones: SlotTone[] = validateSessionPlugins(parsed, session.tracks.map((t) => t.file)).map((p) => {
    const entry = byName.get(p.file);
    if (!entry) throw new Error(`session.json lists "${p.file}" but the archive has no entry with that name`);
    // Checked here, before anything reaches the host: the archive limit alone would pass one tone the
    // size the stems are allowed.
    if (entry.data.byteLength > MAX_TONE_BYTES) {
      throw new Error(`"${p.file}": a tone of ${entry.data.byteLength} bytes; the largest a tone can be is ${MAX_TONE_BYTES}`);
    }
    const { slot, format, path, id, name } = p;
    return { slot, plugin: { format, path, id, name }, bytes: entry.data };
  });

  if (session.sampleRate !== engineRate) {
    // Deliberate v0 constraint: no resampling (see the header comment).
    throw new Error(
      `This session was exported at ${session.sampleRate} Hz and the engine runs at ${engineRate} Hz. ` +
        `Open it on a setup running at ${session.sampleRate} Hz.`,
    );
  }

  const tracks = session.tracks.map((st) => {
    const entry = byName.get(st.file);
    if (!entry) throw new Error(`session.json lists "${st.file}" but the archive has no entry with that name`);
    // The stem's dimensions from its header, before a sample is allocated (the grid is at most 60 s).
    const head = readWavHeader(entry.data);
    if (head.channels !== st.channels) {
      throw new Error(`"${st.file}": expected a ${st.channels === 2 ? 'stereo' : 'mono'} stem, got ${head.channels} channels`);
    }
    if (session.formatVersion >= 3 && !head.float) {
      throw new Error(`"${st.file}": a format v3 stem is Float32, and this one is PCM16`);
    }
    if (head.sampleRate !== session.sampleRate) {
      throw new Error(`"${st.file}": WAV sample rate ${head.sampleRate} !== session sampleRate ${session.sampleRate}`);
    }
    if (head.frames !== session.masterLengthFrames) {
      throw new Error(`"${st.file}": ${head.frames} frames, expected masterLengthFrames ${session.masterLengthFrames}`);
    }
    const wav = decodeWav(entry.data, head);
    return {
      index: st.track - 1,
      // A mono stem (v1, v2) goes as its one channel: the engine plays it on both sides.
      pcm: wav.channels,
      volume: st.volume,
      muted: st.muted,
      reversed: st.reversed,
      state: st.state,
      fx: st.fx,
      dubFeedback: st.dubFeedback,
      pan: st.pan,
    };
  });

  return {
    payload: {
      bpm: session.bpm,
      meter: session.meter,
      bars: session.bars,
      masterLengthFrames: session.masterLengthFrames,
      tracks,
    },
    tones,
  };
}
