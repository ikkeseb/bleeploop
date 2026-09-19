// Executable verification of the count-in BPM-lock lifecycle (bug-hunt 2026-06-23, R4) — a faithful
// model of src/audio/looper/machine.ts's lock/unlock state flow + the clock.setBpm lock gate (looper.ts
// was split into src/audio/looper/{state,capture,peaks,playback,machine,mixer}.ts + a facade on
// 2026-07-01). Same idiom as the other fs-*-verify.mjs (the TS
// can't be imported in Node — Tone/Web Audio deps — so this mirrors the exact transitions by line).
//
// THE BUG (R4): for a FREE-record-with-count-in (the default first-track path), BPM was NOT locked at the
// press — the lock lived only inside `if (fixedLengthEnabled())`. The Transport BPM controls gate solely on
// clock.bpmLocked(), so the user could retune tempo mid-take. clock.startCountIn captured the count pulse's
// period from bpm_press and never re-anchors on setBpm, while finishRecording recomputes the loop/master
// period from clock.bpm() = bpm_commit — so the count the player heard and the committed loop disagree (a
// tempo seam at the count→loop boundary; the one-grid promise breaks).
//
// THE FIX: lock BPM for EVERY first-track count-in (free or fixed) at the press; any abort unlocks it
// (releaseRecorderState, now keyed on master===0). A committed loop keeps the lock (master>0).
//
// What this PROVES: (1) press locks BPM (free + fixed); (2) a mid-take retune is a no-op while locked, so
// count grid == commit grid; (3) every abort path (stopCapture/stop/clear) unlocks a first-track
// count-in; (4) a committed loop stays locked; (5) a LATER-track arm abort does NOT unlock (master>0).

let fails = 0, checks = 0;
const approx = (a, b, eps = 1e-9) => Math.abs(a - b) <= eps;
function ok(name, cond, detail = '') { checks++; if (!cond) { fails++; console.log(`  FAIL  ${name}  ${detail}`); } }

// ── Minimal clock model (src/audio/clock.ts) ───────────────────────────────────────────────
function makeClock(bpm = 120) {
  return {
    bpm,
    locked: false,
    // MIRRORS: src/audio/clock.ts@41-53 sha256:8a06763d68be9472  (setBpm — lock guard + clamp + identical-value no-op)
    // (The source additionally re-anchors a live FREE-RUN pulse on a REAL change — count/master
    // pulses are untouched. That side effect is out of scope for this lock-lifecycle model and is
    // guarded in fs-accent-grid-verify.mjs section E.)
    setBpm(n) {
      if (this.locked) return;
      const clamped = Math.max(40, Math.min(300, Math.round(n)));
      if (clamped === this.bpm) return; // value-identical set = full no-op
      this.bpm = clamped;
    },
    // MIRRORS: src/audio/clock.ts@106-108 sha256:a57cdfc98fae3f23  (setBpmLocked)
    setBpmLocked(on) { this.locked = on; },
  };
}

// ── Minimal looper model: the lock/unlock transitions that matter (src/audio/looper/machine.ts) ──
function makeLooper(clock) {
  return {
    clock,
    master: 0, // masterLengthFrames()
    captureStartFrame: null,
    captureEndFrame: null,
    activeRecordIndex: -1,
    countPressBpm: 0, // = startCountIn's captured bpm (the count grid tempo)

    // startRecording (NEW: lock for EVERY first-track count-in)
    startRecording(i, { fixed = false, fixedBars = 2 } = {}) {
      if (this.activeRecordIndex >= 0) return;
      this.activeRecordIndex = i;
      if (this.master === 0) {
        // FIRST track: count-in. Capture the count grid tempo, then lock (R4: was fixed-only).
        this.countPressBpm = this.clock.bpm; // clock.startCountIn(anchor, 60/bpm, ...)
        this.clock.setBpmLocked(true); // R4 — moved OUT of the fixedLength block

      }
      this.captureStartFrame = 10000; // fixed fixture origin; timing arithmetic is tested separately
      this.captureEndFrame = this.captureStartFrame + (this.master || (fixed ? fixedBars * 1000 : 60000));
      // LATER track (master>0): arm only, NO lock (BPM must stay frozen to the committed loop).
    },

    // MIRRORS: src/audio/looper/machine.ts@377-398 sha256:3be20034b52c094c  (releaseRecorderState: owner guard, window reset, BPM unlock)
    releaseRecorderState(i) {
      if (this.activeRecordIndex !== i) return;
      this.activeRecordIndex = -1;
      this.captureStartFrame = null;
      this.captureEndFrame = null;
      if (this.master === 0) this.clock.setBpmLocked(false);
    },

    // abort paths all converge on releaseRecorderState (stopCapture arm branch / stop / clear)
    abortDuringArm() {
      this.releaseRecorderState(this.activeRecordIndex);
    },

    // finishRecording establishes the master before release, so the BPM lock survives.
    commit(bars = 2) {
      const master = bars * 1000;
      // derivedBpm consistent with the integer frame count == the (locked) press tempo
      this.master = master;
      this.clock.setBpmLocked(true);
      this.releaseRecorderState(this.activeRecordIndex);
      // The commit grid period basis = clock.bpm (frozen at press since the lock held).
      return this.clock.bpm;
    },
  };
}

