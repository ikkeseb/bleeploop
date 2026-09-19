// Short later takes import the production tiling and stop-plan rules directly.
import { commitLaterTake, planLaterStop, tileTake } from '../src/audio/looper/grid-math.ts';
import { framesPerBar } from '../src/audio/quantize.ts';

let failed = 0;
let checks = 0;
function ok(name, condition, detail = '') {
  checks++;
  if (!condition) {
    failed++;
    console.log(`  FAIL  ${name}  ${detail}`);
  }
}
const exact = (actual, expected) =>
  actual.length === expected.length && actual.every((sample, i) => sample === expected[i]);

console.log('=== A. tile the chosen take window across the master ===');
{
  const oneBar = Float32Array.from([1, 2, 3]);
  const buf = new Float32Array(26).fill(99);
  buf.set(oneBar);
  tileTake(buf, 3, 24);
  ok(
    '1-over-8 makes eight sample-exact copies',
    exact(buf.subarray(0, 24), Float32Array.from({ length: 24 }, (_, i) => oneBar[i % 3])),
  );
  ok('1-over-8 leaves frames past master untouched', buf[24] === 99 && buf[25] === 99);
}
{
  const take = Float32Array.from([1, 2, 3, 4, 5, 6, 7, 8, 9]);
  const buf = new Float32Array(26).fill(77);
  buf.set(take);
  tileTake(buf, 9, 24);
  const expected = Float32Array.from({ length: 24 }, (_, i) => take[i % 9]);
  ok('3-over-8 makes 3+3+2 bars with the last copy cut', exact(buf.subarray(0, 24), expected));
  ok('3-over-8 cuts at the exact master edge', buf[23] === take[5] && buf[24] === 77);
}
{
  const buf = Float32Array.from([1, 2, 3, 4, 5, 6, 88, 89]);
  const before = buf.slice();
  tileTake(buf, 6, 6);
  ok('take length equal to master leaves the buffer unchanged', exact(buf, before));
}
{
  const buf = Float32Array.from([1, 2, 3, 90, 91, 92, 93, 94, 95, 66]);
  tileTake(buf, 3, 9);
  ok('samples captured beyond the chosen window never leak', exact(buf.subarray(0, 9), Float32Array.from([1, 2, 3, 1, 2, 3, 1, 2, 3])));
  ok('tiling never writes beyond master', buf[9] === 66);
}

console.log('=== B. later stop chooses completed bars from musical time ===');
{
  const bpm = 120;
  const sr = 48000;
  const masterBars = 8;
  const fpb = framesPerBar(bpm, sr);
  const barSec = fpb / sr;
  const musicalStart = 10;
  const plan = (bars, compensation = 0) =>
    planLaterStop(
      musicalStart + bars * barSec,
      Math.round(musicalStart * sr) + compensation,
      compensation,
      bpm,
      sr,
      masterBars,
    );

  ok('press at 0.4 bar chooses one bar', plan(0.4).bars === 1);
  ok('press at 1.5 bars chooses one bar', plan(1.5).bars === 1);
  ok('press 1/32 bar before bar 2 is inside grace', plan(2 - 1 / 32).bars === 2);
  ok('press just after bar 2 chooses two bars', plan(2 + 1e-7).bars === 2);
  ok('press at master chooses master bars', plan(masterBars).bars === masterBars);
  ok('press after master clamps to master bars', plan(masterBars + 3).bars === masterBars);
  ok('target is the selected whole-bar frame count', plan(3.4).target === 3 * fpb);
  // 5 ms outside the grace: a C of 960 frames (20 ms @ 48k) leaking into the elapsed time would flip it to 2.
  const justOutside = 2 - 1 / 16 - 0.005 / barSec;
  ok('C = 0 just outside the grace keeps one bar', plan(justOutside, 0).bars === 1);
  ok('C = 960 just outside the grace keeps one bar', plan(justOutside, 960).bars === 1);
  ok('C = 960 inside the grace still chooses two bars', plan(2 - 1 / 32, 960).bars === 2);

  // commitLaterTake: the floor and the short-window guard finishRecording relies on.
  const FPB = 4;
  const half = new Float32Array(16).fill(9); // stale frames everywhere
  half.set([1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 0); // 2.5 bars captured
  commitLaterTake(half, 10, FPB, 16);
  ok('2.5 captured bars floor to two and tile', Array.from(half).join() === '1,2,3,4,5,6,7,8,1,2,3,4,5,6,7,8');
  const cut = new Float32Array(16).fill(9);
  cut.set([1, 2], 0); // a window cut short of its first bar line
  commitLaterTake(cut, 2, FPB, 16);
  ok('a sub-bar window blanks its stale tail before tiling', Array.from(cut).join() === '1,2,0,0,1,2,0,0,1,2,0,0,1,2,0,0');
}

console.log(`\n=== RESULT: ${checks - failed}/${checks} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);
