// Executable verification of the PHASE-PRESERVING COMMIT: the real grid-math commit math (planCommit,
// commitAnchor, phaseOffset) swept across tempos, lengths and commit instants, plus the REAL
// startPlayback clamp, resume and later-take join driven through the verify rig.
//
// Background: an earlier commit anchored the grid to ctx.currentTime + HBL at a drain tick ~30-50 ms past
// the counted downbeat, so the looper click and the loop audio re-anchored off the count-in grid (an
// audible phase hop). The anchor is now the counted downbeat, an integer number of periods back.
//
// What this PROVES (the deterministic core; static review can't confirm the analog feel):
//   A. startOffset ∈ [0, period); gridAnchor = playAt - startOffset is EXACTLY on the count grid.
//   B. The loop-audio wrap points and the click downbeats COINCIDE on the count grid, with no accumulation.
//   C. Fixed-length: late drain-tick discovery still anchors the grid to the counted downbeat.
//   D. Free-record floored to completed bars: the grid always phase-locks to the count downbeat; H sweeps
//      every rounding zone with the real planCommit.
//   E. The real startPlayback: a `when` clamped up to now advances the offset by the clamp, wrapped into
//      [0, dur); a future `when` is kept exactly.
//   G. The real resume and later-take commit join at the live master phase; R: a real first take's
//      source and grid sit on the counted downbeat.
// What it does NOT prove (needs a by-ear check on the PC): the audible seamlessness, the analog feel of the come-in.

let fails = 0, checks = 0;
const approx = (a, b, eps) => Math.abs(a - b) <= eps;
function ok(name, cond, detail = '') {
  checks++;
  if (!cond) { fails++; console.log(`  FAIL  ${name}  ${detail}`); }
}

import { framesPerBar } from '../../src/audio/quantize.ts';
import {
  HEARTBEAT_INTERNAL_LATENCY as HBL,
  commitAnchor as realCommitAnchor,
  planCommit,
} from '../../src/audio/looper/grid-math.ts';
import { bootLooper } from '../harness/rig.ts';

// ---- commitMasterLoop phase-LOCKED anchor: the REAL commitAnchor (grid-math.ts, pure) — no port ----
// The source computes playAt = ctx.currentTime + HBL at the commit instant and hands it in; this wrapper
// does the same from `commitNow` and echoes playAt for the assertions.
function commitAnchor(firstTakeDownbeatCtx, master, sr, commitNow, raw = master) {
  const playAt = commitNow + HBL;
  return { playAt, ...realCommitAnchor(firstTakeDownbeatCtx, master, sr, playAt, raw) };
}

// integer test: is x an integer multiple of p (to a sub-nanosecond residual)?
const isIntegerMultiple = (x, p, eps = 1e-9) => {
  const k = Math.round(x / p);
  return Math.abs(x - k * p) <= eps;
};

const CFG = [];
for (const sr of [48000, 44100])
  for (const bpm of [120, 90, 137, 100, 200, 73.5])
    for (const bars of [1, 2, 4, 8, 3])
      CFG.push({ sr, bpm, bars });

console.log('=== A. gridAnchor lands EXACTLY on the count grid; startOffset ∈ [0, period) ===');
for (const { sr, bpm, bars } of CFG) {
  const master = bars * framesPerBar(bpm, sr);
  const period = master / sr;
  const recordStart = 1234.5678;                  // the counted come-in downbeat (firstTakeDownbeatCtx)
  // Sweep the commit instant across a whole period of possible drain-tick discoveries (+ multiple loops).
  for (const frac of [0.0001, 0.013, 0.25, 0.5, 0.731, 0.999, 1.0, 1.5, 2.34, 5.0]) {
    const commitNow = recordStart + frac * period;
    const r = commitAnchor(recordStart, master, sr, commitNow);
    ok(`A offset∈[0,period) sr=${sr} bpm=${bpm} bars=${bars} frac=${frac}`,
      r.startOffset >= 0 && r.startOffset < r.period,
      `off=${r.startOffset} period=${r.period}`);
    ok(`A gridAnchor==playAt-offset sr=${sr} bpm=${bpm} bars=${bars} frac=${frac}`,
      approx(r.gridAnchor, r.playAt - r.startOffset, 0), '');
    // The load-bearing property: gridAnchor is an integer number of loop periods from the counted downbeat.
    ok(`A gridAnchor on count grid sr=${sr} bpm=${bpm} bars=${bars} frac=${frac}`,
      isIntegerMultiple(r.gridAnchor - recordStart, period),
      `(gridAnchor-recordStart)/period=${(r.gridAnchor - recordStart) / period}`);
    // gridAnchor is the downbeat at/just-before playAt (so the buffer offset is forward, never negative).
    ok(`A gridAnchor<=playAt<gridAnchor+period sr=${sr} bpm=${bpm} bars=${bars} frac=${frac}`,
      r.gridAnchor <= r.playAt + 1e-12 && r.playAt < r.gridAnchor + r.period + 1e-12, '');
  }
}

