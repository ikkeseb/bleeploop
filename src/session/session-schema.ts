// src/session/session-schema.ts
// PURE session.json schema: validateSession + its parsed-shape types, with NO engine/looper/Web-Audio
// dependency. Split out of import.ts so verify/guards/import.mjs can import the REAL validator under
// Node (TS type-stripping) while import.ts is free to pull in engine/looper STATICALLY. Runtime imports
// here must stay PURE + Node-importable — no engine/looper/Tone/Tauri/Web Audio.
import { validateFxStates, type FxState } from '../ui/state/fx-metadata.ts';
import { framesPerBar } from '../ui/state/quantize.ts';
import { isPluginFormat } from '../ui/state/plugin-format.ts';
import type { PluginFormat, TimeSignature } from '../platform';

export type ParsedSessionTrackState = 'PLAYING' | 'STOPPED';

/** One validated session track (normalized: legacy fields normalized, fx deep-copied). */
export interface ParsedSessionTrack {
  /** 1-based track number (the session.json `track` field; engine index = track - 1). */
  track: number;
  /** Stem file name inside the archive (the lookup key for the WAV entry). */
  file: string;
  /** Clamped to [0, 1.5] (the mixer's range) — out-of-range values are clamped, not rejected. */
  volume: number;
  muted: boolean;
  /** Serialized orientation; missing legacy field normalizes to false. */
  reversed: boolean;
  /** Restored playback state. Missing/OVERDUBBING legacy values normalize to PLAYING. */
  state: ParsedSessionTrackState;
  /** Always === masterLengthFrames (validated). */
  frames: number;
  /** Exactly 5 entries, chain order filter,pitch,stutter,delay,reverb. */
  fx: FxState[];
  /** DUB FEEDBACK, clamped to [0, 1]; missing (every export before it) normalizes to 1, a plain sum. */
  dubFeedback: number;
  /** Pan, -1 (hard left) to 1 (hard right), clamped; missing (every export before it) normalizes to 0,
   * the centre, where the lane plays as it did before pan. */
  pan: number;
}
export interface ParsedSession {
  bpm: number;
  /** The meter `bars` count in, one of the offered list `validateSession` was handed; 4/4 for v1. */
  meter: TimeSignature;
  bars: number;
  masterLengthFrames: number;
  sampleRate: number;
  tracks: ParsedSessionTrack[];
}

/**
 * session.json's `master.kind`. An export's `master` is `{ file, kind, level }`: `file` the stereo PCM16
 * WAV entry, `level` the master gain as heard when exported (0 while muted); a recovery archive has no
 * master, and import reads none of it (the stems are the session).
 * - 'wet-engine': the engine's own render (lf-engine's `render`, asked through the export's snapshot):
 *   every track playing through its FX chain, the reverb bus, master volume and mute, the limiter, frame
 *   0 at loop position 0 like the stems.
 * - 'wet-v1': the same mix rendered on Tone.js in the WebView, by exports before the engine rendered it
 *   (v0.1.0's fixtures hold one); it lags the stems by the limiter's pre-delay.
 * - 'dry-fallback': the volume/mute/pan stereo mixdown (dual mono before pan), written when the wet
 *   render failed.
 */
export type SessionMasterKind = 'wet-engine' | 'wet-v1' | 'dry-fallback';

/** One plugin slot in session.json's optional `plugins` list: which plugin the
 * slot held, and the archive entry holding its tone (a tone file, `src-tauri/src/host/tone.rs`). */
export interface ParsedSessionPlugin {
  /** 0 = slot A, 1 = slot B (session.json writes the letter). */
  slot: 0 | 1;
  format: PluginFormat;
  path: string;
  id: string;
  name: string;
  /** The tone file's entry name inside the archive. */
  file: string;
}

