// Overdub timer ownership and boundary progression. The model checks one pending timeout per
// active track and no callbacks after completion. The old state-only scheduler remains as a
// regression contrast; the preceding generation guard already prevented duplicate PCM swaps.
// Actual timeout registration/cancellation and dispatcher behavior: overdub-timers.mjs.
// MIRRORS: src/audio/looper/playback.ts@118-123 sha256:c5276b8403aa7026  (cancelOverdubSwap: cancel and release the owned handle)
// MIRRORS: src/audio/looper/machine.ts@696-698 sha256:9eff65c27c4a9766  (startOverdub: reset stop intent and schedule its timer)
// MIRRORS: src/audio/looper/machine.ts@702-717 sha256:d8467ffdf994b309  (finishOverdub: successful REC/DUB completion; no STOP or loss in this model)
// MIRRORS: src/audio/looper/playback.ts@141-182 sha256:0f9a28739937416f  (scheduleOverdubSwap: owned timeout and anchor-derived boundary swap)

let fails = 0, checks = 0;
function ok(name, cond, detail = '') {
  checks++;
  if (!cond) { fails++; console.log(`  FAIL  ${name}  ${detail}`); }
}

// Events run before the next boundary. Timer ids model the browser's owned/cancelled handles.
function simulate({ useCancellation, taps, boundaries }) {
  const track = { state: 'PLAYING', overdubBuf: false, overdubTimer: null, captureStopPlayback: false };
  let pending = [], nextId = 0;
  const cancel = () => {
    if (!useCancellation) return;
    pending = pending.filter((id) => id !== track.overdubTimer);
    track.overdubTimer = null;
  };
  const schedule = () => {
    cancel();
    track.overdubTimer = ++nextId;
    pending.push(track.overdubTimer);
  };
  const startOverdub = () => {
    if (track.state !== 'PLAYING') return;
    track.state = 'OVERDUBBING';
    track.overdubBuf = true;
    track.captureStopPlayback = false;
    schedule();
  };
  const endOverdub = () => {
    if (track.state !== 'OVERDUBBING') return;
    track.overdubBuf = false;
    track.state = track.captureStopPlayback ? 'STOPPED' : 'PLAYING';
    cancel(); // finishCapture releases the recorder and its timer.
  };
  for (const event of taps) {
    (event === 'start' ? startOverdub : endOverdub)();
    if (useCancellation) ok(`pending timer matches capture state after ${event}`,
      pending.length === (track.state === 'OVERDUBBING' ? 1 : 0));
  }
  const swapsPerBoundary = [];
  for (let b = 0; b < boundaries; b++) {
    const firing = pending;
    pending = [];
    let swaps = 0;
    for (const id of firing) {
      if (useCancellation && track.overdubTimer !== id) continue;
      track.overdubTimer = null;
      if (track.state !== 'OVERDUBBING' || !track.overdubBuf) continue;
      swaps++;
      if (track.state === 'OVERDUBBING') schedule();
    }
    swapsPerBoundary.push(swaps);
  }
  return swapsPerBoundary;
}

// ---- A. The exact live repro: PLAYING -> DUB -> DUB -> DUB (one end->start cycle in a period) ----
{
  const taps = ['start', 'end', 'start'];
  const fixed = simulate({ useCancellation: true, taps, boundaries: 8 });
  const buggy = simulate({ useCancellation: false, taps, boundaries: 8 });
  ok('A.fixed settles to exactly 1 swap/boundary', fixed.every((n) => n === 1), JSON.stringify(fixed));
  ok('A.buggy runs >1 swap/boundary (parallel chains)', buggy.every((n) => n >= 2), JSON.stringify(buggy));
  ok('A.buggy steady == 2 chains for 1 cycle', buggy[buggy.length - 1] === 2, JSON.stringify(buggy));
}

// ---- B. N within-period end->start cycles: fixed stays 1, buggy grows linearly with N ----
for (let cycles = 1; cycles <= 6; cycles++) {
  const taps = ['start'];
  for (let c = 0; c < cycles; c++) { taps.push('end', 'start'); } // start,(end,start)*cycles
  const fixed = simulate({ useCancellation: true, taps, boundaries: 10 });
  const buggy = simulate({ useCancellation: false, taps, boundaries: 10 });
  ok(`B.cycles=${cycles} fixed == 1/boundary (no leak)`, fixed.every((n) => n === 1), JSON.stringify(fixed));
  // buggy: every start-tap leaves a live chain -> (cycles+1) swaps/boundary, unbounded with cycles.
  ok(`B.cycles=${cycles} buggy == ${cycles + 1}/boundary (leak grows)`,
    buggy.every((n) => n === cycles + 1), JSON.stringify(buggy));
}

