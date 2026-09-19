// verify/fs-wav-decode-verify.mjs — deterministic guard for decodeWav in src/audio/export/wav.ts.
// Imports the REAL encoder + decoder (Node TS type-stripping) so it cannot drift from the source.
// Asserts: (A) decode(encode(x)) ≈ x within one PCM16 step, mono + stereo, incl. out-of-range inputs;
// (B) encode(decode(bytes)) is BYTE-IDENTICAL for a synthetic file over the full int16 edge set +
// seeded-PRNG samples; (C) malformed inputs throw 'unsupported WAV: …' (or a bounds error); (D) the
// chunk walk skips an unknown chunk (LIST) before 'data' and still decodes.
// Run: node verify/fs-wav-decode-verify.mjs
import { encodeWav, decodeWav, floatToPcm16 } from '../src/audio/export/wav.ts';

let fails = 0, checks = 0;
function ok(name, cond, detail = '') {
  checks++;
  if (!cond) { fails++; console.log(`  FAIL  ${name}  ${detail}`); }
}
const u8 = (x) => new Uint8Array(x); // encodeWav already returns a Uint8Array; this copies it defensively.

// Deterministic LCG (glibc constants) → float in [-1, 1). No Math.random.
function makePrng(seed) {
  let s = seed >>> 0;
  return () => {
    s = (Math.imul(1103515245, s) + 12345) >>> 0;
    return (s / 0xffffffff) * 2 - 1;
  };
}

const STEP = 1 / 32767; // one PCM16 quantization step

// Build a WAV byte buffer directly from raw int16 samples (bypasses encode's float path) so section B
// can cover exact int16 values including -32768, plus optional leading extra chunks for the chunk walk.
function buildWav(int16Channels, sampleRate, extraChunks = []) {
  const numCh = int16Channels.length;
  const frames = numCh > 0 ? int16Channels[0].length : 0;
  const dataBytes = frames * numCh * 2;
  let extraBytes = 0;
  for (const c of extraChunks) extraBytes += 8 + c.data.length + (c.data.length & 1);
  const buf = new ArrayBuffer(44 + extraBytes + dataBytes);
  const view = new DataView(buf);
  const writeStr = (off, s) => { for (let i = 0; i < s.length; i++) view.setUint8(off + i, s.charCodeAt(i)); };
  writeStr(0, 'RIFF');
  view.setUint32(4, 36 + extraBytes + dataBytes, true);
  writeStr(8, 'WAVE');
  writeStr(12, 'fmt ');
  view.setUint32(16, 16, true);
  view.setUint16(20, 1, true);
  view.setUint16(22, numCh, true);
  view.setUint32(24, sampleRate, true);
  view.setUint32(28, sampleRate * numCh * 2, true);
  view.setUint16(32, numCh * 2, true);
  view.setUint16(34, 16, true);
  let off = 36;
  for (const c of extraChunks) {
    writeStr(off, c.id); off += 4;
    view.setUint32(off, c.data.length, true); off += 4;
    for (let i = 0; i < c.data.length; i++) view.setUint8(off + i, c.data[i]);
    off += c.data.length;
    if (c.data.length & 1) { view.setUint8(off, 0); off += 1; } // pad byte
  }
  writeStr(off, 'data'); off += 4;
  view.setUint32(off, dataBytes, true); off += 4;
  for (let f = 0; f < frames; f++) {
    for (let c = 0; c < numCh; c++) { view.setInt16(off, int16Channels[c][f], true); off += 2; }
  }
  return new Uint8Array(buf);
}

const bytesEqual = (a, b) => {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
};

