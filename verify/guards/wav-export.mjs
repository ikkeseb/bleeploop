// verify/guards/wav-export.mjs — deterministic guard for src/session/wav.ts.
// Imports the REAL encoder (Node TS type-stripping) so it cannot drift from the source. Asserts the
// canonical 44-byte RIFF/WAVE/fmt/data header byte-for-byte, PCM16 quantization + clamping, the
// mono/stereo channel layout + interleave order, and mixStereo's track/master gains, pan + hard-clamp
// (the export's dry fallback): a centred mix is bit-identical to the dual-mono fallback before pan, a
// hard-panned lane leaves its far side exactly silent, and panGains is the engine's law. The wet master
// is the engine's (lf-engine `tests/render_master.rs`, the pan law `tests/pan.rs`).
// Run: node verify/guards/wav-export.mjs
import { createHash } from 'node:crypto';
import { encodeWav, floatToPcm16, mixStereo, panGains } from '../../src/session/wav.ts';

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

// ---- D. mixStereo, centred: sum, volume scaling, mute skip, hard-clamp, zero-pad, on both channels ----
// Each case is centred (no pan), so both channels must be the one mono mix.
const centred = (tracks, frames, level) => {
  const [left, right] = mixStereo(tracks, frames, level);
  ok(`D.centred channels equal (${JSON.stringify(Array.from(left))})`, left.every((x, i) => Object.is(x, right[i])), JSON.stringify(Array.from(right)));
  return left;
};
{
  const two = centred(
    [
      { pcm: Float32Array.from([0.5, 0.5]), volume: 1, muted: false },
      { pcm: Float32Array.from([0.5, 0.5]), volume: 1, muted: false },
    ],
    2,
    1,
  );
  ok('D.sum 0.5+0.5 == 1', two[0] === 1 && two[1] === 1, JSON.stringify(Array.from(two)));

  const muted = centred(
    [
      { pcm: Float32Array.from([0.5, 0.5]), volume: 1, muted: false },
      { pcm: Float32Array.from([0.5, 0.5]), volume: 1, muted: true },
    ],
    2,
    1,
  );
  ok('D.muted track contributes 0', muted[0] === 0.5 && muted[1] === 0.5, JSON.stringify(Array.from(muted)));

  const scaled = centred([{ pcm: Float32Array.from([1, 1]), volume: 0.25, muted: false }], 2, 1);
  ok('D.volume scales', scaled[0] === 0.25 && scaled[1] === 0.25, JSON.stringify(Array.from(scaled)));

  const clamped = centred(
    [
      { pcm: Float32Array.from([0.8, 0.8]), volume: 1, muted: false },
      { pcm: Float32Array.from([0.8, 0.8]), volume: 1, muted: false },
    ],
    2,
    1,
  );
  ok('D.sum 1.6 hard-clamps to 1', clamped[0] === 1 && clamped[1] === 1, JSON.stringify(Array.from(clamped)));

  const masterScaled = centred(
    [
      { pcm: Float32Array.from([0.8]), volume: 1, muted: false },
      { pcm: Float32Array.from([0.8]), volume: 1, muted: false },
    ],
    1,
    0.5,
  );
  ok('D.master level scales before final clamp (1.6×0.5 == 0.8)',
    Math.abs(masterScaled[0] - 0.8) < 1e-6, String(masterScaled[0]));

  const masterMuted = centred([{ pcm: Float32Array.from([1]), volume: 1, muted: false }], 1, 0);
  ok('D.master mute level 0 silences fallback mix', masterMuted[0] === 0, String(masterMuted[0]));

  const padded = centred([{ pcm: Float32Array.from([0.5]), volume: 1, muted: false }], 3, 1);
  ok('D.shorter pcm zero-padded past end', padded[0] === 0.5 && padded[1] === 0 && padded[2] === 0, JSON.stringify(Array.from(padded)));
}