console.log('=== B. loop-audio wraps == click downbeats, on the count grid, no accumulation ===');
for (const { sr, bpm, bars } of CFG) {
  const master = bars * framesPerBar(bpm, sr);
  const period = master / sr;
  const { beatPeriod } = planCommit(master, bpm, sr, master); // the pulse period the commit starts
  const recordStart = 7.0;
  const r = commitAnchor(recordStart, master, sr, recordStart + period + 0.031); // fixed-length-ish
  // 4*bars beats span exactly one loop period (the click's per-loop downbeat == the audio wrap).
  ok(`B 4*bars*beatPeriod==period sr=${sr} bpm=${bpm} bars=${bars}`,
    approx(4 * bars * beatPeriod, period, 1e-9), `${4 * bars * beatPeriod} vs ${period}`);
  const oneSample = 1 / sr;
  let maxOff = 0, firstOff = null, lastOff = 0;
  const loops = Math.floor((30 * 60) / period);            // 30 minutes of loops
  for (let k = 0; k <= loops; k++) {
    const audioWrap = r.gridAnchor + k * period;            // src.start wrap to frame 0
    const clickDownbeat = r.gridAnchor + (4 * bars * k) * beatPeriod; // master pulse downbeat for loop k
    const off = Math.abs(audioWrap - clickDownbeat);
    maxOff = Math.max(maxOff, off);
    if (firstOff === null) firstOff = off; lastOff = off;
    // and both are on the count grid (integer periods from the counted downbeat)
    if (k === loops) ok(`B last wrap on count grid sr=${sr} bpm=${bpm} bars=${bars}`,
      isIntegerMultiple(audioWrap - recordStart, period, 1e-6), '');
  }
  ok(`B wrap==downbeat <1 sample sr=${sr} bpm=${bpm} bars=${bars} (${loops} loops)`,
    maxOff < oneSample, `maxOff=${(maxOff * 1e9).toFixed(2)}ns`);
  ok(`B no accumulation sr=${sr} bpm=${bpm} bars=${bars}`,
    maxOff < 1e-6, `first=${(firstOff * 1e9).toFixed(2)}ns last=${(lastOff * 1e9).toFixed(2)}ns`);
}

console.log('=== C. Fixed-length: late drain discovery still anchors to the counted downbeat ===');
for (const { sr, bpm, bars } of CFG) {
  const master = bars * framesPerBar(bpm, sr);
  const period = master / sr;
  const recordStart = 3.0;
  // Auto-stop is discovered on a drain tick δ AFTER the true downbeat (recordStart + period). δ models
  // ring + drain-tick latency: realistic 5..60ms.
  for (const delta of [0.005, 0.021, 0.048, 0.060]) {
    const commitNow = recordStart + period + delta;        // consume() sees writeHead>=target this late
    const r = commitAnchor(recordStart, master, sr, commitNow);
    // n=1: the grid anchor is the FIRST counted downbeat (recordStart + period), not the commit instant.
    ok(`C gridAnchor==recordStart+period sr=${sr} bpm=${bpm} bars=${bars} δ=${delta}`,
      approx(r.gridAnchor, recordStart + period, 1e-9),
      `gridAnchor-recordStart=${(r.gridAnchor - recordStart)} period=${period}`);
    // startOffset ≈ δ+HBL (small) -> playback starts essentially at frame 0, gapless AND phase-correct.
    ok(`C startOffset≈δ+HBL sr=${sr} bpm=${bpm} bars=${bars} δ=${delta}`,
      approx(r.startOffset, delta + HBL, 1e-9) && r.startOffset < period,
      `off=${r.startOffset} expected≈${delta + HBL}`);
    // the FIRST audio wrap (frame 0) lands on the count grid one period later (= recordStart + 2*period).
    const firstWrap = r.playAt + (r.period - r.startOffset);
    ok(`C first wrap on count grid sr=${sr} bpm=${bpm} bars=${bars} δ=${delta}`,
      approx(firstWrap, recordStart + 2 * period, 1e-9) && isIntegerMultiple(firstWrap - recordStart, period),
      `firstWrap-recordStart=${(firstWrap - recordStart)} vs ${2 * period}`);
  }
}

