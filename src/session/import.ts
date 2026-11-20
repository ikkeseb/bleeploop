// src/session/import.ts
// SESSION-IMPORT coordinator: the read-back mate of export.ts. Takes the bytes of an exported
// BleepLoop .zip, finds + validates its session.json, decodes each referenced stem WAV, and hands
// the assembled payload to the source's loadSession (the engine, which re-validates its own
// preconditions and establishes the master grid). The slot tones the session carries are validated
// with it and handed back, for the caller to store once the loops are in
// (`restoreSessionTones`, `src/ui/state/slot-tones.ts`). Like export.ts this coordinator THROWS and never
// toasts — the UI catches + notifies. Unlike export.ts, the one-download-per-gesture constraint does
// NOT apply here: import consumes bytes the UI hands in (file input / drag-drop), no downloads.
//
// DELIBERATE v0 CONSTRAINT — no resampling: a session exported at a different sample rate than the
// running engine is rejected with a friendly error naming both rates. Loading 44.1k PCM into a 48k
// context (or vice versa) would silently detune + shift tempo AND break the integer-frame grid math.
//
// The session.json schema + validateSession live in the PURE session-schema.ts (no engine/looper/Web
// Audio) so verify/guards/import.mjs can import the validator under Node. This coordinator is the
// browser-only half.
import { TIME_SIGNATURES } from '../platform';
import type { SlotTone } from '../ui/state/slot-tones';
import type { SessionSource } from './session-source';
import { MAX_ARCHIVE_ENTRIES, MAX_TONE_BYTES, maxImportArchiveBytes } from './archive-limits.ts';
import { validateSession, validateSessionPlugins } from './session-schema.ts';
import { parseZip } from './unzip.ts';
import { decodeWav } from './wav.ts';

/**
 * Import a BleepLoop export archive into an all-EMPTY looper: parse the zip, validate its single
 * session.json, decode each stem WAV it references (found BY NAME via the per-track `file` field),
 * and hand the payload to looper.loadSession — which restores PLAYING tracks on one shared grid anchor
 * while preserving STOPPED tracks without starting sources. Throws a descriptive Error on any problem
 * (the UI catches + notifies; nothing is mutated unless every stem validated). Browser-only: this is
 * the path that touches engine/looper. `source` is the looper to load into (`session-source.ts`).
 * Resolves with the slot tones the session carries (none for an older export or a recovery), checked
 * against session.json but not yet stored anywhere: the caller hands them to `restoreSessionTones`.
 */
export async function importSession(bytes: Uint8Array | ArrayBuffer, source: SessionSource): Promise<SlotTone[]> {
  const u8 = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
  const limit = maxImportArchiveBytes(source.sampleRate());
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
  let parsed: unknown;
  try {
    parsed = JSON.parse(new TextDecoder().decode(sessions[0].data));
  } catch (err) {
    throw new Error(`could not parse ${sessions[0].name}: ${err instanceof Error ? err.message : String(err)}`);
  }
  const session = validateSession(parsed, TIME_SIGNATURES);
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

  const engineRate = source.sampleRate();
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
    const wav = decodeWav(entry.data);
    if (wav.channels.length !== st.channels) {
      throw new Error(`"${st.file}": expected a ${st.channels === 2 ? 'stereo' : 'mono'} stem, got ${wav.channels.length} channels`);
    }
    if (wav.sampleRate !== session.sampleRate) {
      throw new Error(`"${st.file}": WAV sample rate ${wav.sampleRate} !== session sampleRate ${session.sampleRate}`);
    }
    if (wav.channels[0].length !== session.masterLengthFrames) {
      throw new Error(`"${st.file}": ${wav.channels[0].length} frames, expected masterLengthFrames ${session.masterLengthFrames}`);
    }
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

  await source.loadSession({
    bpm: session.bpm,
    meter: session.meter,
    bars: session.bars,
    masterLengthFrames: session.masterLengthFrames,
    tracks,
  });
  return tones;
}
