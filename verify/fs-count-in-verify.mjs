// Executable verification of the first-track COUNT-IN (record-UX fix, design 2026-06-19).
// Faithful Node port of the count-in logic in src/audio/looper/machine.ts (startRecording) +
// src/audio/looper/capture.ts (consume) + src/audio/clock.ts (cited by line). looper.ts was split into
// src/audio/looper/{state,capture,peaks,playback,machine,mixer}.ts + a facade on 2026-07-01;
// none of the split files can be imported in Node (Web Audio deps),
// so this mirrors the exact transitions by line. Same idiom as fs-grid-verify.mjs /
// fs-looper-arm-verify.mjs.
//
// What this PROVES (the deterministic core; the analog feel is owed a by-ear check on the PC):
//   - the count discards EXACTLY pendingRecordStartFrame captured frames (lead-in + count bar),
//     then the take begins at frame 0 with the post-count content — NO dead air, frame-exact   [capture.ts consume]
//   - that property holds however the capture batches straddle the boundary (mid-batch, on the
//     batch edge, across many batches) — it counts FRAMES, not wall-clock, so it's jitter-free
//   - pendingRecordStartFrame = round((HBL + COUNT_IN_BEATS*beatPeriod)*sr) across bpm/sr        [machine.ts startRecording]
//   - the 4 count clicks are FORCED audible even with the metronome OFF; beats >= 4 honor it,
//     and the accent lands on beat 0 of every bar (count "1" and the come-in "1")               [pulseTick]
//   - stop/clear DURING the count aborts to EMPTY (no dead silent loop), clears the shared
//     arm-count, and tears down the count pulse (restore the free-run grid)                     [stopRecording/stop]
//   - the later-track arm (master>0) is UNREGRESSED by the stopRecording refactor               [regression]

let fails = 0, checks = 0;
const approx = (a, b, eps) => Math.abs(a - b) <= eps;
function ok(name, cond, detail = '') { checks++; if (!cond) { fails++; console.log(`  FAIL  ${name}  ${detail}`); } }

// ── Constants mirrored from the source ──────────────────────────────────────────────────────
import {
  COUNT_IN_BEATS,
  HEARTBEAT_INTERNAL_LATENCY as HBL,
  armSplitAt,
  countInArm,
} from '../src/audio/looper/grid-math.ts';
const FLT = 1e-6;            // Float32Array store tolerance (0.7 -> 0.69999998)

// ── startRecording first-track count-in branch (looper/machine.ts) ──────────────────────────
// recordStart = anchor + COUNT_IN_BEATS*beatPeriod;  anchor = now + HBL.
// pendingRecordStartFrame = round((recordStart - now)*sr) = round((HBL + N*beatPeriod)*sr).
// The REAL grid-math.ts countInArm — no port. (HBL/COUNT_IN_BEATS above are imported for the assertions.)
function armCountIn(now, bpm, sr) {
  const a = countInArm(now, bpm, sr);
  return { beatPeriod: a.beatPeriod, anchor: a.anchor, recordStart: a.recordStart, pending: a.pendingFrames };
}

// MIRRORS: src/audio/looper/capture.ts@282-297 sha256:644d6403f64574d2  (armSplitOffset: retain the batch suffix after the absolute start)
// MIRRORS: src/audio/looper/capture.ts@331-341 sha256:c3a0b6e09d727c00  (consume: recording append, below the completion end in these fixtures)
// (the split itself is the REAL grid-math.ts armSplitAt; the write is the port)
// ── consume() first-track armed branch — the count-in arm split ──
// Models the recording prefix before the completion deadline: discard exactly `pending` frames, then linear-append the
// take from frame 0. Returns nothing; mutates the track + the shared `pending` via the closure obj.
function makeTrack(cap) {
  return { state: 'RECORDING', armed: false, writeHead: 0, fillFrames: 0, lengthFrames: 0,
           record: new Float32Array(cap) };
}
function consumeFirst(state, t, data) {
  const count = data.length;
  const firstFrame = state.frame ?? 0;
  state.startFrame ??= firstFrame + state.pending;
  state.frame = firstFrame + count;
  let offset = 0;
  if (t.armed) {                                   // count-in arm split (the real armSplit)
    const split = armSplitAt(state.startFrame, firstFrame, count);
    state.pending = split.pending;
    if (split.offset < 0) return;                  // still counting in — discard
    offset = split.offset;
    t.armed = false; t.writeHead = 0; t.fillFrames = 0;
  }
  const room = t.record.length - t.writeHead;
  const end = Math.min(count, (state.startFrame + t.record.length) - firstFrame);
  const n = Math.max(0, Math.min(end - offset, room));
  t.record.set(data.subarray(offset, offset + n), t.writeHead);
  t.writeHead += n; t.fillFrames = t.writeHead;
}

