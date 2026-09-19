// Executable verification of the PHASE-PRESERVING COMMIT math (2026-06-20).
// Ports the EXACT formulae from src/audio/looper/machine.ts commitMasterLoop (line-cited; commitMasterLoop
// moved out of the former monolithic looper.ts into machine.ts on 2026-07-01)
// and proves the fix for the by-ear finding that the looper click ran independent of, and off from, the count-in click.
//
// ROOT CAUSE the fix addresses: the old commit anchored masterStartTime = ctx.currentTime + HBL, where
// currentTime is a DRAIN-TICK moment ~30-50ms past the true counted downbeat. So the looper click +
// loop-audio re-anchored OFF the count-in grid -> an audible phase hop at commit.
//
// What this PROVES (the deterministic core; static review can't confirm the analog feel):
//   A. startOffset ∈ [0, period); gridAnchor = playAt - startOffset is EXACTLY on the count grid
//      (an integer number of loop periods from the counted downbeat firstTakeDownbeatCtx).
//   B. The loop-audio wrap points (gridAnchor + k*period) and the click downbeats
//      (gridAnchor + 4*bars*k * beatPeriod) COINCIDE on the count grid, with no accumulation.
//   C. Fixed-length: late drain-tick discovery (auto-stop δ past the downbeat) still anchors the grid
//      to the counted downbeat — startOffset stays small (≈ δ+HBL) and the FIRST wrap lands on-grid.
//   D. Free-record floored to completed bars: the grid ALWAYS phase-locks to the count downbeat — no commit
//      HOP for ANY stop point (section H sweeps every rounding zone, incl. the round-UP that hopped ~2.7 beats).
//   E. startPlayback offset re-derivation when `when` is clamped up to now stays in [0, dur), phase-correct.
//   F. The OLD anchor's phase error (the bug) is real and the NEW anchor eliminates it (contrast test).
//   G. Later-track completion/resume join at the live phase; boundary overdub restarts keep offset 0.
// What it does NOT prove (needs a by-ear check on the PC): the audible seamlessness, the analog feel of the come-in.

let fails = 0, checks = 0;
const approx = (a, b, eps) => Math.abs(a - b) <= eps;
function ok(name, cond, detail = '') {
  checks++;
  if (!cond) { fails++; console.log(`  FAIL  ${name}  ${detail}`); }
}

import { framesPerBar } from '../src/audio/quantize.ts';
import {
  HEARTBEAT_INTERNAL_LATENCY as HBL,
  commitAnchor as realCommitAnchor,
  phaseOffset,
} from '../src/audio/looper/grid-math.ts';

// ---- commitMasterLoop phase-LOCKED anchor: the REAL commitAnchor (grid-math.ts, pure) — no port ----
// The source computes playAt = ctx.currentTime + HBL at the commit instant and hands it in; this wrapper
// does the same from `commitNow` and echoes playAt for the assertions.
function commitAnchor(firstTakeDownbeatCtx, master, sr, commitNow, raw = master) {
  const playAt = commitNow + HBL;
  return { playAt, ...realCommitAnchor(firstTakeDownbeatCtx, master, sr, playAt, raw) };
}

