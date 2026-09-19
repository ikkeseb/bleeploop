// Free-record stop arithmetic with an exclusive absolute capture end. Musical time chooses
// complete bars with the existing quarter-beat grace; a sub-bar stop retains through the press.
// The clean-stream model supplies packet timestamps and drains to the selected end. Actual
// dispatchers, compensation and playback are exercised by record-stop-window.mjs / golden-jam.mjs.
let fails = 0, checks = 0;
function ok(name, cond, detail = '') { checks++; if (!cond) { fails++; console.log(`  FAIL  ${name}  ${detail}`); } }

import { framesPerBar } from '../src/audio/quantize.ts';
import { planCommit, planFreeStop } from '../src/audio/looper/grid-math.ts';
const MAX_LOOP_SECONDS = 60; // looper/state.ts

// MIRRORS: src/audio/looper/machine.ts@603-659 sha256:ff923f1ebff6aadb  (stopCapture: first-take end, minimum deadline and completion check)
function stopDecision({ now, downbeat, bpm, sr, writeHead, recordLen, compensation = 0, previousEnd }) {
  // Missing musical anchor is a defensive fixture. Its capture start is still known independently.
  const start = downbeat > 0 ? Math.round(downbeat * sr) + compensation : Math.round(now * sr) - writeHead;
  const frontier = start + writeHead;
  const elapsed = downbeat > 0 ? now - downbeat : 0;
  const { bars, target } = planFreeStop(elapsed, bpm, sr, recordLen);
  let end = Math.round(now * sr) + compensation;
  if (bars >= 1) end = start + target;
  end = Math.min(previousEnd ?? start + recordLen, end);
  return { defer: frontier < end, target: end - start, bars, end,
    commitBars: planCommit(end - start, bpm, sr, recordLen).bars };
}

// ── the OLD stop (pre-fix): commitMasterLoop's floor of writeHead, straight away — for contrast ──
function stopOld({ bpm, sr, writeHead, recordLen }) {
  const fpb = framesPerBar(bpm, sr);
  const maxBars = Math.max(1, Math.floor(recordLen / fpb));
  return Math.min(maxBars, Math.max(1, Math.floor(writeHead / fpb)));
}

// MIRRORS: src/audio/looper/capture.ts@334-375 sha256:c1bc32e9ca3a9e9d  (consume: timestamp end clips the append and triggers completion; overdub omitted)
function drainToCap(startWriteHead, target, batchSizes, recordLen) {
  const startFrame = 76543;
  const endFrame = startFrame + target;
  let firstFrame = startFrame + startWriteHead;
  let writeHead = startWriteHead;
  let committed = false, bi = 0, guard = 0;
  while (!committed && guard++ < 100000) {
    const count = batchSizes[bi++ % batchSizes.length];
    const end = Math.min(count, endFrame - firstFrame);
    writeHead += Math.max(0, Math.min(end, recordLen - writeHead));
    committed = firstFrame + count >= endFrame;
    firstFrame += count;
  }
  return writeHead;
}

// ════════════════════════════════════════════════════════════════════════════════════════════
console.log('=== A. RIG SCENARIO: 8 bars, stop 50ms after the downbeat, 150ms pipeline lag ===');
for (const [bpm, sr] of [[120, 44100], [120, 48000], [90, 44100], [137, 48000]]) {
  const fpb = framesPerBar(bpm, sr);
  const barSec = fpb / sr;
  const recordLen = Math.ceil(MAX_LOOP_SECONDS * sr);
  const downbeat = 100.0;
  const pressAfter = 0.050;           // human press 50 ms after the bar-9 downbeat ("on the one")
  const pipelineLag = 0.150;          // C (~100ms) + drain tick (~25ms) + margin — measured on the rig 2026-07-04
  const now = downbeat + 8 * barSec + pressAfter;
  const writeHead = Math.round((8 * barSec + pressAfter - pipelineLag) * sr); // drained frames at press
  const d = stopDecision({ now, downbeat, bpm, sr, writeHead, recordLen });
  const old = stopOld({ bpm, sr, writeHead, recordLen });
  ok(`A OLD floors to 7 (the bug) bpm=${bpm} sr=${sr}`, old === 7, `old=${old}`);
  ok(`A NEW counts 8 bars bpm=${bpm} sr=${sr}`, d.bars === 8, `bars=${d.bars}`);
  ok(`A NEW defers (tail in flight) bpm=${bpm} sr=${sr}`, d.defer === true);
  ok(`A NEW target = 8·fpb exactly bpm=${bpm} sr=${sr}`, d.target === 8 * fpb, `target=${d.target}`);
  // the deferred cap then commits EXACTLY the target, across ragged drain batches
  for (const sizes of [[128], [1024, 333, 128], [d.target - writeHead + 4096]]) {
    const wh = drainToCap(writeHead, d.target, sizes, recordLen);
    ok(`A cap commits at exactly target bpm=${bpm} [${sizes.length}b]`, wh === d.target, `wh=${wh}`);
  }
  // the wait is bounded: what's missing is at most the pipeline lag + grace worth of frames
  const bound = Math.ceil((pipelineLag + barSec / 16) * sr) + 1;
  ok(`A deferral window bounded bpm=${bpm}`, d.target - writeHead <= bound, `${d.target - writeHead} > ${bound}`);
}