// ---- A. decode(encode(x)) ≈ x within one PCM16 step (mono + stereo) ----
{
  // Mono, incl. ±1.0 and beyond-range (encode clamps to [-1,1]).
  const mono = Float32Array.from([0, 0.5, -0.5, 1, -1, 2, -2, 0.123456, -0.987654]);
  const { sampleRate, channels } = decodeWav(u8(encodeWav([mono], 44100)));
  ok('A.mono sampleRate preserved', sampleRate === 44100, String(sampleRate));
  ok('A.mono channel count == 1', channels.length === 1, String(channels.length));
  ok('A.mono length preserved', channels[0].length === mono.length, String(channels[0].length));
  let worst = 0;
  for (let i = 0; i < mono.length; i++) {
    const expect = Math.max(-1, Math.min(1, mono[i])); // encode clamps before quantize
    worst = Math.max(worst, Math.abs(channels[0][i] - expect));
  }
  ok('A.mono round-trips within one PCM16 step', worst <= STEP + 1e-12, `worst=${worst}`);

  // Stereo, PRNG-filled, some beyond range.
  const prng = makePrng(0xC0FFEE);
  const N = 300;
  const L = new Float32Array(N), R = new Float32Array(N);
  for (let i = 0; i < N; i++) { L[i] = prng() * 1.3; R[i] = prng() * 1.3; } // *1.3 pushes some past ±1
  const st = decodeWav(u8(encodeWav([L, R], 48000)));
  ok('A.stereo sampleRate preserved', st.sampleRate === 48000, String(st.sampleRate));
  ok('A.stereo channel count == 2', st.channels.length === 2, String(st.channels.length));
  let worstL = 0, worstR = 0;
  for (let i = 0; i < N; i++) {
    worstL = Math.max(worstL, Math.abs(st.channels[0][i] - Math.max(-1, Math.min(1, L[i]))));
    worstR = Math.max(worstR, Math.abs(st.channels[1][i] - Math.max(-1, Math.min(1, R[i]))));
  }
  ok('A.stereo L round-trips within one step', worstL <= STEP + 1e-12, `worst=${worstL}`);
  ok('A.stereo R round-trips within one step', worstR <= STEP + 1e-12, `worst=${worstR}`);
}

// ---- B. encode(decode(bytes)) BYTE-IDENTICAL (full int16 edge set + PRNG, mono + stereo) ----
{
  const edge = [-32768, -32767, -1, 0, 1, 32767];
  const prng = makePrng(0x1234);
  const rand = [];
  for (let i = 0; i < 400; i++) rand.push(Math.round(prng() * 32767)); // clamp-safe: |round| ≤ 32767
  const monoInt = Int16Array.from([...edge, ...rand]);
  const original = buildWav([monoInt], 44100);
  const round = u8(encodeWav(decodeWav(original).channels, decodeWav(original).sampleRate));
  ok('B.mono encode(decode(bytes)) is byte-identical', bytesEqual(original, round),
     `len ${original.length} vs ${round.length}`);
  // Explicitly assert -32768 survives the round trip (the 32767-divisor invariant).
  const dec = decodeWav(original);
  ok('B.-32768 re-encodes to -32768', floatToPcm16(dec.channels[0][0]) === -32768,
     String(floatToPcm16(dec.channels[0][0])));

  // Stereo edge set + PRNG.
  const prng2 = makePrng(0x99);
  const Lr = [], Rr = [];
  for (let i = 0; i < 200; i++) { Lr.push(Math.round(prng2() * 32767)); Rr.push(Math.round(prng2() * 32767)); }
  const Li = Int16Array.from([...edge, ...Lr]);
  const Ri = Int16Array.from([32767, 1, 0, -1, -32767, -32768, ...Rr]);
  const originalSt = buildWav([Li, Ri], 48000);
  const decSt = decodeWav(originalSt);
  const roundSt = u8(encodeWav(decSt.channels, decSt.sampleRate));
  ok('B.stereo encode(decode(bytes)) is byte-identical', bytesEqual(originalSt, roundSt),
     `len ${originalSt.length} vs ${roundSt.length}`);
}