// ── stopRecording armed-abort, hoisted to cover BOTH count-in (master===0) and later (master>0) ─
function stopRecording(state, t, master) {
  if (t.armed) {
    const wasCountIn = master === 0;
    t.armed = false;
    state.activeRecordIndex = -1;
    state.pending = 0;
    t.state = t.lengthFrames > 0 ? 'STOPPED' : 'EMPTY';
    if (wasCountIn) state.countPulseTornDown = true;  // clock.stopCountIn() -> restore free-run grid
    return;
  }
  // (not armed) completion and compensated stop run in record-stop-window.mjs; not modelled here.
}

// ── pulseTick forced-click decision (clock.ts) — count beats forced, later beats metronome-gated ─
function clickDecision(nextN, forcedUntilN, metronomeOn) {
  const beatInBar = nextN % 4;
  const forced = nextN < forcedUntilN;
  return { fires: forced || metronomeOn, accent: beatInBar === 0 };
}

// ════════════════════════════════════════════════════════════════════════════════════════════
console.log('=== A. pendingRecordStartFrame = round((HBL + N*beatPeriod)*sr) across bpm/sr ===');
for (const sr of [48000, 44100]) {
  for (const bpm of [120, 90, 137, 100, 200, 73.5]) {
    const { beatPeriod, recordStart, anchor, pending } = armCountIn(1000, bpm, sr);
    const expect = Math.round((HBL + COUNT_IN_BEATS * beatPeriod) * sr);
    ok(`A pending exact bpm=${bpm} sr=${sr}`, pending === expect, `${pending} vs ${expect}`);
    // recordStart is the come-in downbeat = anchor + one bar of count.
    ok(`A recordStart = anchor+4beats bpm=${bpm} sr=${sr}`,
       approx(recordStart - anchor, COUNT_IN_BEATS * beatPeriod, 1e-12));
    // a one-bar count at this tempo is the expected couple of seconds (sanity, not laggy multi-bar).
    ok(`A count duration ~one bar bpm=${bpm}`, approx(recordStart - 1000 - HBL, 4 * 60 / bpm, 1e-9));
  }
}