console.log('=== B. mid-bar stop: immediate commit, completed-bars floor (unchanged musical rule) ===');
for (const [bpm, sr] of [[120, 44100], [90, 48000]]) {
  const fpb = framesPerBar(bpm, sr);
  const barSec = fpb / sr;
  const recordLen = Math.ceil(MAX_LOOP_SECONDS * sr);
  const downbeat = 100.0;
  // stop half-way through bar 8 (7.5 bars elapsed): keep the 7 completed bars, discard the partial
  const now = downbeat + 7.5 * barSec;
  const writeHead = Math.round((7.5 * barSec - 0.150) * sr);
  const d = stopDecision({ now, downbeat, bpm, sr, writeHead, recordLen });
  ok(`B immediate (frames already past 7 bars) bpm=${bpm}`, d.defer === false);
  ok(`B commits 7 completed bars bpm=${bpm}`, d.commitBars === 7, `got ${d.commitBars}`);
}

console.log('=== C. grace: a press ≤ quarter-beat EARLY keeps the bar; earlier drops it ===');
for (const [bpm, sr] of [[120, 44100], [200, 48000]]) {
  const fpb = framesPerBar(bpm, sr);
  const barSec = fpb / sr;
  const beatSec = barSec / 4;
  const recordLen = Math.ceil(MAX_LOOP_SECONDS * sr);
  const downbeat = 100.0;
  // pressed 60% of the grace window BEFORE the bar-4 downbeat (anticipation): still 4 bars, deferred
  {
    const now = downbeat + 4 * barSec - 0.6 * (beatSec / 4);
    const writeHead = Math.round((now - downbeat - 0.150) * sr);
    const d = stopDecision({ now, downbeat, bpm, sr, writeHead, recordLen });
    ok(`C within grace → 4 bars bpm=${bpm}`, d.bars === 4, `bars=${d.bars}`);
    ok(`C within grace → deferred bpm=${bpm}`, d.defer === true && d.target === 4 * fpb);
  }
  // pressed a HALF BEAT early (well past the grace): the 4th bar was not reached — 3 bars
  {
    const now = downbeat + 4 * barSec - beatSec / 2;
    const writeHead = Math.round((now - downbeat - 0.150) * sr);
    const d = stopDecision({ now, downbeat, bpm, sr, writeHead, recordLen });
    ok(`C past grace → 3 bars bpm=${bpm}`, d.bars === 3, `bars=${d.bars}`);
  }
}

console.log('=== D. sub-bar stop retains its tail before one-bar padding ===');
{
  const bpm = 120, sr = 44100;
  const fpb = framesPerBar(bpm, sr);
  const barSec = fpb / sr;
  const recordLen = Math.ceil(MAX_LOOP_SECONDS * sr);
  const downbeat = 100.0;
  const now = downbeat + 0.4 * barSec; // stopped 40% into the first bar
  const writeHead = Math.round((0.4 * barSec - 0.100) * sr);
  const d = stopDecision({ now, downbeat, bpm, sr, writeHead, recordLen });
  ok('D bars < 1', d.bars === 0);
  ok('D tail is deferred even below one bar', d.defer === true);
  ok('D end is the musical press, not the already-drained head', d.target === Math.round(0.4 * barSec * sr));
  ok('D full tail drains exactly to the press', drainToCap(writeHead, d.target, [128, 1024, 333], recordLen) === d.target);
  ok('D completed short take still pads to the 1-bar minimum', d.commitBars === 1);
}

console.log('=== E. maxBars clamp: the wall-clock count never exceeds whole bars that FIT the buffer ===');
{
  const bpm = 40, sr = 48000; // slow tempo: 6s bars, 10 bars max in 60s
  const fpb = framesPerBar(bpm, sr);
  const barSec = fpb / sr;
  const recordLen = Math.ceil(MAX_LOOP_SECONDS * sr);
  const maxBars = Math.floor(recordLen / fpb);
  const downbeat = 100.0;
  // pretend 12 bars elapsed on the wall clock (the cap would long since have fired in reality — this is
  // the defensive clamp): target must be maxBars, an exact whole-bar multiple <= recordLen
  const now = downbeat + 12 * barSec;
  const writeHead = recordLen - 5000;
  const d = stopDecision({ now, downbeat, bpm, sr, writeHead, recordLen });
  const target = d.defer ? d.target : d.commitBars * fpb;
  ok('E target clamped to whole bars that fit', target === maxBars * fpb, `target=${target} max=${maxBars * fpb}`);
  ok('E target <= recordLen (no zero-pad / later-track RangeError)', target <= recordLen);
}

console.log('=== F. defensive: downbeat missing (0) → immediate commit of drained frames (no defer) ===');
{
  const bpm = 120, sr = 44100;
  const recordLen = Math.ceil(MAX_LOOP_SECONDS * sr);
  const fpb = framesPerBar(bpm, sr);
  const d = stopDecision({ now: 500.0, downbeat: 0, bpm, sr, writeHead: 4 * fpb + 100, recordLen });
  ok('F no anchor → not deferred', d.defer === false);
  ok('F floors drained frames', d.commitBars === 4);
}

console.log('=== G. Compensation moves both edges equally and repeated stops cannot extend the window ===');
{
  const bpm = 120, sr = 48000, downbeat = 10, now = 10.5, writeHead = 18000;
  const recordLen = sr * 60;
  const a = stopDecision({ now, downbeat, bpm, sr, writeHead, recordLen });
  const b = stopDecision({ now, downbeat, bpm, sr, writeHead, recordLen, compensation: 7200 });
  ok('G compensation preserves the selected musical length', a.target === b.target);
  ok('G compensation shifts the absolute end', b.end - a.end === 7200);
  const again = stopDecision({ now: now + 0.1, downbeat, bpm, sr, writeHead, recordLen, compensation: 7200, previousEnd: b.end });
  ok('G repeated stop keeps the first end', again.end === b.end);
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails === 0 ? 0 : 1);