// ---- C. A plain single overdub (no cycle) is unaffected by the fix: exactly 1/boundary either way ----
{
  const fixed = simulate({ useCancellation: true, taps: ['start'], boundaries: 8 });
  const buggy = simulate({ useCancellation: false, taps: ['start'], boundaries: 8 });
  ok('C.single overdub fixed == 1/boundary', fixed.every((n) => n === 1), JSON.stringify(fixed));
  ok('C.single overdub buggy == 1/boundary (no regression risk)', buggy.every((n) => n === 1), JSON.stringify(buggy));
}

// ---- D. End without restart: no further PCM swaps in either scheduler ----
{
  // start -> end, then NO third start: at the boundary state is PLAYING, so the lone timer bails.
  const fixed = simulate({ useCancellation: true, taps: ['start', 'end'], boundaries: 5 });
  const buggy = simulate({ useCancellation: false, taps: ['start', 'end'], boundaries: 5 });
  ok('D.start+end fixed -> 0 swaps (state guard)', fixed.every((n) => n === 0), JSON.stringify(fixed));
  ok('D.start+end buggy -> 0 swaps (state guard already covers non-reentry)', buggy.every((n) => n === 0), JSON.stringify(buggy));
}

// ---- E. Same-boundary duplicate (early timer fire): the re-arm must target the NEXT boundary ----
// Pins the anchor-derived re-arm in scheduleOverdubSwap: the wall-clock setTimeout can fire while
// ctx.currentTime (render-quantum granularity) still reads a hair BEFORE the boundary `when` this
// firing commits at. The OLD re-arm recomputed nextBoundary() from ctx time — ceil returns the SAME
// boundary → a duplicate swap whose double-buffer pick writes into the AudioBuffer the outgoing source
// is still reading. The NEW re-arm takes the later of the anchor-derived scheduled successor and the
// first anchor-derived future boundary, so it strictly advances without replaying missed boundaries.
{
  const masterStart = 10.0;
  for (const period of [0.5, 2.0, 60 / 137]) {
    for (const k of [1, 5, 1000]) {
      const when = masterStart + k * period; // the boundary this firing commits at
      for (const skewMs of [-4, -0.1, 0, +4]) { // ctx clock vs the timer: early, hair-early, exact, late
        const ctxNow = when + skewMs / 1000;
        // OLD: nextBoundary() recompute at re-arm time (playback.ts nextBoundary: ceil from elapsed)
        const nOld = Math.ceil((ctxNow - masterStart) / period);
        const oldTarget = masterStart + nOld * period;
        // NEW: scheduled successor in the normal case, first future boundary after a real stall.
        const scheduledNext = Math.round((when - masterStart) / period) + 1;
        const firstFuture = Math.floor((ctxNow - masterStart) / period) + 1;
        const n = Math.max(scheduledNext, firstFuture);
        const newTarget = masterStart + n * period;
        ok(`E.new re-arm strictly advances one period (p=${period.toFixed(3)} k=${k} skew=${skewMs}ms)`,
           Math.abs(newTarget - (when + period)) < 1e-9, `got ${newTarget} want ${when + period}`);
        if (skewMs < 0) {
          ok(`E.old recompute duplicates the SAME boundary when ctx lags (p=${period.toFixed(3)} k=${k} skew=${skewMs}ms)`,
             Math.abs(oldTarget - when) < 1e-9, `old target ${oldTarget} vs when ${when}`);
        }
      }
      // No float accumulation: chaining the NEW re-arm 200 boundaries out stays exact vs the anchor.
      let w = when;
      for (let i = 0; i < 200; i++) {
        const scheduledNext = Math.round((w - masterStart) / period) + 1;
        const firstFuture = Math.floor((w - masterStart) / period) + 1;
        const ni = Math.max(scheduledNext, firstFuture);
        w = masterStart + ni * period;
      }
      ok(`E.200 chained re-arms stay anchor-exact (p=${period.toFixed(3)} k=${k})`,
         Math.abs(w - (masterStart + (k + 200) * period)) < 1e-9, `err=${Math.abs(w - (masterStart + (k + 200) * period))}`);
    }
  }

  for (const stalledPeriods of [1.2, 2.0, 4.7, 20.25]) {
    const period = 0.5;
    const when = masterStart + 3 * period;
    const ctxNow = when + stalledPeriods * period;
    const scheduledNext = Math.round((when - masterStart) / period) + 1;
    const firstFuture = Math.floor((ctxNow - masterStart) / period) + 1;
    const n = Math.max(scheduledNext, firstFuture);
    const next = masterStart + n * period;
    ok(`E.stall ${stalledPeriods}p jumps directly to first future boundary`,
      n === firstFuture && next > ctxNow && next - ctxNow <= period,
      `scheduledNext=${scheduledNext} firstFuture=${firstFuture} next=${next} now=${ctxNow}`);
  }
}

// The old Float32-ring mock here no longer represents the timestamped transport. Actual packet
// publication is executed by fs-capture-packets-verify.mjs; overdub-window.mjs drives real punch-out
// and proves its in-flight tail reaches the correct absolute frames, including delayed STOP.

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
if (fails) process.exit(1);