console.log('=== B. NO dead air: count discarded, take starts at frame 0 (frame-exact) ===');
// Feed capture where the count region carries a MARKER value (0.5) that MUST be discarded, and the
// take region carries 0.7. After consume, record[0] must be the take (0.7), never the marker.
function runCapture(pending, takeFrames, batchSizes) {
  const cap = pending + takeFrames + 4096;
  const t = makeTrack(cap); t.armed = true;
  const state = { pending };
  // Build the full stream: [pending frames of 0.5][takeFrames of 0.7].
  const stream = new Float32Array(pending + takeFrames);
  stream.fill(0.5, 0, pending);
  stream.fill(0.7, pending, pending + takeFrames);
  let pos = 0, bi = 0;
  while (pos < stream.length) {
    const bs = Math.min(batchSizes[bi++ % batchSizes.length], stream.length - pos);
    consumeFirst(state, t, stream.subarray(pos, pos + bs));
    pos += bs;
  }
  return { t, state };
}
{
  const pending = 96960; // ~ (0.02 + 4*0.5)s @48k (120bpm)
  const take = 96000;    // one bar @120/48k
  // Three batch regimes: the boundary falls mid-batch, exactly on a batch edge, and across many
  // tiny batches (a stalled-then-bursty drain). All must land frame-exact.
  for (const [label, sizes] of [
    ['mid-batch (128 quanta)', [128]],
    ['boundary on a batch edge', [pending, 4096]],
    ['ragged drain', [1024, 333, 5000, 128, 999, 20000]],
    ['one giant batch straddling', [pending + take]],
  ]) {
    const { t, state } = runCapture(pending, take, sizes);
    ok(`B took exactly ${take} frames [${label}]`, t.writeHead === take, `writeHead=${t.writeHead}`);
    ok(`B armed cleared [${label}]`, t.armed === false);
    ok(`B pending fully consumed [${label}]`, state.pending === 0, `pending=${state.pending}`);
    ok(`B frame 0 is the TAKE not the count [${label}]`, approx(t.record[0], 0.7, FLT), `record[0]=${t.record[0]}`);
    ok(`B last take frame present [${label}]`, approx(t.record[take - 1], 0.7, FLT));
    // The marker (0.5) must appear NOWHERE in the recorded region — the count bar is gone.
    let markerLeak = false;
    for (let k = 0; k < take; k++) if (approx(t.record[k], 0.5, FLT)) { markerLeak = true; break; }
    ok(`B no count-bar leak (dead air discarded) [${label}]`, !markerLeak);
  }
}

console.log('=== B2. partial come-in: take shorter than the count (still frame-exact) ===');
{
  // The user comes in, plays a short bit, stops mid-bar (commit quantizes later). Verify the short
  // take still starts at frame 0 with content (no leading silence from the count).
  const pending = 96960, take = 12000;
  const { t, state } = runCapture(pending, take, [128]);
  ok('B2 short take length', t.writeHead === take);
  ok('B2 short take frame 0 is content', approx(t.record[0], 0.7, FLT));
  ok('B2 pending consumed', state.pending === 0);
}

console.log('=== C. Count clicks FORCED audible (metronome off); accent on every bar 1 ===');
{
  // Metronome OFF: the 4 count beats (0..3) must fire; the come-in beat (4) and beyond must NOT.
  const off = false;
  for (let N = 0; N < COUNT_IN_BEATS; N++) {
    const d = clickDecision(N, COUNT_IN_BEATS, off);
    ok(`C count beat ${N} fires (forced, metronome off)`, d.fires === true);
    ok(`C count beat ${N} accent==${N === 0}`, d.accent === (N % 4 === 0));
  }
  const comeIn = clickDecision(COUNT_IN_BEATS, COUNT_IN_BEATS, off); // beat 4
  ok('C come-in beat silent when metronome off', comeIn.fires === false);
  ok('C come-in beat is an accent (loop downbeat)', comeIn.accent === true);
  ok('C beat 5 silent when metronome off', clickDecision(5, COUNT_IN_BEATS, off).fires === false);

  // Metronome ON: every beat fires (count forced OR metronome) and the come-in "1" clicks too.
  const on = true;
  for (let N = 0; N <= 8; N++) ok(`C metronome on: beat ${N} fires`, clickDecision(N, COUNT_IN_BEATS, on).fires === true);
  ok('C metronome on: come-in beat 4 accent', clickDecision(4, COUNT_IN_BEATS, on).accent === true);
}

console.log('=== D. Stop DURING the count aborts to EMPTY (no dead silent loop) ===');
{
  const t = makeTrack(96000); t.armed = true; t.state = 'RECORDING'; t.lengthFrames = 0;
  const state = { pending: 50000, activeRecordIndex: 1, countPulseTornDown: false };
  // user fed some pre-downbeat (discarded) frames, still armed, then stops
  consumeFirst(state, t, new Float32Array(4096).fill(0.5));
  ok('D still armed before come-in', t.armed === true && t.writeHead === 0);
  stopRecording(state, t, 0); // master===0 -> count-in abort
  ok('D aborts to EMPTY (no committed loop)', t.state === 'EMPTY' && t.lengthFrames === 0);
  ok('D pending reset', state.pending === 0);
  ok('D activeRecordIndex released', state.activeRecordIndex === -1);
  ok('D count pulse torn down (free-run grid restored)', state.countPulseTornDown === true);
  ok('D record buffer untouched / silent (no dead loop committed)', t.record.every((x) => x === 0));
}