console.log('=== D1. Free-record rounded-DOWN (raw >= master): phase-preserve on grid, start in real audio ===');
for (const { sr, bpm, bars } of CFG) {
  const fpb = framesPerBar(bpm, sr);
  const master = bars * fpb;
  const period = master / sr;
  const recordStart = 11.0;

  // D1 — rounded DOWN: the player plays a HAIR MORE than `bars` bars (raw >= master) -> phase-preserve, on grid,
  // and startOffset lands in REAL audio (offset = the small overshoot, < a bit).
  {
    const overshootFrames = Math.round(0.12 * fpb);        // played ~0.12 bar past the downbeat
    const raw = master + overshootFrames;                  // raw > master -> quantizes DOWN to `bars`
    const stopAt = recordStart + raw / sr;
    const r = commitAnchor(recordStart, master, sr, stopAt, raw);
    ok(`D1 rounded-down gridAnchor on grid sr=${sr} bpm=${bpm} bars=${bars}`,
      isIntegerMultiple(r.gridAnchor - recordStart, period, 1e-6),
      `(gridAnchor-recordStart)/period=${(r.gridAnchor - recordStart) / period}`);
    ok(`D1 offset∈[0,period) sr=${sr} bpm=${bpm} bars=${bars}`,
      r.startOffset >= 0 && r.startOffset < r.period, `off=${r.startOffset}`);
    // startOffset (the buffer position at playAt) must land in REAL audio, i.e. < master frames AND, since
    // raw>=master means the whole buffer is real audio, anywhere in [0,period) is real. Confirm < period.
    ok(`D1 startOffset in real audio (no pad) sr=${sr} bpm=${bpm} bars=${bars}`,
      r.startOffset * sr < master + 1e-6, `offFrames=${r.startOffset * sr} master=${master}`);
  }

}

console.log('=== D2. Sub-1-bar free-record (floored UP to 1 bar): phase-lock + start on next downbeat, no hop ===');
// Under FLOOR, the ONLY raw < master case is stopping BEFORE finishing bar 1 (floored up to the 1-bar
// minimum -> a silence pad [raw..master)). The grid still phase-locks (no hop) and playback is delayed to
// the next counted downbeat from frame 0 (start on the next "1"). Not bars-dependent, so its own section.
for (const { sr, bpm } of [{ sr: 48000, bpm: 120 }, { sr: 44100, bpm: 137 }, { sr: 48000, bpm: 90 }]) {
  const fpb = framesPerBar(bpm, sr);
  const master = fpb;                 // floored up to 1 bar
  const period = master / sr;
  const recordStart = 11.0;
  for (const playedBars of [0.3, 0.5, 0.85, 0.97]) {
    const raw = Math.round(playedBars * fpb);             // raw < master
    const stopAt = recordStart + raw / sr;
    const r = commitAnchor(recordStart, master, sr, stopAt, raw);
    const m = ((r.gridAnchor - recordStart) % period + period) % period;
    const hop = Math.min(m, period - m);                  // distance to nearest count-grid downbeat
    ok(`D2 no hop (grid on count downbeat) sr=${sr} bpm=${bpm} played=${playedBars}`,
      hop < 1e-9, `hop=${(hop * 1e3).toFixed(2)}ms`);
    ok(`D2 startOffset==0 sr=${sr} bpm=${bpm} played=${playedBars}`, r.startOffset === 0, `off=${r.startOffset}`);
    ok(`D2 playWhen==next downbeat sr=${sr} bpm=${bpm} played=${playedBars}`,
      approx(r.playWhen, r.gridAnchor + period, 1e-9), `playWhen=${r.playWhen} expected=${r.gridAnchor + period}`);
    ok(`D2 playWhen>playAt (future start) sr=${sr} bpm=${bpm} played=${playedBars}`,
      r.playWhen > r.playAt - 1e-12, `playWhen=${r.playWhen} playAt=${r.playAt}`);
    ok(`D2 frame0 ('1') lands on count grid sr=${sr} bpm=${bpm} played=${playedBars}`,
      isIntegerMultiple(r.playWhen - recordStart, period, 1e-6),
      `(playWhen-recordStart)/period=${(r.playWhen - recordStart) / period}`);
  }
}

