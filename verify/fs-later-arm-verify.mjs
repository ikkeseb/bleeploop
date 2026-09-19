// Later-track arm arithmetic with the production armSplitAt helper and a discrete batch fixture.
// Clean streams establish frame identity across rates, BPMs and batch sizes. The separate overrun
// case preserves the legacy unframed transport as a counterexample; it does NOT model the shipped
// timestamped packet ring or its take rejection. Those run in fs-capture-packets-verify.mjs and the
// real application probes capture-clock.mjs / overdub-window.mjs.
//
// This fixture advances its clock between batches only. It cannot establish currentTime stability
// inside a synchronous task; capture-clock.mjs measures that independently in the actual browser.

let fails = 0, checks = 0;
function ok(name, cond, detail = '') {
  checks++;
  if (!cond) { fails++; console.log(`  FAIL  ${name}  ${detail}`); }
}
import { framesPerBar } from '../src/audio/quantize.ts';
import { armSplitAt, framesToBoundary, nextBoundaryTime } from '../src/audio/looper/grid-math.ts';

// ── A discrete capture rig ─────────────────────────────────────────────────────────────
// produced[] = the true capture-stream frame index sequence. We model the ring as a count of
// produced-but-not-yet-drained frames. A drain tick pops all available. consume gets them.
class Rig {
  constructor(sr, ringCap = 65536) {
    this.sr = sr;
    // Deliberately a SMALL modelled cap, not looper/state.ts's real RING_CAPACITY_FRAMES (bumped to
    // 524288 ≈ 11s on 2026-07-01 to cover the R2/R5 multi-second-freeze overrun case).
    // This rig models the overrun MECHANISM, so a small cap keeps the stall scenario
    // below cheap to simulate; it is a model parameter, not the production value.
    this.ringCap = ringCap;
    this.produced = 0;            // total frames the worklet has produced (== render-clock frames)
    this.drained = 0;             // total frames popped by drain ticks (consumed or stale-dropped)
    this.ringFill = 0;            // frames currently in the ring (produced - drained), capped at cap-1
    this.overruns = 0;            // frames dropped because ring was full (lossy push)
  }
  // Advance the render clock by `frames`, pushing them in 128-quanta into the ring (lossy on full).
  render(frames) {
    let left = frames;
    while (left > 0) {
      const q = Math.min(128, left);
      // ringbuf.js capacity is cap-1 usable.
      const room = (this.ringCap - 1) - this.ringFill;
      const landed = Math.min(q, Math.max(0, room));
      this.ringFill += landed;
      if (landed < q) this.overruns += (q - landed); // overrun: producer dropped the shortfall
      this.produced += q;
      left -= q;
    }
  }
  // Pop everything available (one drainTick). Returns the count popped.
  drainPop() {
    const got = this.ringFill;
    this.ringFill = 0;
    this.drained += got;
    return got;
  }
  ctxFrame() { return this.produced; } // round(ctx.currentTime*sr): the render-clock frame index
}

// consume() later-arm state for one track.
function makeLaterTake(master) {
  return { capacity: master * 2, armed: true, writeHead: 0, started: false, startStreamPos: -1, framesWritten: 0 };
}
// MIRRORS: src/audio/looper/capture.ts@331-341 sha256:c3a0b6e09d727c00  (consume: append the absolute recording window; completion is outside this fixture)
// (nextBoundary / framesToBoundary / the arm split are the REAL grid-math.ts functions — no port.)
// Faithful port of consume() later-track WRITE for a `got`-frame batch whose
// FIRST frame is at absolute capture-stream position `batchStreamStart`.
function consumeLater(t, pending, got, batchStreamStart, master) {
  let offset = 0;
  if (t.armed) {
    const split = armSplitAt(pending.startFrame, batchStreamStart, got);                   // capture.ts armSplitOffset → grid-math armSplit
    pending.v = split.pending;
    if (split.offset < 0) return;                             // still counting in — discard all
    offset = split.offset;                                    // straddle: split here
    t.armed = false;                                          // take begins (writeHead 0)
    t.writeHead = 0;
    t.started = true;
    t.startStreamPos = batchStreamStart + offset;             // absolute stream pos of take frame 0
  }
  if (!t.started) return;
  const remaining = t.capacity - t.writeHead;                     // :433
  pending.endFrame ??= pending.startFrame + master;
  const end = Math.min(got, pending.endFrame - batchStreamStart);
  const n = Math.max(0, Math.min(end - offset, remaining));
  t.writeHead += n;                                           // :436
  t.framesWritten += n;
  // finishCapture releases the recorder at endFrame; this fixture stops feeding then.
}