// MIRRORS: src/audio/looper/playback.ts@55-59 sha256:95a7581734ce461a  (startPlayback — startAt clamp + startOffset)
// ---- startPlayback offset re-derivation (startAt clamp + startOffset) ----
function playbackStartOffset(when, offset, dur, currentTime) {
  const startAt = Math.max(when, currentTime);             // startPlayback: startAt
  const startOffset = dur > 0 ? (offset + (startAt - when)) % dur : 0; // startPlayback: startOffset
  return { startAt, startOffset };
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
  const beatPeriod = master / sr / (4 * bars);             // commitMasterLoop beatPeriod
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
// Port commitMasterLoop's floor quantize + the phase-locked anchor; sweep the stop point across every
// rounding zone. NB on what each assert proves: the `hop < 1e-9` zero-hop check is the always-true ANCHOR
// property — commitAnchor sets gridAnchor = playAt − ((playAt−recordStart) mod period), so (gridAnchor −
// recordStart) is an exact period-multiple for ANY master, floor OR round (empirically 0.0 ns in both). It
// does NOT distinguish the floor fix. The genuine floor-vs-round discriminator is the `master <= raw` check
// below (0 violations under floor vs ~228/480 under round — round-UP padded the tail + re-anchored to the
// stop, the ~2.5-2.7-beat hop heard on the rig). Both are kept: the anchor guarantees phase-lock, the floor keeps it pad-free.
function commitFull(recordStart, sr, bpm, recordLen, commitNow, raw) {
  const fpb = framesPerBar(bpm, sr);
  const maxBars = Math.max(1, Math.floor(recordLen / fpb));
  const bars = Math.min(maxBars, Math.max(1, Math.floor(raw / fpb)));   // FLOOR (was Math.round)
  const master = bars * fpb;
  return { fpb, bars, master, ...commitAnchor(recordStart, master, sr, commitNow, raw) };
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

console.log('=== E. startPlayback offset re-derivation (when clamped to now) ===');
{
  const dur = 2.0; // loop duration (s)
  // Normal: when is in the future, startAt==when, offset unchanged.
  let p = playbackStartOffset(100.02, 0.037, dur, 100.0);
  ok('E future when: startAt==when', p.startAt === 100.02);
  ok('E future when: offset unchanged', approx(p.startOffset, 0.037, 0), `off=${p.startOffset}`);
  // Clamp: when already 0.01s in the PAST -> startAt=now, offset advanced by (now-when) so the sounding
  // buffer position stays phase-correct.
  p = playbackStartOffset(99.99, 0.037, dur, 100.0);
  ok('E past when: startAt==now', p.startAt === 100.0);
  ok('E past when: offset advanced by clamp', approx(p.startOffset, 0.037 + 0.01, 1e-12), `off=${p.startOffset}`);
  ok('E offset stays in [0,dur)', p.startOffset >= 0 && p.startOffset < dur);
  // Clamp larger than dur wraps cleanly into [0,dur).
  p = playbackStartOffset(100.0 - 2.5, 0.1, dur, 100.0); // clamp 2.5s, dur 2.0 -> wraps
  ok('E big clamp wraps into [0,dur)', p.startOffset >= 0 && p.startOffset < dur, `off=${p.startOffset}`);
  ok('E big clamp value correct', approx(p.startOffset, (0.1 + 2.5) % dur, 1e-12), `off=${p.startOffset}`);
}

console.log('=== F. The OLD anchor had a real phase error; the NEW anchor eliminates it (contrast) ===');
for (const { sr, bpm, bars } of [{ sr: 48000, bpm: 120, bars: 2 }, { sr: 44100, bpm: 137, bars: 1 }]) {
  const master = bars * framesPerBar(bpm, sr);
  const period = master / sr;
  const recordStart = 50.0;
  const delta = 0.041; // drain-tick latency past the true downbeat
  const commitNow = recordStart + period + delta;
  // OLD: anchor = commitNow + HBL (the bug). NEW: phase-preserving gridAnchor.
  const oldAnchor = commitNow + HBL;
  const r = commitAnchor(recordStart, master, sr, commitNow);
  // The count grid's NEXT downbeat after the take is recordStart + period (where the heard click was).
  const countDownbeat = recordStart + period;
  const oldErr = Math.abs(((oldAnchor - countDownbeat) % period + period) % period);
  const oldErrSigned = Math.min(oldErr, period - oldErr); // distance to nearest count-grid downbeat
  const newErr = Math.min(
    ((r.gridAnchor - countDownbeat) % period + period) % period,
    period - ((r.gridAnchor - countDownbeat) % period + period) % period);
  ok(`F OLD anchor is audibly off-grid sr=${sr} bpm=${bpm} (err=${(oldErrSigned * 1e3).toFixed(1)}ms)`,
    oldErrSigned > 0.03, `oldErr=${(oldErrSigned * 1e3).toFixed(2)}ms (should be ~δ+HBL=${((delta + HBL) * 1e3).toFixed(0)}ms)`);
  ok(`F NEW anchor is ON the count grid sr=${sr} bpm=${bpm}`,
    newErr < 1e-9, `newErr=${(newErr * 1e9).toFixed(2)}ns`);
  console.log(`    sr=${sr} bpm=${bpm} bars=${bars}: OLD off by ${(oldErrSigned * 1e3).toFixed(1)}ms, NEW off by ${(newErr * 1e9).toFixed(2)}ns`);
}

console.log('=== G. Live-phase joins and boundary restarts stay phase-locked ===');
{
  const dur = 2.0;
  const masterStart = 200.0;
  const currentTime = 200.75;
  // finishLaterRecording and resume use this exact live-phase pattern: schedule with the heartbeat lead,
  // derive the offset from the shared master anchor (the REAL grid-math.ts phaseOffset), then let
  // startPlayback apply any late clamp correction.
  const when = currentTime + HBL;
  const offset = phaseOffset(when, masterStart, dur);
  const live = playbackStartOffset(when, offset, dur, currentTime);
  ok('G later/resume join after heartbeat lead', live.startAt === when);
  ok('G later/resume join at the current master phase', approx(live.startOffset, 0.77, 1e-12));
  ok('G later/resume next wrap lands on the master grid',
    isIntegerMultiple(live.startAt + (dur - live.startOffset) - masterStart, dur));

  // endOverdub still swaps on a future boundary with the default source offset 0.
  const boundary = playbackStartOffset(202.0, 0, dur, currentTime);
  ok('G overdub boundary restart: offset 0', boundary.startOffset === 0);
  ok('G overdub boundary restart: starts at boundary', boundary.startAt === 202.0);
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails === 0 ? 0 : 1);
