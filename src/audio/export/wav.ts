// src/audio/export/wav.ts
// Pure WAV (PCM16 / Float32) encoder + master-mix helpers. NO Web Audio / DOM / Tauri imports so a Node
// verify guard can import it directly (verify/guards/wav-export.mjs). All functions are pure.

/** Clamp a float sample to [-1,1] and quantize to signed 16-bit. -1→-32767, 1→32767, |x|>1 clamps. */
export function floatToPcm16(sample: number): number {
  let v = Math.round(sample * 32767);
  if (v > 32767) v = 32767;
  else if (v < -32768) v = -32768;
  return v;
}

/**
 * Encode equal-length channels as interleaved WAV. Masters use PCM16; editable stems use Float32
 * to preserve headroom and quiet samples exactly. Float files carry an extended fmt and a fact chunk.
 */
export function encodeWav(
  channels: Float32Array[],
  sampleRate: number,
  format: 'pcm16' | 'float32' = 'pcm16',
): Uint8Array<ArrayBuffer> {
  const floating = format === 'float32';
  const sampleBytes = floating ? 4 : 2;
  const headerBytes = floating ? 58 : 44;
  const numCh = channels.length;
  const frames = numCh > 0 ? channels[0].length : 0;
  const dataBytes = frames * numCh * sampleBytes;
  const buf = new ArrayBuffer(headerBytes + dataBytes);
  const view = new DataView(buf);
  const writeStr = (off: number, s: string) => {
    for (let i = 0; i < s.length; i++) view.setUint8(off + i, s.charCodeAt(i));
  };

  writeStr(0, 'RIFF');
  view.setUint32(4, buf.byteLength - 8, true); // chunk size
  writeStr(8, 'WAVE');
  writeStr(12, 'fmt ');
  view.setUint32(16, floating ? 18 : 16, true);
  view.setUint16(20, floating ? 3 : 1, true); // IEEE float or PCM
  view.setUint16(22, numCh, true);
  view.setUint32(24, sampleRate, true);
  view.setUint32(28, sampleRate * numCh * sampleBytes, true); // byte rate
  view.setUint16(32, numCh * sampleBytes, true); // block align
  view.setUint16(34, sampleBytes * 8, true);
  if (floating) {
    view.setUint16(36, 0, true); // cbSize: no format-specific extension
    writeStr(38, 'fact');
    view.setUint32(42, 4, true);
    view.setUint32(46, frames, true);
  }
  writeStr(headerBytes - 8, 'data');
  view.setUint32(headerBytes - 4, dataBytes, true);

  let off = headerBytes;
  for (let f = 0; f < frames; f++) {
    for (let c = 0; c < numCh; c++) {
      const sample = channels[c][f];
      if (floating) {
        if (!Number.isFinite(sample)) throw new Error('Cannot save non-finite WAV samples');
        view.setFloat32(off, sample, true);
      } else {
        view.setInt16(off, floatToPcm16(sample), true);
      }
      off += sampleBytes;
    }
  }
  return new Uint8Array(buf);
}

/**
 * Decode PCM16 or IEEE Float32 WAV into `{ sampleRate, channels }`.
 *
 * Parses by CHUNK WALK — RIFF/WAVE container, then iterate `fmt `/`data`/… chunks — rather than
 * trusting the 44-byte offset our own encoder emits, so it also reads files with extra chunks
 * (LIST/fact/…) and honours the RIFF odd-size padding rule. Formats are PCM16 (1) and Float32 (3);
 * anything else throws `unsupported WAV: …`. Non-finite floats are rejected. Every read
 * is bounds-checked, so a truncated header or short `data` chunk throws instead of reading garbage.
 *
 * int16 → float is `v / 32767` with NO clamp — deliberately the inverse of `floatToPcm16` (which maps
 * into [-32768, 32767]). Using 32767 (not 32768) makes `encodeWav(decodeWav(bytes))` byte-identical for
 * EVERY int16 value, including -32768: `Math.round(-32768/32767 * 32767) === -32768`. The only cost is
 * that -32768 decodes to ≈ -1.00003 (just past -1.0) — Web Audio accepts out-of-range float samples,
 * and our own encoder round-trips it back to -32768 exactly, so the round trip is lossless.
 */