// ════════════════════════════════════════════════════════════════════════════════════════════
console.log('=== A. Free-record count-in: press LOCKS BPM (the R4 fix) ===');
{
  const c = makeClock(120); const lp = makeLooper(c);
  lp.startRecording(0, { fixed: false });
  ok('A free-record count-in press locks BPM', c.locked === true);
  ok('A count grid tempo captured at press', lp.countPressBpm === 120);
}

console.log('=== B. Mid-take retune is a NO-OP while locked → count grid == commit grid ===');
{
  const c = makeClock(120); const lp = makeLooper(c);
  lp.startRecording(0, { fixed: false });   // locks @120, count grid = 120
  c.setBpm(100);                            // user drags tempo mid-take — gated by locked → no-op
  ok('B setBpm is a no-op while locked', c.bpm === 120);
  const commitBpm = lp.commit(2);
  ok('B commit grid tempo == count grid tempo (one grid)', approx(60 / commitBpm, 60 / lp.countPressBpm),
     `count=${60 / lp.countPressBpm} commit=${60 / commitBpm}`);

  // Contrast: the OLD code (no lock on free-record) — setBpm would succeed and the grids diverge.
  const c2 = makeClock(120);
  const countGridOld = 60 / c2.bpm; // 0.5
  c2.setBpm(100); // OLD free-record: unlocked → succeeds
  const commitGridOld = 60 / c2.bpm; // 0.6
  ok('B OLD (unlocked) grids DIVERGE — the bug', !approx(countGridOld, commitGridOld),
     `count=${countGridOld} commit=${commitGridOld}`);
}

console.log('=== C. Every abort path UNLOCKS a first-track count-in (free + fixed) ===');
{
  for (const fixed of [false, true]) {
    const c = makeClock(120); const lp = makeLooper(c);
    lp.startRecording(0, { fixed });
    ok(`C [${fixed ? 'fixed' : 'free'}] locked after press`, c.locked === true);
    lp.abortDuringArm(); // stopCapture arm branch / stop / clear all reach releaseRecorderState
    ok(`C [${fixed ? 'fixed' : 'free'}] abort UNLOCKS BPM`, c.locked === false);
    ok(`C [${fixed ? 'fixed' : 'free'}] abort clears the fixed-length arm`, lp.captureStartFrame === null && lp.captureEndFrame === null);
  }
}

console.log('=== D. A committed loop STAYS locked (commit does not unlock) ===');
{
  const c = makeClock(120); const lp = makeLooper(c);
  lp.startRecording(0, { fixed: false });
  lp.commit(2);
  ok('D committed loop keeps BPM locked', c.locked === true);
  ok('D master defined', lp.master > 0);
  c.setBpm(140);
  ok('D BPM still frozen after commit', c.bpm === 120);
}

console.log('=== E. A LATER-track arm abort must NOT unlock (master>0, the loop owns the tempo) ===');
{
  const c = makeClock(120); const lp = makeLooper(c);
  lp.startRecording(0, { fixed: false });
  lp.commit(2);                 // master>0, locked
  ok('E pre: master>0 and locked', lp.master > 0 && c.locked === true);
  lp.startRecording(1);         // LATER track arm — no lock change
  lp.abortDuringArm();          // abort the later-track arm → releaseRecorderState with master>0
  ok('E later-track arm abort leaves BPM LOCKED (master>0)', c.locked === true);
}

console.log('=== F. Only the recording lane can release its capture window or BPM lock ===');
{
  const c = makeClock(120), lp = makeLooper(c);
  lp.startRecording(2, { fixed: true });
  const start = lp.captureStartFrame, end = lp.captureEndFrame;
  lp.releaseRecorderState(1);
  ok('F another lane cannot release the recorder', lp.activeRecordIndex === 2);
  ok('F another lane preserves both window edges', lp.captureStartFrame === start && lp.captureEndFrame === end);
  ok('F another lane cannot unlock BPM', c.locked === true);
  lp.abortDuringArm();
  ok('F owner releases both edges and unlocks', lp.activeRecordIndex === -1 && lp.captureStartFrame === null && lp.captureEndFrame === null && !c.locked);
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails === 0 ? 0 : 1);