console.log('=== H. FLOOR quantize keeps the COMPLETED bars (master<=raw, no tail pad) — the floor-specific proof — 2026-06-26 ===');
// The real planCommit floor + the phase-locked anchor; sweep the stop point across every rounding zone. NB on what each assert proves: the `hop < 1e-9` zero-hop check is the always-true ANCHOR
// property — commitAnchor sets gridAnchor = playAt − ((playAt−recordStart) mod period), so (gridAnchor −
// recordStart) is an exact period-multiple for ANY master, floor OR round (empirically 0.0 ns in both). It
// does NOT distinguish the floor fix. The genuine floor-vs-round discriminator is the `master <= raw` check
// below (0 violations under floor vs ~228/480 under round — round-UP padded the tail + re-anchored to the
// stop, the ~2.5-2.7-beat hop heard on the rig). Both are kept: the anchor guarantees phase-lock, the floor keeps it pad-free.
function commitFull(recordStart, sr, bpm, recordLen, commitNow, raw) {
  const plan = planCommit(raw, bpm, sr, recordLen);
  return { ...plan, ...commitAnchor(recordStart, plan.master, sr, commitNow, raw) };
}
for (const sr of [48000, 44100]) {
  for (const bpm of [120, 90, 137, 100, 73.5]) {
    const fpb = framesPerBar(bpm, sr);
    const recordLen = Math.ceil(60 * sr);                  // the real 60 s record buffer
    const recordStart = 100.0;
    // Stop points 0.3 .. 5.7 bars in 0.1-bar steps — covers round-DOWN, round-UP, exact, and multi-bar.
    for (let pb = 0.3; pb <= 5.71; pb += 0.1) {
      const raw = Math.round(pb * fpb);
      const stopAt = recordStart + raw / sr;
      const r = commitFull(recordStart, sr, bpm, recordLen, stopAt, raw);
      const period = r.master / sr;
      const m = ((r.gridAnchor - recordStart) % period + period) % period;
      const hop = Math.min(m, period - m);
      ok(`H zero hop sr=${sr} bpm=${bpm} played=${pb.toFixed(1)}bar (bars=${r.bars})`,
        hop < 1e-9, `hop=${(hop * 1e3).toFixed(2)}ms bars=${r.bars} master=${r.master}`);
      if (raw >= fpb) {
        // >= 1 completed bar: floor keeps the completed bars (master <= raw, no pad) and starts seamlessly.
        ok(`H floor keeps completed bars (master<=raw) sr=${sr} bpm=${bpm} played=${pb.toFixed(1)}`,
          r.master <= raw, `master=${r.master} raw=${raw}`);
        ok(`H >=1bar seamless start (playWhen==playAt) sr=${sr} bpm=${bpm} played=${pb.toFixed(1)}`,
          approx(r.playWhen, r.playAt, 1e-12), `playWhen=${r.playWhen} playAt=${r.playAt}`);
      }
    }
  }
}

console.log('=== E. the real startPlayback: offset re-derivation when `when` is clamped up to now ===');
{
  const rig = await bootLooper({ sampleRate: 48000, startTime: 100 });
  const playback = await rig.import('audio/looper/playback.ts');
  const dur = 2.0;
  const buf = rig.ctx.createBuffer(1, dur * 48000, 48000);
  const now = rig.now();
  const start = (when, offset) => {
    playback.startPlayback(1, buf, when, offset);
    return rig.sources().at(-1);
  };
  let src = start(now + 0.02, 0.037);
  ok('E future when: starts at when', src.startTime === now + 0.02);
  ok('E future when: offset unchanged', src.offset === 0.037, `off=${src.offset}`);
  ok('E the source loops the whole buffer', src.loop === true && src.loopStart === 0 && src.loopEnd === dur);
  src = start(now - 0.01, 0.037);
  ok('E past when: starts now', src.startTime === now);
  ok('E past when: offset advanced by the clamp', approx(src.offset, 0.037 + 0.01, 1e-12), `off=${src.offset}`);
  ok('E past when: buffer position at start stays phase-correct', approx(src.startTime - src.offset, now - 0.01 - 0.037, 1e-12));
  src = start(now - 2.5, 0.1); // a clamp longer than the loop wraps
  ok('E big clamp wraps into [0,dur)', src.offset >= 0 && src.offset < dur, `off=${src.offset}`);
  ok('E big clamp value correct', approx(src.offset, (0.1 + 2.5) % dur, 1e-12), `off=${src.offset}`);
  const prev = rig.sources().at(-2);
  ok('E the previous source retires exactly at the new start', prev.stopTime === src.startTime);
}