// ---- E. pan: the law, the centre's bits, a hard pan's silent side ----
{
  // The engine's law (lf-engine `pan_gains`): exact at the centre and the ends, L^2 + R^2 = 2 throughout.
  const exact = (p, want) => {
    const got = panGains(p);
    ok(`E.panGains(${p}) == [${want}]`, Object.is(got[0], want[0]) && Object.is(got[1], want[1]), JSON.stringify(got));
  };
  exact(0, [1, 1]);
  exact(-1, [Math.SQRT2, 0]);
  exact(1, [0, Math.SQRT2]);
  exact(-3, [Math.SQRT2, 0]);
  exact(7, [0, Math.SQRT2]);
  for (const p of [-0.75, -0.3, 0.01, 0.5, 0.99]) {
    const [l, r] = panGains(p);
    const t = ((p + 1) * Math.PI) / 4;
    ok(`E.panGains(${p}) is sqrt2 cos/sin of (p+1)pi/4`, Math.abs(l - Math.SQRT2 * Math.cos(t)) < 1e-15 && Math.abs(r - Math.SQRT2 * Math.sin(t)) < 1e-15, JSON.stringify([l, r]));
    ok(`E.panGains(${p}) holds constant power`, Math.abs(l * l + r * r - 2) < 1e-12, String(l * l + r * r));
    ok(`E.panGains(${p}) leans its way`, p < 0 ? l > r : r > l, JSON.stringify([l, r]));
  }

  // A seeded jam (five lanes: two plain, one muted, one shorter than the master, one at volume 0) whose
  // sum clips, mixed by the dry fallback before pan (dual mono, `mixMono` at 0dd21bc7): the sha256 of
  // its Float32 output. A centred stereo mix must be those bits on both channels.
  const BEFORE_PAN = '1ce2f4dadf7a93bdf919c223eb1bd450c79e1524ec39698e6a73efdb21ce5072';
  let seed = 0x2545f491;
  const rand = () => {
    seed ^= seed << 13; seed >>>= 0; seed ^= seed >>> 17; seed ^= seed << 5; seed >>>= 0;
    return (seed / 0xffffffff) * 2 - 1;
  };
  const FRAMES = 4096;
  const track = (n, volume, muted) => ({ pcm: Float32Array.from({ length: n }, () => rand() * 0.9), volume, muted });
  const jam = [track(FRAMES, 0.8, false), track(FRAMES, 1.37, false), track(FRAMES, 0.5, true), track(3000, 0.123456789, false), track(FRAMES, 0, false)];
  const sha = (a) => createHash('sha256').update(new Uint8Array(a.buffer, a.byteOffset, a.byteLength)).digest('hex');
  for (const [name, tracks] of [['no pan', jam], ['pan 0', jam.map((t) => ({ ...t, pan: 0 }))], ['pan -0', jam.map((t) => ({ ...t, pan: -0 }))]]) {
    const [left, right] = mixStereo(tracks, FRAMES, 0.9);
    ok(`E.centred (${name}) left is the dual-mono fallback's bits`, sha(left) === BEFORE_PAN, sha(left));
    ok(`E.centred (${name}) right is the dual-mono fallback's bits`, sha(right) === BEFORE_PAN, sha(right));
  }
  ok('E.the seeded jam clips (the clamp is in the bits)', mixStereo(jam, FRAMES, 0.9)[0].some((x) => x === 1 || x === -1));

  // A lane hard right: its left side is exactly silent, its right +3 dB (the master level 1, no clip).
  const pcm = Float32Array.from([0.25, -0.5, 0.125]);
  const [hl, hr] = mixStereo([{ pcm, volume: 0.5, muted: false, pan: 1 }], 3, 1);
  ok('E.hard right: the left side is exactly 0', hl.every((x) => Object.is(x, 0)), JSON.stringify(Array.from(hl)));
  ok('E.hard right: the right side is sqrt2 x volume x pcm', hr.every((x, i) => x === Math.fround(pcm[i] * (0.5 * Math.SQRT2))), JSON.stringify(Array.from(hr)));
  const [ll, lr] = mixStereo([{ pcm, volume: 0.5, muted: false, pan: -1 }], 3, 1);
  ok('E.hard left: the right side is exactly 0', lr.every((x) => Object.is(x, 0)), JSON.stringify(Array.from(lr)));
  ok('E.hard left: the left side is sqrt2 x volume x pcm', ll.every((x, i) => x === Math.fround(pcm[i] * (0.5 * Math.SQRT2))), JSON.stringify(Array.from(ll)));
  // Over a centred lane, a hard-left one adds nothing to the right: the right is the centred lane alone.
  const centre = { pcm: Float32Array.from([0.1, 0.2, -0.3]), volume: 1, muted: false };
  const [, both] = mixStereo([centre, { pcm, volume: 1, muted: false, pan: -1 }], 3, 1);
  ok('E.a hard-left lane leaves a centred lane alone on the right', both.every((x, i) => x === centre.pcm[i]), JSON.stringify(Array.from(both)));
  // Part way: the gains of the law, each side clamped on its own.
  const [pl, pr] = mixStereo([{ pcm: Float32Array.from([1]), volume: 1, muted: false, pan: 0.5 }], 1, 1);
  const [gl, gr] = panGains(0.5);
  ok('E.pan 0.5: each side carries its gain (the right clamps at 1)', pl[0] === Math.fround(gl) && pr[0] === 1, JSON.stringify([pl[0], pr[0], gl, gr]));
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
if (fails) process.exit(1);