/**
 * Validate session.json's optional `plugins` list: a session exported with plugins loaded names each
 * slot's plugin and its tone entry. Missing means none (an export from before tone recall, a recovery,
 * a v0.1.0 web-path export). PURE — safe under Node. Throws a descriptive Error on a list that is not an
 * array, an entry that is not an object, a slot other than "A"/"B" or repeated, a format other than
 * clap/vst3/vst2, an empty or non-string path, id, name or file, and a file a track or another slot uses.
 * `trackFiles` are the stems' entry names (validateSession's). Unknown keys are ignored.
 */
export function validateSessionPlugins(json: unknown, trackFiles: readonly string[] = []): ParsedSessionPlugin[] {
  const raw = (json as Record<string, unknown> | null)?.plugins;
  if (raw === undefined) return [];
  if (!Array.isArray(raw)) throw new Error('session.json: plugins must be an array');
  const files = new Set(trackFiles);
  const slots = new Set<number>();
  return raw.map((entry: unknown, i: number) => {
    if (typeof entry !== 'object' || entry === null) throw new Error(`session.json: plugins[${i}] is not an object`);
    const p = entry as Record<string, unknown>;
    if (p.slot !== 'A' && p.slot !== 'B') {
      throw new Error(`session.json: plugins[${i}].slot must be "A" or "B", got ${JSON.stringify(p.slot)}`);
    }
    const slot = p.slot === 'A' ? 0 : 1;
    if (slots.has(slot)) throw new Error(`session.json: slot ${p.slot} is listed twice in plugins`);
    slots.add(slot);
    if (!isPluginFormat(p.format)) {
      throw new Error(`session.json: plugins[${i}].format must be "clap", "vst3" or "vst2", got ${JSON.stringify(p.format)}`);
    }
    const text = (key: 'path' | 'id' | 'name' | 'file'): string => {
      const v = p[key];
      if (typeof v !== 'string' || v.length === 0) throw new Error(`session.json: plugins[${i}].${key} missing or not a string`);
      return v;
    };
    const file = text('file');
    if (files.has(file)) throw new Error(`session.json: plugins[${i}].file "${file}" is already another entry's`);
    files.add(file);
    return { slot, format: p.format, path: text('path'), id: text('id'), name: text('name'), file };
  });
}

/**
 * Validate + normalize a parsed session.json (the schema export.ts writes). PURE — safe under Node.
 * `meters` is the meters this build offers (`TIME_SIGNATURES`, handed in: the platform seam is not
 * Node-importable). The meter: format v1 (or none) is 4/4 and names no other meter; v2 names one of
 * `meters` with the `beats` this build counts it in (the BPM's unit: a file that read its tempo in
 * another unit is refused, never re-read), and the bars count in it.
 * Throws a descriptive Error on: a non-BleepLoop app tag, a formatVersion newer than this build
 * understands (or a garbage formatVersion), a meter as above that does not hold, missing/mis-typed
 * fields, a non-positive/non-integer masterLengthFrames, an empty or >5-track list, track numbers
 * outside 1..5 or repeated, a per-track frames that differs from masterLengthFrames, a repeated stem
 * file, or an fx array that doesn't exactly match the five-effect key/range contract. Two fields normalize instead of rejecting: volume
 * clamps into [0, 1.5]; muted accepts legacy 0/1 and missing-as-false; reversed is optional and
 * missing-as-false for legacy format-v1 exports; dubFeedback is optional, clamps into [0, 1] and reads
 * missing as 1; pan is optional, clamps into [-1, 1] and reads missing as 0 (the centre).
 * Per-track state preserves STOPPED; PLAYING stays
 * PLAYING; missing state and OVERDUBBING normalize to PLAYING because legacy archives omitted state
 * and an export taken during overdub contains only the last committed loop, not the unfinished layer.
 * Other states are rejected rather than reviving an impossible capture state.
 * Returns a fresh normalized object (never the input aliased). Unknown keys are ignored.
 */
