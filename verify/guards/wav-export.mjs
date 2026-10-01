// verify/guards/wav-export.mjs — deterministic guard for src/session/wav.ts.
// Imports the REAL encoder (Node TS type-stripping) so it cannot drift from the source. Asserts the
// canonical 44-byte RIFF/WAVE/fmt/data header byte-for-byte, PCM16 quantization + clamping, the
// mono/stereo channel layout + interleave order, and mixMono's track/master gains + hard-clamp (the
// export's dry fallback). The wet master is the engine's (lf-engine `tests/render_master.rs`).
// Run: node verify/guards/wav-export.mjs
import { encodeWav, floatToPcm16, mixMono } from '../../src/session/wav.ts';

let fails = 0, checks = 0;
function ok(name, cond, detail = '') {
  checks++;
  if (!cond) { fails++; console.log(`  FAIL  ${name}  ${detail}`); }
}
const str = (dv, off, len) => {
  let s = '';
  for (let i = 0; i < len; i++) s += String.fromCharCode(dv.getUint8(off + i));
  return s;
};

// ---- A. float -> PCM16 quantization + clamping ----
ok('A.floatToPcm16(1) == 32767', floatToPcm16(1) === 32767, String(floatToPcm16(1)));
ok('A.floatToPcm16(-1) == -32767', floatToPcm16(-1) === -32767, String(floatToPcm16(-1)));
ok('A.floatToPcm16(0) == 0', floatToPcm16(0) === 0, String(floatToPcm16(0)));
ok('A.floatToPcm16(2) clamps to 32767', floatToPcm16(2) === 32767, String(floatToPcm16(2)));
ok('A.floatToPcm16(-2) clamps to -32768', floatToPcm16(-2) === -32768, String(floatToPcm16(-2)));
ok('A.floatToPcm16(0.5) == 16384 (round 16383.5)', floatToPcm16(0.5) === 16384, String(floatToPcm16(0.5)));

// ---- B. mono header + sample bytes ----
{
  const wav = encodeWav([Float32Array.from([1, -1, 0.5])], 44100);
  const dv = new DataView(wav.buffer, wav.byteOffset, wav.byteLength); // encodeWav returns a Uint8Array
  ok('B.byteLength == 50', wav.byteLength === 44 + 3 * 1 * 2, String(wav.byteLength));
  ok('B.RIFF@0', str(dv, 0, 4) === 'RIFF');
  ok('B.chunkSize == 36+6', dv.getUint32(4, true) === 36 + 6, String(dv.getUint32(4, true)));
  ok('B.WAVE@8', str(dv, 8, 4) === 'WAVE');
  ok('B.fmt @12', str(dv, 12, 4) === 'fmt ');
  ok('B.subchunk1 == 16', dv.getUint32(16, true) === 16, String(dv.getUint32(16, true)));
  ok('B.audioFormat == 1 (PCM)', dv.getUint16(20, true) === 1, String(dv.getUint16(20, true)));
  ok('B.channels == 1', dv.getUint16(22, true) === 1, String(dv.getUint16(22, true)));
  ok('B.sampleRate == 44100', dv.getUint32(24, true) === 44100, String(dv.getUint32(24, true)));
  ok('B.byteRate == 88200', dv.getUint32(28, true) === 88200, String(dv.getUint32(28, true)));
  ok('B.blockAlign == 2', dv.getUint16(32, true) === 2, String(dv.getUint16(32, true)));
  ok('B.bitsPerSample == 16', dv.getUint16(34, true) === 16, String(dv.getUint16(34, true)));
  ok('B.data@36', str(dv, 36, 4) === 'data');
  ok('B.dataBytes == 6', dv.getUint32(40, true) === 6, String(dv.getUint32(40, true)));
  ok('B.sample[0] == 32767', dv.getInt16(44, true) === 32767, String(dv.getInt16(44, true)));
  ok('B.sample[1] == -32767', dv.getInt16(46, true) === -32767, String(dv.getInt16(46, true)));
  ok('B.sample[2] == 16384', dv.getInt16(48, true) === 16384, String(dv.getInt16(48, true)));
}

