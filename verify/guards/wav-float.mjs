// Recovery must preserve Float32 PCM, including overdub headroom and samples below one PCM16 step.
import { encodeWav, decodeWav } from '../../src/audio/export/wav.ts';

let checks = 0;
let fails = 0;
function ok(name, condition) {
  checks++;
  if (!condition) { fails++; console.log(`  FAIL ${name}`); }
}
function rejects(name, fn) {
  let rejected = false;
  try { fn(); } catch { rejected = true; }
  ok(name, rejected);
}
const left = Float32Array.from([0, -0, 1.75, -2.5, 1e-8, -1e-8, 0.123456789]);
const right = Float32Array.from(left, (sample) => -sample);
for (const sampleRate of [44100, 48000]) {
  for (const channels of [[left], [left, right]]) {
    const bytes = encodeWav(channels, sampleRate, 'float32');
    const header = new DataView(bytes.buffer);
    ok('IEEE float format with extension', header.getUint16(20, true) === 3 && header.getUint32(16, true) === 18);
    ok('32-bit container and correct block alignment', header.getUint16(34, true) === 32 && header.getUint16(32, true) === channels.length * 4);
    ok('fact sample count', new TextDecoder().decode(bytes.subarray(38, 42)) === 'fact' && header.getUint32(46, true) === left.length);
    ok('RIFF byte count', header.getUint32(4, true) === bytes.length - 8);
    const decoded = decodeWav(bytes);
    ok('sample rate and channel count', decoded.sampleRate === sampleRate && decoded.channels.length === channels.length);
    ok('every Float32 bit survives', channels.every((channel, c) => {
      const expected = new Uint32Array(channel.buffer);
      const actual = new Uint32Array(decoded.channels[c].buffer);
      return expected.length === actual.length && expected.every((bits, i) => bits === actual[i]);
    }));
  }
}

// Independent IEEE-float fixture: reuse only the PCM container, then replace its format and data.
const fixture = encodeWav([new Float32Array(8)], 48000);
const view = new DataView(fixture.buffer);
view.setUint16(20, 3, true);
view.setUint16(34, 32, true);
view.setUint16(32, 4, true);
view.setUint32(28, 48000 * 4, true);
for (const [i, value] of [2.25, -3.5, 1e-9, -0].entries()) view.setFloat32(44 + i * 4, value, true);
ok('independent float fixture', decodeWav(fixture).channels[0][1] === -3.5);
view.setFloat32(44, NaN, true);
rejects('reject NaN before it reaches the audio graph', () => decodeWav(fixture));
view.setFloat32(44, Infinity, true);
rejects('reject Infinity before it reaches the audio graph', () => decodeWav(fixture));
rejects('reject non-finite recovery samples at save time', () => encodeWav([Float32Array.of(NaN)], 48000, 'float32'));

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
if (fails) process.exit(1);