export function validateSession(json: unknown, meters: readonly TimeSignature[]): ParsedSession {
  if (typeof json !== 'object' || json === null || Array.isArray(json)) {
    throw new Error('session.json: not a JSON object');
  }
  const s = json as Record<string, unknown>;
  // 'LoopForge' is the app's earlier name: exports written under it still import.
  if (s.app !== 'BleepLoop' && s.app !== 'LoopForge') {
    throw new Error(`session.json: app tag is ${JSON.stringify(s.app)}, expected "BleepLoop" — not a BleepLoop session`);
  }
  // formatVersion gate: MISSING is treated as legacy v1 (every pre-versioning export still imports);
  // 1 (4/4) and 2 (another meter) are this build's formats; an integer > 2 was written by a newer
  // BleepLoop; anything else is garbage. A non-integer/<1 value is rejected as invalid rather than
  // silently ignored.
  const fv = s.formatVersion === undefined ? 1 : s.formatVersion;
  if (typeof fv !== 'number' || !Number.isInteger(fv) || fv < 1) {
    throw new Error(`session.json: formatVersion must be a positive integer, got ${JSON.stringify(fv)}`);
  }
  if (fv > 2) {
    throw new Error(`This session was exported by a newer version of BleepLoop (format v${fv}) — update BleepLoop to open it.`);
  }
  const meter = sessionMeter(s.meter, fv, meters);
  const num = (key: string): number => {
    const v = s[key];
    if (typeof v !== 'number' || !Number.isFinite(v)) {
      throw new Error(`session.json: ${key} missing or not a finite number (got ${JSON.stringify(v)})`);
    }
    return v;
  };
  const bpm = num('bpm');
  // Mirror the engine's MIN/MAX_BPM (lf-engine `grid.rs`) instead of letting it silently clamp+round
  // a hand-edited value — a clamped bpm would recover the WRONG bar count from the frame math with no
  // error.
  if (!Number.isInteger(bpm) || bpm < 40 || bpm > 300) {
    throw new Error(`session.json: bpm must be an integer in 40..300 (the clock's range), got ${bpm}`);
  }
  const bars = num('bars');
  if (!Number.isInteger(bars) || bars < 1) {
    throw new Error(`session.json: bars must be a positive integer, got ${bars}`);
  }
  const sampleRate = num('sampleRate');
  if (!Number.isInteger(sampleRate) || sampleRate <= 0) {
    throw new Error(`session.json: sampleRate must be a positive integer, got ${sampleRate}`);
  }
  const master = num('masterLengthFrames');
  if (!Number.isInteger(master) || master <= 0) {
    throw new Error(`session.json: masterLengthFrames must be a positive integer, got ${master}`);
  }
  const expectedFrames = bars * framesPerBar(bpm, sampleRate, meter.beats);
  if (!Number.isSafeInteger(expectedFrames) || expectedFrames !== master) {
    throw new Error(
      `session.json: grid expects ${expectedFrames} frames from bpm/bars/sampleRate in ${meter.num}/${meter.den}, got masterLengthFrames ${master}`,
    );
  }
  const rawTracks = s.tracks;
  if (!Array.isArray(rawTracks)) throw new Error('session.json: tracks missing or not an array');
  if (rawTracks.length < 1 || rawTracks.length > 5) {
    throw new Error(`session.json: ${rawTracks.length} tracks (need 1..5)`);
  }
  const seen = new Set<number>();
  const seenFiles = new Set<string>();
  const tracks: ParsedSessionTrack[] = rawTracks.map((raw: unknown, i: number) => {
    if (typeof raw !== 'object' || raw === null) throw new Error(`session.json: tracks[${i}] is not an object`);
    const t = raw as Record<string, unknown>;
    const trackNo = t.track;
    if (typeof trackNo !== 'number' || !Number.isInteger(trackNo) || trackNo < 1 || trackNo > 5) {
      throw new Error(`session.json: tracks[${i}].track must be an integer 1..5, got ${JSON.stringify(trackNo)}`);
    }
    if (seen.has(trackNo)) throw new Error(`session.json: duplicate track number ${trackNo}`);
    seen.add(trackNo);
    if (typeof t.file !== 'string' || t.file.length === 0) {
      throw new Error(`session.json: track ${trackNo} file missing or not a string`);
    }
    if (seenFiles.has(t.file)) {
      throw new Error(`session.json: multiple tracks reference "${t.file}"`);
    }
    seenFiles.add(t.file);
    if (typeof t.frames !== 'number' || t.frames !== master) {
      throw new Error(`session.json: track ${trackNo} frames ${JSON.stringify(t.frames)} !== masterLengthFrames ${master}`);
    }
    if (typeof t.volume !== 'number' || !Number.isFinite(t.volume)) {
      throw new Error(`session.json: track ${trackNo} volume missing or not a number`);
    }
    const volume = Math.max(0, Math.min(1.5, t.volume)); // clamp, don't reject
    let muted = false;
    if (typeof t.muted === 'boolean') muted = t.muted;
    else if (t.muted === 0 || t.muted === 1) muted = Boolean(t.muted);
    else if (t.muted !== undefined) {
      throw new Error(`session.json: track ${trackNo} muted must be boolean or 0/1, got ${JSON.stringify(t.muted)}`);
    }
    let reversed = false;
    if (typeof t.reversed === 'boolean') reversed = t.reversed;
    else if (t.reversed !== undefined) {
      throw new Error(`session.json: track ${trackNo} reversed must be a boolean, got ${JSON.stringify(t.reversed)}`);
    }
    let state: ParsedSessionTrackState;
    if (t.state === undefined || t.state === 'PLAYING' || t.state === 'OVERDUBBING') state = 'PLAYING';
    else if (t.state === 'STOPPED') state = 'STOPPED';
    else {
      throw new Error(
        `session.json: track ${trackNo} state must be PLAYING, OVERDUBBING or STOPPED, got ${JSON.stringify(t.state)}`,
      );
    }
    const fx: FxState[] = validateFxStates(t.fx, `session.json: track ${trackNo}`);
    let dubFeedback = 1;
    if (typeof t.dubFeedback === 'number' && Number.isFinite(t.dubFeedback)) dubFeedback = Math.max(0, Math.min(1, t.dubFeedback));
    else if (t.dubFeedback !== undefined) {
      throw new Error(`session.json: track ${trackNo} dubFeedback must be a number, got ${JSON.stringify(t.dubFeedback)}`);
    }
    let pan = 0;
    if (typeof t.pan === 'number' && Number.isFinite(t.pan)) pan = Math.max(-1, Math.min(1, t.pan)) + 0; // -0 as 0
    else if (t.pan !== undefined) {
      throw new Error(`session.json: track ${trackNo} pan must be a number, got ${JSON.stringify(t.pan)}`);
    }
    return { track: trackNo, file: t.file, volume, muted, reversed, state, frames: master, fx, dubFeedback, pan };
  });
  return { bpm, meter, bars, masterLengthFrames: master, sampleRate, tracks };
}

