// src/audio/export/session-schema.ts
// PURE session.json schema: validateSession + its parsed-shape types, with NO engine/looper/Web-Audio
// dependency. Split out of import.ts so verify/fs-import-verify.mjs can import the REAL validator under
// Node (TS type-stripping) while import.ts is free to pull in engine/looper STATICALLY. Runtime imports
// here must stay PURE + Node-importable — no engine/looper/Tone/Tauri/Web Audio.
import { validateFxStates, type FxState } from '../fx/metadata.ts';
import { framesPerBar } from '../quantize.ts';

/** One validated session track (normalized: volume clamped, legacy muted normalized, fx deep-copied). */
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
  /** Always === masterLengthFrames (validated). */
  frames: number;
  /** Exactly 5 entries, chain order filter,pitch,stutter,delay,reverb. */
  fx: FxState[];
}
export interface ParsedSession {
  bpm: number;
  bars: number;
  masterLengthFrames: number;
  sampleRate: number;
  tracks: ParsedSessionTrack[];
}

/**
 * Validate + normalize a parsed session.json (the schema export.ts writes). PURE — safe under Node.
 * Throws a descriptive Error on: a non-BleepLoop app tag, a formatVersion newer than this build
 * understands (or a garbage formatVersion), missing/mis-typed fields, a non-positive/non-integer
 * masterLengthFrames, an empty or >5-track list, track numbers outside 1..5 or repeated, a per-track
 * frames that differs from masterLengthFrames, a repeated stem file, or an fx array that doesn't
 * exactly match the five-effect key/range contract. Two fields normalize instead of rejecting: volume
 * clamps into [0, 1.5]; muted accepts legacy 0/1 and missing-as-false; reversed is optional and
 * missing-as-false for legacy format-v1 exports.
 * Returns a fresh normalized object (never the input aliased). Unknown keys are ignored — notably the
 * per-track `state` export.ts writes (PLAYING/OVERDUBBING/STOPPED) is ADVISORY only (it records which
 * stems fed the master) and is deliberately NOT required, so pre-`state` exports still import; loadSession
 * brings every imported track to PLAYING regardless.
 */
export function validateSession(json: unknown): ParsedSession {
  if (typeof json !== 'object' || json === null || Array.isArray(json)) {
    throw new Error('session.json: not a JSON object');
  }
  const s = json as Record<string, unknown>;
  // 'LoopForge' is the app's earlier name: exports written under it still import.
  if (s.app !== 'BleepLoop' && s.app !== 'LoopForge') {
    throw new Error(`session.json: app tag is ${JSON.stringify(s.app)}, expected "BleepLoop" — not a BleepLoop session`);
  }
  // formatVersion gate: MISSING is treated as legacy v1 (every pre-versioning export still imports);
  // ===1 is this build's format; an integer > 1 was written by a newer BleepLoop; anything else is
  // garbage. A non-integer/<1 value is rejected as invalid rather than silently ignored.
  const fv = s.formatVersion;
  if (fv !== undefined) {
    if (typeof fv !== 'number' || !Number.isInteger(fv) || fv < 1) {
      throw new Error(`session.json: formatVersion must be a positive integer, got ${JSON.stringify(fv)}`);
    }
    if (fv > 1) {
      throw new Error(`This session was exported by a newer version of BleepLoop (format v${fv}) — update BleepLoop to open it.`);
    }
  }
  const num = (key: string): number => {
    const v = s[key];
    if (typeof v !== 'number' || !Number.isFinite(v)) {
      throw new Error(`session.json: ${key} missing or not a finite number (got ${JSON.stringify(v)})`);
    }
    return v;
  };
  const bpm = num('bpm');
  // Mirror clock.ts's MIN/MAX_BPM instead of letting setBpm silently clamp+round a hand-edited
  // value — a clamped bpm would recover the WRONG bar count from the frame math with no error.
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
  const expectedFrames = bars * framesPerBar(bpm, sampleRate);
  if (!Number.isSafeInteger(expectedFrames) || expectedFrames !== master) {
    throw new Error(`session.json: grid expects ${expectedFrames} frames from bpm/bars/sampleRate, got masterLengthFrames ${master}`);
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
    const fx: FxState[] = validateFxStates(t.fx, `session.json: track ${trackNo}`);
    return { track: trackNo, file: t.file, volume, muted, reversed, frames: master, fx };
  });
  return { bpm, bars, masterLengthFrames: master, sampleRate, tracks };
}