// ── Track 1 reference: its frame 0 capture-stream position. ──────────────────────────────
// finishRecording anchors masterStartTime = gridAnchor (ctx time) and the loop plays from there.
// For phase-identity we only need: track-1 frame 0 sits at some stream position P1, and
// masterStartTime (ctx) corresponds to that same render-clock instant => P1 == round(masterStartTime*sr).
// A later track armed later must land its frame 0 at P1 + k*master for integer k.

function run() {
  const SRS = [44100, 48000];
  const BPMS = [60, 90, 120, 137.3, 174, 220];
  const BARSS = [1, 2, 4];
  for (const sr of SRS) for (const bpm of BPMS) for (const bars of BARSS) {
    const fpb = framesPerBar(bpm, sr);
    const master = bars * fpb;
    // Track 1's frame 0 at stream position P1. Pick a realistic non-zero anchor.
    // masterStartTime (sec) and P1 (frames) are the SAME instant: P1 = round(masterStartTime*sr).
    const masterStartTimeSec = 0.337; // arbitrary
    const P1 = Math.round(masterStartTimeSec * sr);

    // ── Arm a later track at a set of press instants spread across a loop period. ──
    const period = master / sr;
    for (let k = 0; k < 8; k++) {
      const pressSec = masterStartTimeSec + period * (1 + k / 8) + 0.013; // mid-loop presses
      // the REAL nextBoundary / framesToBoundary math (grid-math.ts; playback.ts feeds it the live clock)
      const nextBoundarySec = nextBoundaryTime(masterStartTimeSec, period, pressSec);
      const pendingRecordStartFrame = framesToBoundary(masterStartTimeSec, period, pressSec, sr);

      // Rig: render up to the press instant WITH periodic drains (the setInterval runs the whole
      // time — model that so the pre-press fill never artificially overflows), then stale-drain.
      const rig = new Rig(sr);
      const drainFramesPre = Math.round(0.025 * sr);
      let toPress = Math.round(pressSec * sr);
      while (toPress > 0) { const c = Math.min(drainFramesPre, toPress); rig.render(c); rig.drainPop(); toPress -= c; }
      rig.drainPop();                           // stale-drain: ring emptied at press
      const streamPosAtPress = rig.ctxFrame();  // first NEW frame consume sees is at this stream pos

      const t = makeLaterTake(master);
      const pending = { v: pendingRecordStartFrame, startFrame: Math.round(nextBoundarySec * sr) };
      // Drain ticks every 25ms => render 25ms of frames, pop, consume — until the take fills master.
      const drainFrames = Math.round(0.025 * sr);
      let guard = 0;
      let batchStreamStart = streamPosAtPress; // absolute stream pos of the next batch's first frame
      while (t.writeHead < master && guard++ < 100000) {
        rig.render(drainFrames);
        const got = rig.drainPop();
        if (got > 0) consumeLater(t, pending, got, batchStreamStart, master);
        batchStreamStart += got;
        if (t.framesWritten >= master) break;
      }
      // ── Assertions ──
      // (1) Take frame 0 stream position must equal nextBoundary's stream position.
      const boundaryStreamPos = Math.round(nextBoundarySec * sr);
      ok(`later frame0 == boundary stream pos  sr=${sr} bpm=${bpm} bars=${bars} k=${k}`,
        Math.abs(t.startStreamPos - boundaryStreamPos) <= 1,
        `start=${t.startStreamPos} boundary=${boundaryStreamPos} diff=${t.startStreamPos - boundaryStreamPos}`);
      // (2) PHASE-IDENTITY: take frame 0 must be an INTEGER multiple of master after track-1 frame 0.
      const delta = t.startStreamPos - P1;
      const phaseOff = ((delta % master) + master) % master;
      const phaseErr = Math.min(phaseOff, master - phaseOff); // distance to nearest k*master
      ok(`later phase-locked to track1 (k*master)  sr=${sr} bpm=${bpm} bars=${bars} k=${k}`,
        phaseErr <= 1,
        `delta=${delta} phaseOff=${phaseOff} phaseErr=${phaseErr} master=${master}`);
      // (3) Length identity: exactly master frames written.
      ok(`later wrote exactly master  sr=${sr} bpm=${bpm} bars=${bars} k=${k}`,
        t.framesWritten === master, `wrote=${t.framesWritten} master=${master}`);
      ok(`no overruns on clean drain  sr=${sr} bpm=${bpm} bars=${bars} k=${k}`,
        rig.overruns === 0, `overruns=${rig.overruns}`);
    }
  }
}