export function decodeWav(bytes: Uint8Array): { sampleRate: number; channels: Float32Array[] } {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const len = bytes.byteLength;
  const str = (off: number, n: number): string => {
    let s = '';
    for (let i = 0; i < n; i++) s += String.fromCharCode(view.getUint8(off + i));
    return s;
  };

  // RIFF/WAVE container header (12 bytes: 'RIFF' <size> 'WAVE').
  if (len < 12) throw new Error(`unsupported WAV: file too short (${len} bytes, need ≥ 12)`);
  if (str(0, 4) !== 'RIFF') throw new Error(`unsupported WAV: expected 'RIFF' magic, got '${str(0, 4)}'`);
  if (str(8, 4) !== 'WAVE') throw new Error(`unsupported WAV: expected 'WAVE' form, got '${str(8, 4)}'`);

  // Walk the chunks after the 12-byte container header.
  let fmt: { audioFormat: number; numCh: number; sampleRate: number; bitsPerSample: number } | null = null;
  let dataOff = -1;
  let dataSize = -1;
  let off = 12;
  while (off + 8 <= len) {
    const id = str(off, 4);
    const size = view.getUint32(off + 4, true);
    const body = off + 8;
    if (body + size > len) throw new Error(`unsupported WAV: chunk '${id}' size ${size} overruns file (${len} bytes)`);
    if (id === 'fmt ') {
      if (size < 16) throw new Error(`unsupported WAV: fmt chunk too small (${size} bytes, need ≥ 16)`);
      fmt = {
        audioFormat: view.getUint16(body, true),
        numCh: view.getUint16(body + 2, true),
        sampleRate: view.getUint32(body + 4, true),
        bitsPerSample: view.getUint16(body + 14, true),
      };
    } else if (id === 'data') {
      dataOff = body;
      dataSize = size;
    }
    // Advance past body + RIFF odd-size padding byte.
    off = body + size + (size & 1);
  }

  if (!fmt) throw new Error(`unsupported WAV: no 'fmt ' chunk found`);
  if (dataOff < 0) throw new Error(`unsupported WAV: no 'data' chunk found`);
  const floating = fmt.audioFormat === 3 && fmt.bitsPerSample === 32;
  if (!floating && !(fmt.audioFormat === 1 && fmt.bitsPerSample === 16)) {
    throw new Error(`unsupported WAV: format ${fmt.audioFormat}, ${fmt.bitsPerSample} bits (expected PCM16 or Float32)`);
  }
  const numCh = fmt.numCh;
  if (numCh < 1) throw new Error(`unsupported WAV: ${numCh} channels`);

  const sampleBytes = floating ? 4 : 2;
  const frameBytes = numCh * sampleBytes;
  if (dataSize % frameBytes !== 0) {
    throw new Error(
      `unsupported WAV: data chunk ${dataSize} bytes is not aligned to ${frameBytes}-byte frames`,
    );
  }
  const frames = dataSize / frameBytes;
  const channels: Float32Array[] = [];
  for (let c = 0; c < numCh; c++) channels.push(new Float32Array(frames));
  let p = dataOff;
  for (let f = 0; f < frames; f++) {
    for (let c = 0; c < numCh; c++) {
      // int16 → float, NO clamp (see the doc comment above for why 32767, not 32768).
      const sample = floating ? view.getFloat32(p, true) : view.getInt16(p, true) / 32767;
      if (!Number.isFinite(sample)) throw new Error('unsupported WAV: non-finite sample');
      channels[c][f] = sample;
      p += sampleBytes;
    }
  }
  return { sampleRate: fmt.sampleRate, channels };
}

/**
 * Extract the final `frames`-long period after `warmupPasses` complete priming periods. The tail that
 * would spill past the loop end is already present at the retained loop head, exactly like live looped
 * playback. Throws if the render is too short (a duration bug should fail loudly, not truncate).
 */
export function finalPeriod(
  rendered: Float32Array,
  frames: number,
  warmupPasses: number,
): Float32Array {
  if (!Number.isInteger(warmupPasses) || warmupPasses < 0) {
    throw new Error(`finalPeriod: warmupPasses must be a non-negative integer, got ${warmupPasses}`);
  }
  const end = (warmupPasses + 1) * frames;
  if (rendered.length < end) {
    throw new Error(`finalPeriod: rendered ${rendered.length} < ${warmupPasses + 1}×${frames} frames`);
  }
  return rendered.slice(warmupPasses * frames, end);
}

/**
 * Sum mono tracks into a single mono master, applying per-track volume, skipping muted tracks.
 * HARD-CLAMPS the per-sample sum to [-1,1] AFTER summing (chosen over normalization so the export is
 * deterministic and matches what the limiter would tame at playback; a v1 could add peak-normalize).
 * `frames` = masterLengthFrames; tracks shorter than that are treated as zero past their end.
 */
export function mixMono(
  tracks: { pcm: Float32Array; volume: number; muted: boolean }[],
  frames: number,
  masterLevel: number,
): Float32Array {
  const out = new Float32Array(frames);
  for (const t of tracks) {
    if (t.muted || t.volume === 0) continue;
    const n = Math.min(frames, t.pcm.length);
    for (let i = 0; i < n; i++) out[i] += t.pcm[i] * t.volume;
  }
  for (let i = 0; i < frames; i++) {
    const sample = out[i] * masterLevel;
    if (sample > 1) out[i] = 1;
    else if (sample < -1) out[i] = -1;
    else out[i] = sample;
  }
  return out;
}