// ---- C. stereo interleave + byte-length ----
{
  const L = Float32Array.from([1, 0.5]);
  const R = Float32Array.from([-1, -0.5]);
  const wav = encodeWav([L, R], 48000);
  const dv = new DataView(wav.buffer, wav.byteOffset, wav.byteLength); // encodeWav returns a Uint8Array
  ok('C.byteLength == 52', wav.byteLength === 44 + 2 * 2 * 2, String(wav.byteLength));
  ok('C.channels == 2', dv.getUint16(22, true) === 2, String(dv.getUint16(22, true)));
  ok('C.byteRate == 192000', dv.getUint32(28, true) === 192000, String(dv.getUint32(28, true)));
  ok('C.blockAlign == 4', dv.getUint16(32, true) === 4, String(dv.getUint16(32, true)));
  // interleave order L0, R0, L1, R1
  ok('C.L0 == 32767', dv.getInt16(44, true) === 32767, String(dv.getInt16(44, true)));
  ok('C.R0 == -32767', dv.getInt16(46, true) === -32767, String(dv.getInt16(46, true)));
  ok('C.L1 == 16384', dv.getInt16(48, true) === 16384, String(dv.getInt16(48, true)));
  // -0.5*32767 = -16383.5; Math.round rounds toward +Infinity -> -16383.
  ok('C.R1 == -16383', dv.getInt16(50, true) === -16383, String(dv.getInt16(50, true)));
}

// ---- D. mixMono: sum, volume scaling, mute skip, hard-clamp, zero-pad ----
{
  const two = mixMono(
    [
      { pcm: Float32Array.from([0.5, 0.5]), volume: 1, muted: false },
      { pcm: Float32Array.from([0.5, 0.5]), volume: 1, muted: false },
    ],
    2,
    1,
  );
  ok('D.sum 0.5+0.5 == 1', two[0] === 1 && two[1] === 1, JSON.stringify(Array.from(two)));

  const muted = mixMono(
    [
      { pcm: Float32Array.from([0.5, 0.5]), volume: 1, muted: false },
      { pcm: Float32Array.from([0.5, 0.5]), volume: 1, muted: true },
    ],
    2,
    1,
  );
  ok('D.muted track contributes 0', muted[0] === 0.5 && muted[1] === 0.5, JSON.stringify(Array.from(muted)));

  const scaled = mixMono([{ pcm: Float32Array.from([1, 1]), volume: 0.25, muted: false }], 2, 1);
  ok('D.volume scales', scaled[0] === 0.25 && scaled[1] === 0.25, JSON.stringify(Array.from(scaled)));

  const clamped = mixMono(
    [
      { pcm: Float32Array.from([0.8, 0.8]), volume: 1, muted: false },
      { pcm: Float32Array.from([0.8, 0.8]), volume: 1, muted: false },
    ],
    2,
    1,
  );
  ok('D.sum 1.6 hard-clamps to 1', clamped[0] === 1 && clamped[1] === 1, JSON.stringify(Array.from(clamped)));

  const masterScaled = mixMono(
    [
      { pcm: Float32Array.from([0.8]), volume: 1, muted: false },
      { pcm: Float32Array.from([0.8]), volume: 1, muted: false },
    ],
    1,
    0.5,
  );
  ok('D.master level scales before final clamp (1.6×0.5 == 0.8)',
    Math.abs(masterScaled[0] - 0.8) < 1e-6, String(masterScaled[0]));

  const masterMuted = mixMono([{ pcm: Float32Array.from([1]), volume: 1, muted: false }], 1, 0);
  ok('D.master mute level 0 silences fallback mix', masterMuted[0] === 0, String(masterMuted[0]));

  const padded = mixMono([{ pcm: Float32Array.from([0.5]), volume: 1, muted: false }], 3, 1);
  ok('D.shorter pcm zero-padded past end', padded[0] === 0.5 && padded[1] === 0 && padded[2] === 0, JSON.stringify(Array.from(padded)));
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
if (fails) process.exit(1);