// ── OVERRUN scenario: a stalled drain mid-arm drops frames; quantify the resulting shift. ──
// Historical counterexample: deliberately omit timestamps and advance the simulated origin only
// by drained frames, as the old unframed transport did. Real packet loss is rejected at commit;
// fs-capture-packets-verify and the browser capture-window probe exercise the current transport.
function runOverrun() {
  const sr = 48000, bpm = 120, bars = 2;
  const fpb = framesPerBar(bpm, sr);
  const master = bars * fpb;
  const masterStartTimeSec = 0.337;
  const period = master / sr;
  const pressSec = masterStartTimeSec + period * 1.5;
  const nextBoundarySec = masterStartTimeSec + 2 * period;
  const pendingRecordStartFrame = Math.round((nextBoundarySec - pressSec) * sr);

  const rig = new Rig(sr, 65536);
  const dpre = Math.round(0.025 * sr);
  let toP = Math.round(pressSec * sr);
  while (toP > 0) { const c = Math.min(dpre, toP); rig.render(c); rig.drainPop(); toP -= c; }
  rig.drainPop();
  const streamPosAtPress = rig.ctxFrame();

  const t = makeLaterTake(master);
  const pending = { v: pendingRecordStartFrame, startFrame: Math.round(nextBoundarySec * sr) };
  let batchStreamStart = streamPosAtPress;
  // Simulate a ~1.5s main-thread stall DURING the arm count-down: render >1.4s with NO drain
  // (this rig's MODELLED ringCap ~= 1.36s @48k — a deliberately small model parameter; the real
  // production RING_CAPACITY_FRAMES is now 524288 ≈ 11s, see the Rig constructor comment above),
  // forcing an overrun, THEN resume normal drains.
  rig.render(Math.round(1.5 * sr)); // stall: ring overflows, frames dropped
  let got = rig.drainPop();
  if (got > 0) consumeLater(t, pending, got, batchStreamStart, master);
  batchStreamStart += got;
  const drainFrames = Math.round(0.025 * sr);
  let guard = 0;
  while (t.framesWritten < master && guard++ < 100000) {
    rig.render(drainFrames);
    got = rig.drainPop();
    if (got > 0) consumeLater(t, pending, got, batchStreamStart, master);
    batchStreamStart += got;
  }
  // The legacy model counts DRAINED frames, but the stall DROPPED `overruns` frames between produced and
  // drained. So batchStreamStart (which we advanced by `got`=drained) UNDER-counts the true stream
  // position by `overruns`. The take's TRUE downbeat in real time is shifted by the dropped frames.
  const trueStartStreamPos = t.startStreamPos + rig.overruns; // realign for the silent drop
  const boundaryStreamPos = Math.round(nextBoundarySec * sr);
  console.log(`\n[legacy unframed overrun counterexample] sr=${sr} bpm=${bpm} bars=${bars}`);
  console.log(`  overruns(dropped frames) = ${rig.overruns}`);
  console.log(`  consume thought frame0 @ stream ${t.startStreamPos}; TRUE real-time @ ${trueStartStreamPos}; boundary @ ${boundaryStreamPos}`);
  const phaseShiftFrames = trueStartStreamPos - boundaryStreamPos;
  console.log(`  => take frame0 is shifted ${phaseShiftFrames} frames (${(phaseShiftFrames/sr*1000).toFixed(1)} ms) LATE off the master grid`);
  ok('legacy unframed overrun model shifts the later take off-grid',
    Math.abs(phaseShiftFrames) > 100,
    `shift=${phaseShiftFrames} frames`);
  // And it is NOT detectable from consume's own state (writeHead == master regardless):
  ok('legacy drained-count model hides loss behind writeHead == master',
    t.framesWritten === master, `wrote=${t.framesWritten}`);
}

run();
runOverrun();
console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
if (fails) process.exit(1);
