// verify/fs-wav-export-verify.mjs — deterministic guard for src/audio/export/wav.ts.
// Imports the REAL encoder (Node TS type-stripping) so it cannot drift from the source. Asserts the
// canonical 44-byte RIFF/WAVE/fmt/data header byte-for-byte, PCM16 quantization + clamping, the
// mono/stereo channel layout + interleave order, mixMono's track/master gains + hard-clamp, the wet
// master's generalized final-period slice, and the pure enabled-FX warm-up plan. The OfflineAudioContext
// graph itself is browser-only — its runtime gate is the Playwright export probe, not this guard.
// Run: node verify/fs-wav-export-verify.mjs
import { warmupPassesForFx } from '../src/audio/export/render-plan.ts';
import { encodeWav, finalPeriod, floatToPcm16, mixMono } from '../src/audio/export/wav.ts';

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

// ---- E. finalPeriod: retain the final pass after a variable number of warm-up periods ----
{
  const frames = 4;
  const rendered = Float32Array.from([
    0, 0, 0, 0,
    0.125, 0.125, 0.125, 0.125,
    0.25, 0.25, 0.25, 0.25,
    0.5, 0.625, 0.75, 0.875,
    0, 0,
  ]);
  const out = finalPeriod(rendered, frames, 3);
  ok('E.slice is exactly [warmup*frames, (warmup+1)*frames)', out.length === 4 && out[0] === 0.5 && out[3] === 0.875,
     JSON.stringify(Array.from(out)));
  ok('E.slice is a copy (mutating it leaves the render intact)', (out[0] = 9, rendered[12] === 0.5),
     String(rendered[12]));
  ok('E.exact multi-pass render (no padding) still slices', finalPeriod(new Float32Array(16), 4, 3).length === 4);
  let threw = false;
  try { finalPeriod(new Float32Array(15), 4, 3); } catch { threw = true; }
  ok('E.short render throws loudly (never a truncated master)', threw);
  let badPassesThrew = false;
  try { finalPeriod(new Float32Array(8), 4, 1.5); } catch { badPassesThrew = true; }
  ok('E.non-integer warmup count rejects', badPassesThrew);
}

// ---- F. enabled FX tails derive warm-up passes from the real serialized state ----
{
  const fx5 = () => [
    { bypassed: true, params: { cutoff: 1200, q: 2 } },
    { bypassed: true, params: { semitones: 0 } },
    { bypassed: true, params: { rate: 1 } },
    { bypassed: true, params: { time: 1, feedback: 0.4, mix: 0.3 } },
    { bypassed: true, params: { amount: 0.3 } },
  ];

  ok('F.no enabled tail keeps one priming pass', warmupPassesForFx([{ fx: fx5() }], 120, 0.8) === 1);

  const reverb = fx5();
  reverb[4].bypassed = false;
  ok('F.2.62s reverb over a 0.8s loop needs 4 warm-up passes',
    warmupPassesForFx([{ fx: reverb }], 300, 0.8) === 4);

  const dryReverb = fx5();
  dryReverb[4] = { bypassed: false, params: { amount: 0 } };
  ok('F.zero reverb send has no audible tail', warmupPassesForFx([{ fx: dryReverb }], 300, 0.8) === 1);

  const longDelay = fx5();
  longDelay[3] = { bypassed: false, params: { time: 0, feedback: 0.95, mix: 0.3 } };
  ok('F.quarter delay @300/0.95 reaches 1e-4 after 45 one-bar warm-ups',
    warmupPassesForFx([{ fx: longDelay }], 300, 0.8) === 45);

  const oneEcho = fx5();
  oneEcho[3] = { bypassed: false, params: { time: 2, feedback: 0, mix: 0.3 } };
  ok('F.actual dotted-eighth delay choice drives the tail duration',
    warmupPassesForFx([{ fx: oneEcho }], 120, 0.2) === 2);

  const dryDelay = fx5();
  dryDelay[3] = { bypassed: false, params: { time: 0, feedback: 0.95, mix: 0 } };
  ok('F.zero delay mix has no audible tail', warmupPassesForFx([{ fx: dryDelay }], 300, 0.8) === 1);
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
if (fails) process.exit(1);
