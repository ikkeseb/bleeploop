// Executable verification of the COUNT-IN ANCHOR (revised 2026-07-04: minLead for BOTH metronome states).
// Faithful Node port of the startRecording anchor selection (src/audio/looper/machine.ts) + the
// consume() frame-exact countdown (src/audio/looper/capture.ts). The TS can't run in Node (Web Audio
// deps), so this mirrors the exact math by line. Same idiom as fs-count-in-verify.mjs.
//
// HISTORY: 2026-06-20 the count anchored to the next SOUNDING free-run '4n' beat when the metronome was
// on (clock.nextGridBeat — the fix for the by-ear finding that click and count-in ran on separate timers, a phase hop). On
// 2026-07-04 the idle free-run grid became SILENT (the click is a transport mode — count-in +
// recording/playback only; clock.setTransportActive), so there is no audible grid for the count to stay
// in phase with anymore: nextGridBeat was REMOVED (recover from git if ever needed) and the anchor is
// minLead (now + HBL) unconditionally — the snappiest possible count start. This file guards the NEW
// contract; the old grid-anchor sections died with the feature.
//
// What this PROVES (the deterministic core; the analog feel is owed a by-ear check on the PC):
//   - the anchor is minLead for BOTH metronome states (no metronome-conditional path remains)
//     and the count never schedules a beat in the past                                        [looper/machine.ts startRecording]
//   - the 4 count beats + the loop downbeat (recordStart) form ONE self-consistent grid:
//     beat n = anchor + n*P, recordStart = anchor + one exact bar                             [internal consistency]
//   - pendingRecordStartFrame = round((recordStart − now)·sr) is positive and the frame-exact
//     consume() countdown reaches the take's frame 0 at exactly that stream index             [looper/capture.ts consume]

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

// ── startRecording first-track anchor selection (looper/machine.ts, 2026-07-04 form) ──────────
// `metronomeOn` is accepted (and exercised) to prove the anchor is UNCONDITIONAL — the parameter must
// not influence the result. A regression that reintroduces a metronome-conditional anchor fails B.
// The REAL grid-math.ts countInArm (startRecording hands it ctx.currentTime + clock.bpm()) — no port.
function armCountIn(now, bpm, sr, _metronomeOn) {
  const a = countInArm(now, bpm, sr);
  return { beatPeriod: a.beatPeriod, minLead: now + HBL, anchor: a.anchor, recordStart: a.recordStart, pending: a.pendingFrames };
}

// MIRRORS: src/audio/looper/capture.ts@282-297 sha256:644d6403f64574d2  (armSplitOffset: retain the batch suffix after the absolute start)
// MIRRORS: src/audio/looper/capture.ts@331-341 sha256:c3a0b6e09d727c00  (consume: recording append, below the completion end in these fixtures)
// (the split itself is the REAL grid-math.ts armSplitAt; the write is the port)
// ── consume() first-track count-in arm — frame-exact countdown ─────
function consumeCountdown(pending, batchSizes, takeFrames) {
  // Stream = [pending discarded frames][takeFrames real]. Assert the take starts at frame 0 after
  // exactly `pending` frames are discarded, regardless of how batches straddle the boundary.
  const t = { armed: true, writeHead: 0 };
  let state = pending;
  const total = pending + takeFrames;
  let pos = 0, bi = 0;
  let firstTakeAbsFrame = -1;
  while (pos < total) {
    const bs = Math.min(batchSizes[bi++ % batchSizes.length], total - pos);
    const count = bs;
    let offset = 0;
    if (t.armed) {
      const split = armSplitAt(pending, pos, count);
      state = split.pending;
      if (split.offset < 0) { pos += bs; continue; }
      offset = split.offset; t.armed = false; t.writeHead = 0;
      firstTakeAbsFrame = pos + offset; // absolute stream index where the take's frame 0 came from
    }
    const n = count - offset;
    t.writeHead += n;
    pos += bs;
  }
  return { writeHead: t.writeHead, firstTakeAbsFrame, stateLeft: state };
}

// ════════════════════════════════════════════════════════════════════════════════════════════
console.log('=== A. anchor == minLead, never in the past; recordStart = anchor + one exact bar ===');
for (const bpm of [120, 90, 137, 100, 200, 73.5, 40, 300]) {
  for (const sr of [48000, 44100]) {
    for (const now of [0.0, 0.3, 0.49, 1.0, 12.7, 41.99, 100.05, 999.999]) {
      const a = armCountIn(now, bpm, sr, true);
      ok(`A anchor==minLead bpm=${bpm} now=${now}`, approx(a.anchor, now + HBL, 1e-12), `anchor=${a.anchor}`);
      ok(`A not in the past bpm=${bpm} now=${now}`, a.anchor >= now + HBL - 1e-9);
      ok(`A recordStart = anchor + 1 bar bpm=${bpm} sr=${sr}`,
         approx(a.recordStart - a.anchor, 4 * (60 / bpm), 1e-12));
      // the 4 count beats form one self-consistent grid off the anchor
      for (let n = 0; n < COUNT_IN_BEATS; n++) {
        const beat = a.anchor + n * a.beatPeriod;
        ok(`A count beat ${n} spacing bpm=${bpm} now=${now}`,
           approx(beat - a.anchor, n * (60 / bpm), 1e-12));
      }
    }
  }
}

console.log('=== B. the metronome state does NOT influence the anchor (unconditional path) ===');
for (const bpm of [120, 90, 137, 200]) {
  for (const sr of [48000, 44100]) {
    for (const now of [0.0, 50.001, 100.137, 999.999]) {
      const on = armCountIn(now, bpm, sr, true);
      const off = armCountIn(now, bpm, sr, false);
      ok(`B anchor identical on/off bpm=${bpm} now=${now}`, on.anchor === off.anchor,
         `on=${on.anchor} off=${off.anchor}`);
      ok(`B pending identical on/off bpm=${bpm} sr=${sr} now=${now}`, on.pending === off.pending);
      // pending matches the closed-form round((HBL + N*beatPeriod)*sr)
      const expect = Math.round((HBL + COUNT_IN_BEATS * (60 / bpm)) * sr);
      ok(`B pending closed form bpm=${bpm} sr=${sr} now=${now}`, on.pending === expect,
         `${on.pending} vs ${expect}`);
    }
  }
}

console.log('=== C. pending positive + frame-exact countdown reaches frame 0 at index == pending ===');
for (const bpm of [120, 90, 200]) {
  for (const sr of [48000, 44100]) {
    const now = 50.001;
    const a = armCountIn(now, bpm, sr, true);
    ok(`C pending > 0 bpm=${bpm} sr=${sr}`, a.pending > 0, `pending=${a.pending}`);
    // the consume() countdown discards exactly `pending` frames then the take starts at frame 0,
    // across ragged batch regimes (jitter-free) — the take's frame 0 came from stream index == pending
    for (const sizes of [[128], [a.pending, 4096], [1024, 333, 5000, 128, 20000], [a.pending + 96000]]) {
      const r = consumeCountdown(a.pending, sizes, 96000);
      ok(`C take==96000 bpm=${bpm} [${sizes.length}b]`, r.writeHead === 96000, `wh=${r.writeHead}`);
      ok(`C frame0 at index==pending bpm=${bpm} [${sizes.length}b]`, r.firstTakeAbsFrame === a.pending,
         `got ${r.firstTakeAbsFrame} want ${a.pending}`);
      ok(`C pending fully consumed bpm=${bpm} [${sizes.length}b]`, r.stateLeft === 0);
    }
  }
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails === 0 ? 0 : 1);