console.log('=== E. Regression: later-track arm (master>0) still aborts/works after the refactor ===');
{
  // The hoisted armed-abort must also cover the later-track case WITHOUT tearing down the master
  // pulse (the master loop still exists).
  const MASTER = 96000;
  const t = makeTrack(MASTER); t.armed = true; t.state = 'RECORDING'; t.lengthFrames = 0;
  const state = { pending: 12000, activeRecordIndex: 2, countPulseTornDown: false };
  stopRecording(state, t, MASTER); // master>0 -> later-track abort
  ok('E later abort to EMPTY', t.state === 'EMPTY');
  ok('E later pending reset', state.pending === 0);
  ok('E later does NOT tear down master pulse', state.countPulseTornDown === false);
}

console.log('=== F. Anti-flam: count "1" never SWALLOWED (unconditional reset, 2026-07-04) ===');
{
  // Models triggerClick's anti-flam guard (clock.ts MIN_CLICK_SPACING) + startCountIn's UNCONDITIONAL
  // lastClickTime reset. HISTORY: 2026-06-20→2026-07-04 the reset was conditional (kept when the
  // metronome was ON) because the count then anchored to the next SOUNDING free-run '4n' beat and a
  // leftover un-cancellable blip could land at the exact anchor — the kept value de-duplicated it. Since
  // 2026-07-04 the idle free-run grid is SILENT (clock.setTransportActive gates the click) and the count
  // anchors at minLead, so no sounding blip can coincide with the anchor: the conditional only preserved
  // the dropped-"EN" hazard (a stale value from a fast abort→re-record suppressing the new count "1")
  // and the reset is now unconditional. startMasterPulse still does NOT reset (commit-flam bridge).
  const MIN_CLICK_SPACING = 0.12;
  const makeGate = (last = -1) => ({ last });
  function triggerClick(gate, time) {        // returns true if the blip SOUNDS
    if (gate.last >= 0 && Math.abs(time - gate.last) < MIN_CLICK_SPACING) return false; // suppressed
    gate.last = time;
    return true;
  }
  function startCountInReset(gate) { gate.last = -1; } // clock.ts startCountIn (unconditional 2026-07-04)
  const P = 0.5;

  // (1) HAZARD (no reset): a stale lastClickTime from a prior count/abort within 0.12 s of the new "EN"
  // WOULD suppress it. Proves the reset is load-bearing.
  {
    const g = makeGate(50.0);                 // prior count "EN" left at 50.0
    const anchor = 50.03;                     // re-record's count "1" 30 ms later
    ok('F hazard: without reset the new "EN" is suppressed', triggerClick(g, anchor) === false);
  }
  // (2) FIX: startCountInReset clears it for EVERY metronome state -> the new "EN" fires; beats 1..3 too.
  // (Both metronome states share one path now — a regression back to a conditional reset re-opens the
  // dropped-"EN" hazard on whichever branch keeps the stale value.)
  {
    const g = makeGate(50.0);
    startCountInReset(g); // -> g.last = -1, unconditionally
    const anchor = 50.03;
    ok('F fix: count "EN" fires after reset', triggerClick(g, anchor) === true);
    let all = true;
    for (let n = 1; n < COUNT_IN_BEATS; n++) if (!triggerClick(g, anchor + n * P)) all = false;
    ok('F fix: count beats 1..3 also fire (>=0.2s apart, no intra-count flam)', all);
  }
  // (3) commit-beat flam guard still works: startMasterPulse does NOT reset, so a leftover blip at 200.010
  // then the master pulse beat 0 at 200.020 (10 ms later) -> the second is SUPPRESSED (single strike).
  {
    const g = makeGate();
    ok('F commit-flam: stale blip fires', triggerClick(g, 200.010) === true);
    ok('F commit-flam: master beat0 10ms later suppressed (bridge preserved)', triggerClick(g, 200.020) === false);
  }
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails === 0 ? 0 : 1);