/** session.json's `meter` in format `version` (validateSession's rules), as the entry of `meters`. */
function sessionMeter(raw: unknown, version: number, meters: readonly TimeSignature[]): TimeSignature {
  const four = meters.find((m) => m.num === 4 && m.den === 4);
  if (!four) throw new Error('validateSession: the offered meters lack 4/4');
  if (raw === undefined) {
    if (version >= 2) throw new Error(`session.json: format v${version} names its meter, and this one has none`);
    return four;
  }
  if (typeof raw !== 'object' || raw === null) throw new Error(`session.json: meter must be an object, got ${JSON.stringify(raw)}`);
  const { num, den, beats } = raw as Record<string, unknown>;
  const meter = meters.find((m) => m.num === num && m.den === den);
  if (!meter) throw new Error(`session.json: meter ${JSON.stringify(num)}/${JSON.stringify(den)} is not a time signature this build offers`);
  if (beats !== meter.beats) {
    throw new Error(
      `This session counts ${meter.num}/${meter.den} in ${JSON.stringify(beats)} beats a bar, and this build counts it in ` +
        `${meter.beats}: its BPM would read in another unit, so it cannot open it.`,
    );
  }
  if (version < 2 && meter !== four) {
    throw new Error(`session.json: a format v1 session is 4/4, but this one names ${meter.num}/${meter.den}`);
  }
  return meter;
}