/** A committed first take on lane 0, returned with its counted downbeat. */
async function committedTake({ bpm, sr, bars, stopAfter }) {
  const rig = await bootLooper({ sampleRate: sr, startTime: 30 });
  rig.clock.setBpm(bpm);
  rig.setInput(0.5);
  const mark = rig.draws().length;
  await rig.looper.recDub(0);
  const beat = 60 / bpm;
  const downbeat = rig.draws().slice(mark).find((b) => b.countLeft === 4).time + 4 * beat;
  await rig.advanceTo(downbeat + bars * 4 * beat + stopAfter);
  await rig.looper.recDub(0);
  await rig.advance(0.3);
  const master = rig.looper.masterLengthFrames();
  return { rig, downbeat, master, period: master / sr, anchor: rig.state.engineState.masterStartTime };
}
/** The loop wrap (buffer frame 0) this source reaches first, minus the grid anchor, in periods. */
const wrapPhase = (src, anchor, period) => {
  const x = (src.startTime - src.offset - anchor) / period;
  return x - Math.round(x);
};

console.log('=== R. a real first take: source and grid on the counted downbeat ===');
for (const [bpm, sr, bars, stopAfter] of [[120, 48000, 2, 0.04], [137, 44100, 1, 0.09], [90, 48000, 3, 0.2], [200, 48000, 4, 0.01]]) {
  const { rig, downbeat, master, period, anchor } = await committedTake({ bpm, sr, bars, stopAfter });
  const tag = `bpm=${bpm} sr=${sr} bars=${bars}`;
  ok(`R ${tag} committed ${bars} bars`, master === bars * framesPerBar(bpm, sr), `master=${master}`);
  ok(`R ${tag} the grid anchor is a whole number of periods from the counted downbeat`,
    isIntegerMultiple(anchor - downbeat, period, 1e-9), `(anchor-downbeat)/period=${(anchor - downbeat) / period}`);
  const src = rig.tracks[0].source;
  ok(`R ${tag} the first source wraps on the grid`, Math.abs(wrapPhase(src, anchor, period)) < 1e-9,
    `phase=${wrapPhase(src, anchor, period)}`);
  ok(`R ${tag} it starts inside the loop`, src.offset >= 0 && src.offset < period);
}

console.log('=== G. the real later-take commit and resume join at the live master phase; idle PLAY re-anchors ===');
for (const [bpm, sr] of [[120, 48000], [137, 44100]]) {
  const { rig, period, anchor } = await committedTake({ bpm, sr, bars: 2, stopAfter: 0.05 });
  const tag = `bpm=${bpm} sr=${sr}`;
  // A later take on lane 2 commits at an arbitrary phase and must join the same grid.
  rig.setInput(0.25);
  await rig.looper.recDub(1);
  const next = anchor + (Math.floor((rig.now() - anchor) / period) + 1) * period;
  await rig.advanceTo(next + 0.6 * period); // 1.2 of 2 bars: commits one bar, tiled, at the press
  await rig.looper.recDub(1);
  await rig.advance(0.1);
  const later = rig.tracks[1].source;
  ok(`G ${tag} the later take commits`, rig.looper.trackInfo(1).state === 'PLAYING' && later !== null);
  ok(`G ${tag} the later take joins at the master phase`, later !== null && Math.abs(wrapPhase(later, anchor, period)) < 1e-9 &&
    later.offset >= 0 && later.offset < period, `phase=${later && wrapPhase(later, anchor, period)}`);
  // Lane 2 stops and resumes while lane 1 keeps the transport running: a live-phase join.
  rig.looper.playStop(1);
  await rig.advance(0.77 * period);
  const pressed = rig.now();
  rig.looper.playStop(1);
  const resumed = rig.tracks[1].source;
  ok(`G ${tag} resume starts after the heartbeat lead`, approx(resumed.startTime, pressed + HBL, 1e-9),
    `start-press=${resumed.startTime - pressed}`);
  ok(`G ${tag} resume joins at the current master phase`, Math.abs(wrapPhase(resumed, anchor, period)) < 1e-9 &&
    resumed.offset >= 0 && resumed.offset < period, `phase=${wrapPhase(resumed, anchor, period)} off=${resumed.offset}`);
  ok(`G ${tag} the grid anchor did not move`, rig.state.engineState.masterStartTime === anchor);
  // Both lanes stopped: PLAY re-anchors the grid at the press (+ lead) and starts from the top.
  rig.looper.playStop(0);
  rig.looper.playStop(1);
  await rig.advance(0.4 * period);
  const idlePress = rig.now();
  rig.looper.playStop(0);
  const top = rig.tracks[0].source;
  ok(`G ${tag} idle PLAY starts from the top after the lead`, top.offset === 0 && approx(top.startTime, idlePress + HBL, 1e-9));
  ok(`G ${tag} idle PLAY re-anchors the grid on its start`, rig.state.engineState.masterStartTime === top.startTime);
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails === 0 ? 0 : 1);