// ---- C. malformed inputs throw ----
{
  const throws = (name, fn) => {
    let threw = false, msg = '';
    try { fn(); } catch (e) { threw = true; msg = String(e.message || e); }
    ok(name, threw, threw ? '' : '(did not throw)');
    return msg;
  };
  // bad RIFF magic
  {
    const b = buildWav([Int16Array.from([1, 2])], 44100);
    b[0] = 0x58; // 'X' — corrupt 'RIFF'
    throws('C.bad RIFF magic throws', () => decodeWav(b));
  }
  // bad WAVE form
  {
    const b = buildWav([Int16Array.from([1, 2])], 44100);
    b[8] = 0x58; // corrupt 'WAVE'
    throws('C.bad WAVE form throws', () => decodeWav(b));
  }
  // 24-bit fmt
  {
    const b = buildWav([Int16Array.from([1, 2])], 44100);
    b[34] = 24; // bitsPerSample = 24
    throws('C.24-bit fmt throws', () => decodeWav(b));
  }
  // IEEE float with an invalid 16-bit container remains unsupported.
  {
    const b = buildWav([Int16Array.from([1, 2])], 44100);
    b[20] = 3; // audioFormat = IEEE float
    throws('C.16-bit float fmt throws', () => decodeWav(b));
  }
  // truncated data chunk (declared size overruns the actual file)
  {
    const b = buildWav([Int16Array.from([1, 2, 3, 4])], 44100);
    const chopped = b.slice(0, b.length - 4); // drop last 2 samples' bytes; data size still says full
    throws('C.truncated data chunk throws', () => decodeWav(chopped));
  }
  // Validly padded but malformed mono data chunk: three PCM bytes cannot form whole 2-byte frames.
  {
    const canonical = buildWav([Int16Array.from([1])], 44100);
    const partialFrame = new Uint8Array(canonical.length + 2);
    partialFrame.set(canonical);
    const view = new DataView(partialFrame.buffer);
    view.setUint32(4, partialFrame.length - 8, true); // RIFF body size
    view.setUint32(40, 3, true); // data size: one frame + one stray byte
    partialFrame[46] = 0x7f;
    partialFrame[47] = 0; // RIFF odd-chunk padding
    const msg = throws('C.partial mono PCM frame throws', () => decodeWav(partialFrame));
    ok('C.partial mono PCM frame names alignment', msg.includes('not aligned to 2-byte frames'), msg);
  }
  // pure garbage
  {
    throws('C.garbage throws', () => decodeWav(Uint8Array.from([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13])));
  }
  // empty / too short
  {
    throws('C.too short throws', () => decodeWav(new Uint8Array(4)));
  }
}

// ---- D. chunk walk: an unknown chunk before 'data' still decodes ----
{
  // Odd-length LIST body to exercise the pad-byte handling, then a fact chunk.
  const int16 = Int16Array.from([1000, -1000, 32767, -32768]);
  const listBody = Array.from('INFOxyz').map((ch) => ch.charCodeAt(0)); // length 7 (odd → padded)
  const factBody = [0, 0, 0, 0]; // 4-byte 'fact' sample count
  const withExtra = buildWav([int16], 44100, [
    { id: 'LIST', data: listBody },
    { id: 'fact', data: factBody },
  ]);
  const dec = decodeWav(withExtra);
  ok('D.decodes past unknown chunks', dec.channels.length === 1 && dec.channels[0].length === 4,
     `${dec.channels.length}ch × ${dec.channels[0]?.length}`);
  // And the samples are the ones from the data chunk, not the extra chunks.
  const re = u8(encodeWav(dec.channels, dec.sampleRate));
  const plain = buildWav([int16], 44100); // no extra chunks → canonical 44-byte layout
  ok('D.re-encode of extra-chunk file matches the canonical no-extra file byte-for-byte',
     bytesEqual(re, plain), `len ${re.length} vs ${plain.length}`);
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
if (fails) process.exit(1);
