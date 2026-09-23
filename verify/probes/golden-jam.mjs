/**
 * verify/probes/golden-jam.mjs — the GOLDEN JAM: an end-to-end grid proof against the running app.
 *
 * Everything else in verify/ asserts PURE LOGIC (ported or imported) with no browser and no
 * AudioContext. That leaves the app's actual dispatchers — recDub/playStop/stop/clear and the real
 * capture drain — with no automated coverage at all: three real bugs injected into machine.ts
 * (a 500 ms grid error in resume(), a dropped deferred-commit flag in playStop(), a removed FX reset
 * in clear()) left all 9560 checks green. This script closes the sharpest part
 * of that hole by DRIVING the real app in a real browser and measuring the recorded PCM.
 *
 * Method: schedule 1-sample impulses at an exact beat interval into engine.looperInputBus — each beat
 * carrying its OWN amplitude, so the train repeats once per master loop rather than once per beat —
 * record a fixed-length take on track 1, then a later take on track 2, and read both back through the
 * real `looper.exportSnapshot()`. Assertions are frame counts and exact amplitude ratios, so a grid
 * error of ONE sample fails, and so does an overdub shifted by a whole beat.
 *
 * What this proves that a by-ear session cannot:
 *   1. the committed master is exactly bars x beats x framesPerBeat  (bar math + count-in split)
 *   2. every inter-impulse gap is exactly framesPerBeat, zero jitter  (continuous, sample-exact capture)
 *   3. track 2's impulses land at byte-identical offsets to track 1's (the later-track boundary arm is
 *      frame-exact — the invariant the whole "tracks cannot drift by construction" claim rests on)
 *   4. an overdub sums onto the existing impulses at the same ABSOLUTE loop position, not merely on
 *      some beat: the per-beat amplitude fingerprint fails a writeHead off by any whole number of beats
 *      (startOverdub's writeHead phase)
 *   5. captureOverruns() === 0 across the whole session          (no silent ring loss)
 *   6. the dispatchers run end-to-end: overdub/undo/redo, playStop -> STOPPED -> resume, clear back to a
 *      blank lane (vol/mute/all five FX entries in lockstep — the 2026-07-11 product call), both armed
 *      abort paths reset a grid whose committed lanes were cleared, and the DEFERRED first-track commit,
 *      where a stop press during the commit window must keep the take
 *   7. injected plugin-bridge loss rejects first/later takes and overdubs; an overdub restores both PCM
 *      and prior undo history, and a wet export cannot start while capture is active
 *   8. AUTO REC stays silent/blank while listening, cancels cleanly, then triggers from real graph PCM
 *      with its soft onset retained near frame 0
 *   9. SHORT TAKES: a later take shorter than the master (stopped inside its first bar, or pre-selected by
 *      FIXED) commits at master length and repeats sample-exactly across it, RETAKE keeps rolling at the
 *      master whatever FIXED says, and PLAY on an idle transport re-anchors the master grid to frame 0
 *      while PLAY beside a playing lane joins the running phase (`looper.phaseValue()`)
 *
 * KNOWN LIMITS — do not read a green run as more than it is:
 *   - Everything measured comes from RECORDED PCM plus dispatcher state. Loop PLAYBACK is not captured
 *     (it routes past the record tap by design), so a phase error in resume()/startPlayback is INVISIBLE
 *     here. `playback-restart.mjs` separately measures restart/join PCM; perceived sound remains a by-ear gate.
 *   - A uniform slip applied to every take alike (e.g. one frame added to every arm split) keeps all the
 *     relative assertions green. The harness proves takes agree with each other, not that they agree with
 *     an absolute reference the browser does not expose.
 *   - The amplitude fingerprint's period IS the master loop, so an overdub writeHead shifted by a whole
 *     number of LOOPS still maps the loop onto itself and passes. Only sub-loop shifts are caught.
 *   - Track 2's frame-identity (3) is asserted on impulse POSITIONS only. The same beat-periodicity blind
 *     spot therefore still covers the later-track arm: an arm off by a whole beat lands on the same
 *     positions and passes. The fingerprint amplitudes are recorded there but not compared.
 *   - The FROM-THE-TOP checks read `looper.phaseValue()`, which is the master GRID (masterStartTime) as the
 *     25 ms drain tick sees it — not the position of any playing AudioBufferSource. A re-anchored grid whose
 *     sources started at the wrong offset would still read phase 0 here. `playback-restart.mjs` covers that
 *     gap with seeded ramps captured at the actual lane gains, including the first audible sample.
 *   - Nothing native: the whole src-tauri half, the real record latency C on a rig, and the mic path are
 *     out of reach. Those stay by-ear/rig gates in STATUS.md.
 *
 * Deliberately OUTSIDE `pnpm verify` / `pnpm check` — it needs a browser and ~60 s, and those gates
 * are a ~1 s pre-commit reflex. Run it with `pnpm verify:jam` before/after touching looper timing.
 *
 * Run:  pnpm verify:jam            (starts its own vite dev server, or reuses one already on 1420)
 *       pnpm verify:jam --headed   (watch it play)
 */
import { chromium } from 'playwright';
import { spawn, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const REPO = join(dirname(fileURLToPath(import.meta.url)), '../..');
const URL = process.argv.find((arg) => arg.startsWith('--url='))?.slice(6) ?? 'http://localhost:1420';
const HEADED = process.argv.includes('--headed');

/** Musical shape of the jam. BPM 120 + 2 bars keeps every expected frame count an exact integer at
 *  both 44.1 k and 48 k, so the harness is sample-rate agnostic without special-casing. */
const BPM = 120;
const BARS = 2;
const BEATS_PER_BAR = 4;
/** Nominal impulse amplitude, and the threshold that finds one again in the recorded PCM. The record
 *  tap is a plain gain mirror of looperInputBus (unity, pre-limiter), so what goes in comes back out. */
const IMPULSE = 1.0;
const FOUND = 0.5;
const BEATS = BARS * BEATS_PER_BAR;
/**
 * Per-beat amplitude fingerprint for the injected impulse train: BEATS distinct amplitudes, so the
 * train repeats with a period of exactly ONE MASTER LOOP instead of one beat. A beat-periodic train
 * maps onto itself under ANY whole-beat writeHead shift — same hit count, same doubled peak — which is
 * exactly the overdub slip a count+peak proof cannot see. The band is deliberately narrow (max/min =
 * 1.278, inside 3/4..4/3, every value distinct): under a shift a hit reads base + m*other, and no such
 * sum is an integer multiple of base for m = 1..3, with a worst-case margin of 0.031 against the 0.01
 * tolerance the overdub check uses. Every value stays above FOUND and below IMPULSE*1.5 (the undo
 * check's pre-dub ceiling), and every doubled value above it.
 */
const FINGERPRINT = Array.from({ length: BEATS }, (_, b) => IMPULSE * (0.9 + (0.25 * b) / (BEATS - 1)));

let passed = 0;
let failed = 0;
function check(name, ok, detail = '') {
  if (ok) {
    passed++;
    console.log(`  ok   ${name}${detail ? `  ${detail}` : ''}`);
  } else {
    failed++;
    console.log(`  FAIL ${name}${detail ? `  ${detail}` : ''}`);
  }
}

/** Poll `fn` until it returns truthy or `ms` elapses. Returns the value, or null on timeout. */
async function waitFor(page, fn, ms, label) {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) {
    const v = await page.evaluate(fn);
    if (v) return v;
    await page.waitForTimeout(100);
  }
  throw new Error(`timed out after ${ms} ms waiting for ${label}`);
}

async function serverUp() {
  try {
    const res = await fetch(URL, { signal: AbortSignal.timeout(1500) });
    return res.ok;
  } catch {
    return false;
  }
}

async function main() {
  // ---- dev server: reuse one already running, else start (and own) one -------------------------
  let vite = null;
  if (await serverUp()) {
    console.log(`golden-jam: reusing the dev server already on ${URL}`);
  } else {
    if (URL !== 'http://localhost:1420') throw new Error(`Start the verification server at ${URL} first`);
    console.log('golden-jam: starting a dev server…');
    // shell:true — on Windows pnpm is pnpm.cmd, which a bare spawn() can't exec (ENOENT).
    vite = spawn('pnpm', ['dev'], { cwd: REPO, stdio: 'ignore', detached: false, shell: true });
    const deadline = Date.now() + 30000;
    while (Date.now() < deadline && !(await serverUp())) await new Promise((r) => setTimeout(r, 300));
    if (!(await serverUp())) {
      if (process.platform === 'win32') {
        spawnSync('taskkill', ['/pid', String(vite.pid), '/T', '/F'], { stdio: 'ignore' });
      } else {
        vite.kill();
      }
      throw new Error('dev server did not come up on 1420 within 30 s');
    }
  }

  const browser = await chromium.launch({
    headless: !HEADED,
    // Headless Chromium will not start an AudioContext without a gesture unless told otherwise; the
    // page click below is kept too, so this stays a belt-and-braces flag rather than the only path.
    args: ['--autoplay-policy=no-user-gesture-required'],
  });

  try {
    const page = await browser.newPage();
    // LF_JAM_CPU_THROTTLE=<n> slows the renderer n× (CDP Emulation) — the way to reproduce the
    // loaded-runner failures (off-grid overdub, 128-frame later-track slip) on a fast dev machine.
    const throttle = Number(process.env.LF_JAM_CPU_THROTTLE ?? 0);
    if (throttle > 1) {
      const cdp = await page.context().newCDPSession(page);
      await cdp.send('Emulation.setCPUThrottlingRate', { rate: throttle });
      console.log(`golden-jam: CPU throttled ${throttle}×`);
    }
    const pageErrors = [];
    page.on('pageerror', (e) => pageErrors.push(String(e)));
    page.on('console', (m) => {
      if (m.type() === 'error') pageErrors.push(m.text());
    });

    await page.goto(URL, { waitUntil: 'domcontentloaded' });
    await waitFor(page, () => typeof window.__lf !== 'undefined', 20000, 'the __lf debug hook');

    // The capture ring is a SharedArrayBuffer — without COOP/COEP looper.init() bails and every
    // button is silently dead, which would read here as a mysterious timeout instead of a cause.
    const isolated = await page.evaluate(() => self.crossOriginIsolated === true);
    check('crossOriginIsolated (SharedArrayBuffer available)', isolated);
    if (!isolated) throw new Error('not cross-origin isolated — the looper cannot run');

    // A real user gesture unlocks the AudioContext (the header title is a safe click target).
    await page.click('body', { position: { x: 5, y: 5 } });
    await page.evaluate(() => window.__lf.ensureActive());
    const running = await waitFor(
      page,
      () => window.__lf.engine.ctx.state === 'running' && window.__lf.engine.ctx.currentTime > 0.05,
      10000,
      'a running AudioContext',
    );
    check('AudioContext is running', !!running);

    const sr = await page.evaluate(() => window.__lf.engine.ctx.sampleRate);
    // Derived independently of src/audio/quantize.ts on purpose: if the app's own framesPerBar ever
    // stops meaning "one bar", this harness must disagree with it rather than agree by construction.
    const framesPerBeat = Math.round((60 / BPM) * sr);
    const expectedMaster = framesPerBeat * BEATS_PER_BAR * BARS;
    console.log(`\ngolden-jam: ${sr} Hz · ${BPM} bpm · ${BARS} bars · beat = ${framesPerBeat} frames`);
    console.log(`            expected master = ${expectedMaster} frames\n`);

    // ---- AUTO REC: first-track level arm + retained onset ---------------------------------------
    console.log('golden-jam: AUTO REC\n');
    const autoListening = await page.evaluate(async () => {
      const lf = window.__lf;
      lf.clock.setBpmLocked(false);
      lf.clock.setBpm(120);
      lf.clock.setMetronome(false);
      lf.looper.setFixedLengthEnabled(false);
      lf.looper.setAutoRecordSensitivity(50);
      lf.looper.setAutoRecordEnabled(true);
      await lf.looper.recDub(0);
      await new Promise((resolve) => setTimeout(resolve, 180));
      const lane = document.querySelector('.lp-lane');
      const info = lf.looper.trackInfo(0);
      return {
        state: info.state,
        armed: info.armed,
        autoArmed: info.autoArmed,
        fill: lf.looper.fillFramesOf(0),
        bpmLocked: lf.clock.bpmLocked(),
        laneState: lane?.getAttribute('data-state'),
        well: lane?.querySelector('.lp-lane__wellmsg')?.textContent?.trim(),
        slider: document.querySelector('[aria-label="Auto record sensitivity"]') !== null,
      };
    });
    check(
      'AUTO arm stayed blank and unlocked through silence',
      autoListening.state === 'RECORDING' &&
        autoListening.armed === false &&
        autoListening.autoArmed === true &&
        autoListening.fill === 0 &&
        autoListening.bpmLocked === false,
      `state=${autoListening.state}, auto=${autoListening.autoArmed}, fill=${autoListening.fill}, locked=${autoListening.bpmLocked}`,
    );
    check(
      'AUTO arm rendered as LISTENING with its sensitivity control',
      autoListening.laneState === 'listening' && autoListening.well === 'WAITING FOR INPUT' && autoListening.slider,
      `lane=${autoListening.laneState}, well=${autoListening.well}, slider=${autoListening.slider}`,
    );

    const autoCancelled = await page.evaluate(async () => {
      const lf = window.__lf;
      await lf.looper.recDub(0);
      const info = lf.looper.trackInfo(0);
      return {
        state: info.state,
        autoArmed: info.autoArmed,
        master: lf.looper.masterLengthFrames(),
        bpmLocked: lf.clock.bpmLocked(),
      };
    });
    check(
      'AUTO arm cancelled back to the true blank slate',
      autoCancelled.state === 'EMPTY' &&
        autoCancelled.autoArmed === false &&
        autoCancelled.master === 0 &&
        autoCancelled.bpmLocked === false,
      `state=${autoCancelled.state}, master=${autoCancelled.master}, locked=${autoCancelled.bpmLocked}`,
    );

    await page.evaluate(async () => {
      const lf = window.__lf;
      await lf.looper.recDub(0);
      const ctx = lf.engine.ctx;
      const buf = ctx.createBuffer(1, Math.round(ctx.sampleRate * 0.08), ctx.sampleRate);
      const pcm = buf.getChannelData(0);
      const softAt = Math.round(ctx.sampleRate * 0.02);
      const loudAt = softAt + Math.round(ctx.sampleRate * 0.008);
      pcm.fill(0.01, softAt, loudAt);
      pcm.fill(0.1, loudAt, loudAt + Math.round(ctx.sampleRate * 0.02));
      const src = ctx.createBufferSource();
      src.buffer = buf;
      src.connect(lf.engine.looperInputBus);
      src.start(ctx.currentTime + 0.05);
      window.__autoSrc = src;
    });
    await waitFor(
      page,
      () => {
        const info = window.__lf.looper.trackInfo(0);
        return info.state === 'RECORDING' && !info.autoArmed && window.__lf.looper.fillFramesOf(0) > 0;
      },
      5000,
      'AUTO REC to trigger from graph audio',
    );
    await page.waitForTimeout(180);
    await page.evaluate(() => window.__lf.looper.recDub(0));
    await waitFor(page, () => window.__lf.looper.stateOf(0) !== 'RECORDING', 5000, 'AUTO capture tail to finish');
    const autoTake = await page.evaluate(() => {
      const lf = window.__lf;
      const track = lf.looper.exportSnapshot().tracks.find((item) => item.index === 0);
      let firstNonZero = -1;
      for (let i = 0; i < track.pcm.length; i++) {
        if (Math.abs(track.pcm[i]) > 0.001) {
          firstNonZero = i;
          break;
        }
      }
      return {
        state: lf.looper.trackInfo(0).state,
        length: track.pcm.length,
        firstNonZero,
        peak: lf.looper.trackPeak(0),
      };
    });
    check(
      'AUTO REC retained the soft onset near frame 0',
      autoTake.state === 'PLAYING' &&
        autoTake.length === framesPerBeat * BEATS_PER_BAR &&
        autoTake.firstNonZero >= 0 &&
        autoTake.firstNonZero <= Math.round(sr * 0.012) &&
        autoTake.peak > 0.05,
      `state=${autoTake.state}, length=${autoTake.length}, first=${autoTake.firstNonZero}, peak=${autoTake.peak}`,
    );

    // FIXED + AUTO freezes its bar target only when input actually starts the take, then follows the
    // ordinary frame-exact auto-stop path.
    await page.evaluate(() => {
      window.__lf.looper.clearAll();
      window.__lf.looper.setFixedLengthBars(1);
      window.__lf.looper.setFixedLengthEnabled(true);
    });
    await page.evaluate(async () => {
      const lf = window.__lf;
      await lf.looper.recDub(0);
      const ctx = lf.engine.ctx;
      const buf = ctx.createBuffer(1, Math.round(ctx.sampleRate * 0.04), ctx.sampleRate);
      buf.getChannelData(0).fill(0.1, Math.round(ctx.sampleRate * 0.01));
      const src = ctx.createBufferSource();
      src.buffer = buf;
      src.connect(lf.engine.looperInputBus);
      src.start(ctx.currentTime + 0.05);
      window.__autoFixedSrc = src;
    });
    await waitFor(
      page,
      () => window.__lf.looper.trackInfo(0).state === 'PLAYING',
      5000,
      'FIXED + AUTO to stop after one bar',
    );
    const autoFixed = await page.evaluate(() => ({
      length: window.__lf.looper.masterLengthFrames(),
      state: window.__lf.looper.trackInfo(0).state,
      autoArmed: window.__lf.looper.trackInfo(0).autoArmed,
    }));
    check(
      'FIXED + AUTO committed exactly one bar after the trigger',
      autoFixed.state === 'PLAYING' && autoFixed.length === framesPerBeat * BEATS_PER_BAR && !autoFixed.autoArmed,
      `state=${autoFixed.state}, length=${autoFixed.length}, auto=${autoFixed.autoArmed}`,
    );
    await page.evaluate(() => {
      window.__lf.looper.clearAll();
      window.__lf.looper.setFixedLengthEnabled(false);
      window.__lf.looper.setAutoRecordEnabled(false);
    });

    // ---- RETAKE: a rolling take keeps the last complete pass ---------------------------------------
    // The source is a slow linear ramp, so every recorded sample names the render time it was captured
    // at: a committed loop must be ONE unbroken pass (monotonic), and its first sample says WHICH pass.
    console.log('golden-jam: RETAKE\n');
    const RAMP_SECONDS = 60, RAMP_TOP = 0.9;
    const retakeBar = Math.round((60 / 120) * sr) * BEATS_PER_BAR; // the AUTO section left the clock at 120
    const barSec = retakeBar / sr;
    await page.evaluate(
      ({ seconds, top }) => {
        const lf = window.__lf;
        const ctx = lf.engine.ctx;
        lf.clock.setBpmLocked(false);
        lf.clock.setBpm(120);
        lf.clock.setMetronome(false);
        lf.looper.setFixedLengthBars(1);
        lf.looper.setFixedLengthEnabled(true);
        lf.looper.setRetakeEnabled(true);
        const ramp = ctx.createConstantSource();
        const t0 = ctx.currentTime + 0.05;
        ramp.offset.setValueAtTime(0, t0);
        ramp.offset.linearRampToValueAtTime(top, t0 + seconds);
        ramp.connect(lf.engine.looperInputBus);
        ramp.start(t0);
        window.__retake = { ramp, t0 };
      },
      { seconds: RAMP_SECONDS, top: RAMP_TOP },
    );
    // Read a committed lane back as capture times: [first, last] sample → seconds on the ctx clock.
    const laneTimes = (idx) =>
      page.evaluate(
        ({ idx, seconds, top }) => {
          const pcm = window.__lf.looper.exportSnapshot().tracks.find((x) => x.index === idx).pcm;
          // The ramp's own float32 rounding wobbles by ~10 µs; a seam between two passes jumps back a whole
          // pass (seconds). 1 ms of ramp separates the two by three orders of magnitude either way.
          const wobble = (top / seconds) * 0.001;
          let monotonic = true;
          for (let k = 1; k < pcm.length; k++) if (pcm[k] < pcm[k - 1] - wobble) monotonic = false;
          const at = (v) => window.__retake.t0 + (v / top) * seconds;
          return { length: pcm.length, monotonic, first: at(pcm[0]), last: at(pcm[pcm.length - 1]) };
        },
        { idx, seconds: RAMP_SECONDS, top: RAMP_TOP },
      );
    const FRAME_TOL = 6 / sr; // float32 ramp resolution is a fraction of a frame; allow a few

    await page.evaluate(() => window.__lf.looper.recDub(0));
    await waitFor(page, () => window.__lf.looper.trackInfo(0).retakePass === 3, 15000, 'the first take to roll into pass 3');
    const rolling = await page.evaluate(() => ({
      word: document.querySelector('.lp-lane .lp-lane__state')?.textContent?.trim(),
      master: window.__lf.looper.masterLengthFrames(),
      otherRec: document.querySelectorAll('.lp-lane')[1]?.querySelector('.lp-core')?.disabled === false,
    }));
    check('a rolling first take shows its pass and commits nothing', rolling.word === 'TAKE 3' && rolling.master === 0,
      `word=${rolling.word}, master=${rolling.master}`);
    check('an EMPTY lane offers REC as the approve gesture while a take rolls', rolling.otherRec);
    await page.waitForTimeout(600); // well inside pass 3, far outside the quarter-beat grace
    const pressA = await page.evaluate(async () => {
      const at = window.__lf.engine.ctx.currentTime;
      await window.__lf.looper.recDub(0);
      return { at, state: window.__lf.looper.trackInfo(0).state };
    });
    const takeA = await laneTimes(0);
    check('approve mid-pass commits at once', pressA.state === 'PLAYING', `state=${pressA.state}`);
    check('the kept pass is one unbroken bar', takeA.length === retakeBar && takeA.monotonic,
      `length=${takeA.length} (want ${retakeBar}), monotonic=${takeA.monotonic}`);
    check(
      'the kept pass is the LAST COMPLETE one, not the pass in flight',
      takeA.first > pressA.at - 2 * barSec && takeA.first < pressA.at - barSec,
      `first sample captured ${(pressA.at - takeA.first).toFixed(3)} s before the press (want ${barSec}..${2 * barSec})`,
    );

    // Later take + REC on another lane: lane 2 rolls, REC on lane 3 approves it and lane 3 records next.
    await page.evaluate(() => window.__lf.looper.recDub(1));
    await waitFor(page, () => window.__lf.looper.trackInfo(1).retakePass === 2, 15000, 'the later take to roll into pass 2');
    await page.waitForTimeout(500);
    const pressB = await page.evaluate(async () => {
      const lf = window.__lf;
      const at = lf.engine.ctx.currentTime;
      await lf.looper.recDub(2);
      return { at, lane2: lf.looper.trackInfo(1).state, lane3: lf.looper.trackInfo(2).state };
    });
    const takeB = await laneTimes(1);
    check('REC on another lane approves the rolling take and takes over the recorder',
      pressB.lane2 === 'PLAYING' && pressB.lane3 === 'RECORDING', `lane2=${pressB.lane2}, lane3=${pressB.lane3}`);
    check('the handed-off lane kept its last complete pass',
      takeB.monotonic && takeB.first > pressB.at - 2 * barSec && takeB.first < pressB.at - barSec,
      `monotonic=${takeB.monotonic}, ${(pressB.at - takeB.first).toFixed(3)} s before the press`);
    const gridSlip = (a, b) => { const n = (a - b) / barSec; return Math.abs(n - Math.round(n)) * barSec; };
    check('every pass sits on the first take\'s grid', gridSlip(takeB.first, takeA.first) < FRAME_TOL,
      `slip=${(gridSlip(takeB.first, takeA.first) * sr).toFixed(2)} frames`);

    // Early-press grace: a press a hair BEFORE the pass edge means "this pass" — it finishes, then commits.
    await waitFor(page, () => window.__lf.looper.trackInfo(2).retakePass === 2, 15000, 'lane 3 to roll into pass 2');
    const pressC = await page.evaluate(
      ({ gridAt, barSec }) =>
        new Promise((resolve) => {
          const lf = window.__lf;
          const ctx = lf.engine.ctx;
          const edge = gridAt + Math.ceil((ctx.currentTime + 0.3 - gridAt) / barSec) * barSec;
          const tick = () => {
            if (ctx.currentTime < edge - 0.06) return void setTimeout(tick, 2);
            const at = ctx.currentTime;
            lf.looper.playStop(2);
            resolve({ at, edge, stateAtPress: lf.looper.trackInfo(2).state });
          };
          tick();
        }),
      { gridAt: takeA.first, barSec },
    );
    await waitFor(page, () => window.__lf.looper.stateOf(2) !== 'RECORDING', 5000, 'the graced pass to finish');
    const takeC = await laneTimes(2);
    const stateC = await page.evaluate(() => window.__lf.looper.trackInfo(2).state);
    check('a press inside the grace lets the pass in flight finish', pressC.stateAtPress === 'RECORDING' && stateC === 'STOPPED',
      `atPress=${pressC.stateAtPress} (${((pressC.edge - pressC.at) * 1000).toFixed(0)} ms early), after=${stateC}`);
    check('…and keeps THAT pass, whole and on the grid',
      takeC.monotonic && Math.abs(takeC.first - (pressC.edge - barSec)) < FRAME_TOL && gridSlip(takeC.first, takeA.first) < FRAME_TOL,
      `monotonic=${takeC.monotonic}, first=${takeC.first.toFixed(5)}, want ${(pressC.edge - barSec).toFixed(5)}`);
    // The handoff seam: lane 3's first pass began exactly where lane 2's approved pass window ended.
    check('the handoff started lane 3 on a pass edge', gridSlip(takeC.first, takeB.first) < FRAME_TOL,
      `slip=${(gridSlip(takeC.first, takeB.first) * sr).toFixed(2)} frames`);

    // A dropout in a DISCARDED pass must not cost the clean pass approved later. The loss lands in pass 1:
    // pass 1 is dropped, pass 2 is skipped (the loss cannot be placed on one side of the edge), pass 3 counts.
    await page.evaluate(() => window.__lf.looper.recDub(3));
    await waitFor(page, () => window.__lf.looper.trackInfo(3).retakePass === 1, 15000, 'lane 4 to start rolling');
    await page.evaluate(() => window.__lf.pluginBridge.injectRecordLossForTest({ droppedFrames: 64, underruns: 0 }));
    await waitFor(page, () => window.__lf.looper.trackInfo(3).retakePass === 3, 15000, 'lane 4 to roll past the damage');
    const pressEarly = await page.evaluate(async () => {
      await window.__lf.looper.recDub(4); // nothing clean is kept yet: REC elsewhere must be ignored
      return { lane4: window.__lf.looper.trackInfo(3).state, lane5: window.__lf.looper.trackInfo(4).state };
    });
    check('with no clean pass kept, REC on another lane is ignored', pressEarly.lane4 === 'RECORDING' && pressEarly.lane5 === 'EMPTY',
      `lane4=${pressEarly.lane4}, lane5=${pressEarly.lane5}`);
    await waitFor(page, () => window.__lf.looper.trackInfo(3).retakePass === 4, 15000, 'lane 4 to finish a clean pass');
    await page.waitForTimeout(500);
    const pressD = await page.evaluate(async () => {
      const at = window.__lf.engine.ctx.currentTime;
      await window.__lf.looper.recDub(3);
      return { at, state: window.__lf.looper.trackInfo(3).state };
    });
    const takeD = await laneTimes(3);
    check('a loss in a discarded pass does not cost the clean pass approved later',
      pressD.state === 'PLAYING' && takeD.monotonic && takeD.first > pressD.at - 2 * barSec && takeD.first < pressD.at - barSec,
      `state=${pressD.state}, monotonic=${takeD.monotonic}, ${(pressD.at - takeD.first).toFixed(3)} s before the press`);

    await page.evaluate(() => {
      const lf = window.__lf;
      window.__retake.ramp.stop();
      window.__retake.ramp.disconnect();
      lf.looper.clearAll();
      lf.looper.setRetakeEnabled(false);
      lf.looper.setFixedLengthEnabled(false);
    });

    // ---- arrange the jam ------------------------------------------------------------------------
    await page.evaluate(
      ({ bpm, bars }) => {
        const lf = window.__lf;
        lf.clock.setBpmLocked(false);
        lf.clock.setBpm(bpm);
        lf.clock.setMetronome(false); // the click is audible-only; keep it out of the recorded tap
        lf.looper.setFixedLengthEnabled(true); // auto-commit on an exact frame — no timing-dependent stop press
        lf.looper.setFixedLengthBars(bars);
      },
      { bpm: BPM, bars: BARS },
    );

    // One AudioBufferSourceNode holding the whole impulse train: start(when) is sample-accurate, so
    // frame k of the buffer lands at exactly ctxFrame(when) + k. Many separate scheduled nodes would
    // measure the scheduler; one buffer measures the CAPTURE, which is the thing under test.
    await page.evaluate(
      ({ framesPerBeat, fingerprint }) => {
        const lf = window.__lf;
        const ctx = lf.engine.ctx;
        const seconds = 120; // covers the grid phase, the dispatcher phase and the deferred-commit phase
        const buf = ctx.createBuffer(1, Math.ceil(seconds * ctx.sampleRate), ctx.sampleRate);
        const d = buf.getChannelData(0);
        // One amplitude per beat, cycling every fingerprint.length beats = exactly one master loop.
        for (let k = 0, b = 0; k < d.length; k += framesPerBeat, b++) d[k] = fingerprint[b % fingerprint.length];
        const src = ctx.createBufferSource();
        src.buffer = buf;
        src.connect(lf.engine.looperInputBus);
        src.start(ctx.currentTime + 0.2);
        window.__jamSrc = src; // keep a handle so GC cannot collect it mid-take
      },
      { framesPerBeat, fingerprint: FINGERPRINT },
    );

    // ---- take 1: the grid-defining track --------------------------------------------------------
    // Count-in is always-on 1 bar, then fixed-length records exactly BARS bars and auto-commits.
    await page.evaluate(() => window.__lf.looper.recDub(0));
    await waitFor(page, () => window.__lf.looper.trackInfo(0).state === 'PLAYING', 25000, 'track 1 to commit');

    // ---- take 2: the later-track boundary arm ---------------------------------------------------
    await page.evaluate(() => window.__lf.looper.recDub(1));
    await waitFor(page, () => window.__lf.looper.trackInfo(1).state === 'PLAYING', 25000, 'track 2 to commit');

    // ---- measure --------------------------------------------------------------------------------
    const snap = await page.evaluate(
      ({ found, bpm, bars }) => {
        const lf = window.__lf;
        const s = lf.looper.exportSnapshot();
        // Keep a real session payload in the page for the dispatcher regressions below. This avoids
        // recording another 2-bar master just to exercise stop() after the recDub() abort case.
        window.__jamSession = {
          bpm,
          bars,
          masterLengthFrames: s.masterLengthFrames,
          tracks: s.tracks,
        };
        // Reduce PCM to impulse INDICES + amplitudes in the page (a 176 400-float array per track does
        // not need to cross the CDP bridge just to be scanned for peaks).
        return {
          sampleRate: s.sampleRate,
          masterLengthFrames: s.masterLengthFrames,
          overruns: lf.looper.captureOverruns(),
          quanta: lf.looper.captureQuanta(),
          tracks: s.tracks.map((t) => {
            const hits = [];
            const amps = [];
            for (let k = 0; k < t.pcm.length; k++) {
              const a = Math.abs(t.pcm[k]);
              if (a > found) {
                hits.push(k);
                amps.push(a);
              }
            }
            return { index: t.index, frames: t.pcm.length, hits, amps };
          }),
        };
      },
      { found: FOUND, bpm: BPM, bars: BARS },
    );

    console.log('golden-jam: measured\n');
    check('sample rate matches the context', snap.sampleRate === sr, `${snap.sampleRate} Hz`);
    check('two tracks committed', snap.tracks.length === 2, `got ${snap.tracks.length}`);
    check(
      `master is exactly ${BARS} bars`,
      snap.masterLengthFrames === expectedMaster,
      `${snap.masterLengthFrames} frames (expected ${expectedMaster})`,
    );
    check('capture dropped zero frames', snap.overruns === 0, `overruns=${snap.overruns}, quanta=${snap.quanta}`);

    for (const t of snap.tracks) {
      const n = `track ${t.index + 1}`;
      check(`${n} is exactly master-length`, t.frames === expectedMaster, `${t.frames} frames`);
      // A BARS-bar take spans BEATS beats, so it holds that many impulses or one fewer, depending on
      // where the take's frame 0 falls inside the (independently phased) impulse train.
      check(
        `${n} caught the impulse train`,
        t.hits.length === BEATS || t.hits.length === BEATS - 1,
        `${t.hits.length} impulses (expected ${BEATS - 1} or ${BEATS})`,
      );
      const gaps = t.hits.slice(1).map((h, k) => h - t.hits[k]);
      const bad = gaps.filter((g) => g !== framesPerBeat);
      check(
        `${n} every gap is exactly one beat`,
        gaps.length > 0 && bad.length === 0,
        `${gaps.length} gaps, ${bad.length} off-grid${bad.length ? ` (${[...new Set(bad)].join(',')})` : ''}`,
      );
      // The loop must close on the same grid it opened on: the tail after the last impulse plus the
      // head before the first must add up to exactly one beat, or the take wraps with a seam.
      const wrap = expectedMaster - t.hits[t.hits.length - 1] + t.hits[0];
      check(`${n} wrap closes to the sample`, wrap === framesPerBeat, `${wrap} frames`);
    }

    // The load-bearing one: a later track is claimed to be frame-identical BY CONSTRUCTION. Since the
    // master length is a whole number of beats, a frame-exact boundary arm must reproduce track 1's
    // impulse offsets EXACTLY. One frame of slip in the arm split shows up here and nowhere else.
    if (snap.tracks.length === 2) {
      const [a, b] = snap.tracks;
      const same = a.hits.length === b.hits.length && a.hits.every((h, k) => h === b.hits[k]);
      check(
        'track 2 is frame-identical to track 1 (the boundary arm is exact)',
        same,
        same ? `both at [${a.hits.slice(0, 3).join(',')}…]` : `t1=[${a.hits.join(',')}] t2=[${b.hits.join(',')}]`,
      );
    }

    // ---- short takes: a later take shorter than the master TILES across it -----------------------
    // F2/F1 (docs/plans/tester-feedback.md § Work order 2). A later take is a whole number of bars, at
    // most the master, repeated across the master-length region at commit — "3 over 8 sounds 3+3+2".
    // Driven through the REAL dispatchers (recDub/playStop + the FIXED auto-stop), never by calling
    // grid-math: what is under test is that stopCapture/finishRecording CHOOSE this window. The master is
    // BARS bars and the impulse train is still running, so a tiled lane is both sample-exactly periodic
    // and audibly non-silent — the two halves of the claim. Lane 3 is left EMPTY again for the sections
    // below, and FIXED is restored to the jam's BARS-bar arrangement.
    console.log('\ngolden-jam: short takes\n');
    const barFrames = framesPerBeat * BEATS_PER_BAR;
    const shortBarSec = barFrames / sr;

    /** One lane's tiling report: length, the first frame breaking pcm[k] === pcm[k % bar], bar-1 peak. */
    const tiling = async (idx, bar) =>
      page.evaluate(
        ({ idx, bar }) => {
          const lf = window.__lf;
          const t = lf.looper.exportSnapshot().tracks.find((x) => x.index === idx);
          if (!t) return null;
          const pcm = t.pcm;
          let mismatch = -1;
          for (let k = bar; k < pcm.length; k++) {
            if (pcm[k] !== pcm[k % bar]) {
              mismatch = k;
              break;
            }
          }
          let peak = 0;
          for (let k = 0; k < Math.min(bar, pcm.length); k++) {
            const a = Math.abs(pcm[k]);
            if (a > peak) peak = a;
          }
          return { length: pcm.length, mismatch, peak, state: lf.looper.stateOf(idx) };
        },
        { idx, bar },
      );

    // A. EARLY STOP. FIXED off, so the stop gesture alone decides the length: a press inside the FIRST
    // bar records on to that bar line (planLaterStop clamps to one bar) and the one bar tiles the master.
    await page.evaluate(() => window.__lf.looper.setFixedLengthEnabled(false));
    await page.evaluate(() => window.__lf.looper.recDub(2));
    await waitFor(
      page,
      () => {
        const t = window.__lf.looper.trackInfo(2);
        return t.state === 'RECORDING' && !t.armed;
      },
      15000,
      'the short take to cross its boundary arm',
    );
    const earlyStop = await page.evaluate(() => {
      const lf = window.__lf;
      lf.looper.playStop(2); // well inside bar 1 of the take
      return { stateAtPress: lf.looper.stateOf(2) };
    });
    await waitFor(page, () => window.__lf.looper.stateOf(2) !== 'RECORDING', 15000, 'the early stop to reach its bar line');
    const earlyTake = await tiling(2, barFrames);
    check(
      'a stop inside the first bar keeps recording on to the bar line',
      earlyStop.stateAtPress === 'RECORDING',
      `state right after the press = ${earlyStop.stateAtPress}`,
    );
    check(
      'the early-stopped take committed at MASTER length',
      earlyTake.state === 'STOPPED' && earlyTake.length === expectedMaster,
      `state=${earlyTake.state}, length=${earlyTake.length} (expected ${expectedMaster})`,
    );
    check(
      'the early-stopped take tiles its one bar across the master, sample-exact',
      earlyTake.mismatch === -1,
      `first mismatch at ${earlyTake.mismatch} (bar = ${barFrames} frames)`,
    );
    check('the tiled bar carries the recorded audio, not silence', earlyTake.peak > FOUND, `peak ${earlyTake.peak.toFixed(3)}`);

    // B. FIXED PRE-SELECTS. One bar, no stop press at all: the capture ends by itself and tiles.
    await page.evaluate(() => {
      const lf = window.__lf;
      lf.looper.clear(2);
      lf.looper.setFixedLengthBars(1);
      lf.looper.setFixedLengthEnabled(true);
    });
    const maxBars = await page.evaluate(() => window.__lf.looper.nextTakeMaxBars());
    check(`FIXED clamps the next take to the master's ${BARS} bars`, maxBars === BARS, `nextTakeMaxBars=${maxBars}`);
    await page.evaluate(() => window.__lf.looper.recDub(2));
    await waitFor(
      page,
      () => window.__lf.looper.stateOf(2) === 'PLAYING',
      25000,
      'the FIXED one-bar later take to commit by itself',
    );
    const fixedTake = await tiling(2, barFrames);
    check(
      'a FIXED one-bar later take committed at MASTER length with no stop press',
      fixedTake.length === expectedMaster,
      `length=${fixedTake.length} (expected ${expectedMaster})`,
    );
    check(
      'the FIXED one-bar take tiles across the master, sample-exact',
      fixedTake.mismatch === -1,
      `first mismatch at ${fixedTake.mismatch} (bar = ${barFrames} frames)`,
    );
    check('the FIXED tiled bar carries the recorded audio', fixedTake.peak > FOUND, `peak ${fixedTake.peak.toFixed(3)}`);

    // C. RETAKE OVERRIDES FIXED. FIXED stays at one bar; with a master present the rolling window must be
    // the MASTER (configureRecordingEnd's `!retakeEnabled()` clause). engineState is not exposed, so the
    // observable is the pass edge: pass 2 arrives after ~BARS bars, not after the FIXED single bar.
    await page.evaluate(() => {
      const lf = window.__lf;
      lf.looper.clear(2);
      lf.looper.setRetakeEnabled(true);
    });
    await page.evaluate(() => window.__lf.looper.recDub(3));
    const rollStart = await waitFor(
      page,
      () => {
        const t = window.__lf.looper.trackInfo(3);
        return t.state === 'RECORDING' && !t.armed ? { at: window.__lf.engine.ctx.currentTime } : null;
      },
      15000,
      'the RETAKE take to cross its boundary arm',
    );
    const passTwo = await waitFor(
      page,
      () =>
        window.__lf.looper.trackInfo(3).retakePass === 2 ? { at: window.__lf.engine.ctx.currentTime } : null,
      25000,
      'the RETAKE roll to reach pass 2',
    );
    const passSec = passTwo.at - rollStart.at;
    check(
      'RETAKE rolls at the MASTER length whatever FIXED says',
      passSec > 1.5 * shortBarSec && passSec < 3 * shortBarSec,
      `pass 1 lasted ${passSec.toFixed(2)} s (one FIXED bar = ${shortBarSec.toFixed(2)} s, master = ${(BARS * shortBarSec).toFixed(2)} s)`,
    );
    const shortRestore = await page.evaluate(
      ({ bars }) => {
        const lf = window.__lf;
        lf.looper.stop(3); // drop the rolling take; nothing was committed
        lf.looper.clear(3);
        lf.looper.setRetakeEnabled(false);
        lf.looper.setFixedLengthBars(bars);
        lf.looper.setFixedLengthEnabled(true);
        return {
          master: lf.looper.masterLengthFrames(),
          states: Array.from({ length: 5 }, (_, i) => lf.looper.stateOf(i)),
        };
      },
      { bars: BARS },
    );
    check(
      'the short-take section left the master and the free lanes as it found them',
      shortRestore.master === expectedMaster &&
        shortRestore.states[0] === 'PLAYING' &&
        shortRestore.states[1] === 'PLAYING' &&
        shortRestore.states.slice(2).every((s) => s === 'EMPTY'),
      `master=${shortRestore.master}, states=${shortRestore.states.join('/')}`,
    );

    // ---- dispatchers: the layer with literally zero verify/ contact -----------------------------
    // Everything above went through recDub + the auto-commit cap. These are the presses a player makes
    // that no guard has ever executed: overdub, undo/redo, stop, resume, clear, and a stop that lands
    // inside the first-track deferred-commit window.
    console.log('\ngolden-jam: dispatchers\n');
    const loopSeconds = expectedMaster / sr;

    // Overdub track 1 for a full loop. The impulse train is still running on the same beat grid, so a
    // CORRECT write-head sums each impulse onto ITSELF: same positions, and — because the train carries
    // a per-beat amplitude fingerprint that repeats once per loop — the same amplitude, so every hit
    // ends up an exact integer multiple of its pre-dub value. A sub-beat phase error scatters NEW
    // impulses between the old ones (count grows); a whole-beat one sums beat b onto beat b+N (count
    // unchanged, but the multiples stop being integers).
    await page.evaluate(() => window.__lf.looper.recDub(0));
    check(
      'overdub started',
      (await page.evaluate(() => window.__lf.looper.trackInfo(0).state)) === 'OVERDUBBING',
    );
    await page.waitForTimeout(loopSeconds * 1000 + 400);
    await page.evaluate(() => window.__lf.looper.recDub(0));
    await waitFor(page, () => window.__lf.looper.trackInfo(0).state === 'PLAYING', 8000, 'overdub to commit');

    /** Impulse indices + their amplitudes + peak for one track, read out of the live snapshot. */
    const probe = async (trackIndex) =>
      page.evaluate(
        ({ idx, found }) => {
          const t = window.__lf.looper.exportSnapshot().tracks.find((x) => x.index === idx);
          if (!t) return null;
          const hits = [];
          const amps = [];
          let peak = 0;
          for (let k = 0; k < t.pcm.length; k++) {
            const a = Math.abs(t.pcm[k]);
            if (a > found) {
              hits.push(k);
              amps.push(a);
            }
            if (a > peak) peak = a;
          }
          return { hits, amps, peak };
        },
        { idx: trackIndex, found: FOUND },
      );

    const dubbed = await probe(0);
    const base = snap.tracks[0];
    const onGrid = dubbed.hits.length === base.hits.length && dubbed.hits.every((h, k) => h === base.hits[k]);
    check(
      'overdub landed ON the grid, not beside it',
      onGrid,
      `${dubbed.hits.length} impulses (was ${base.hits.length}; a mis-phased dub would add more)`,
    );
    // The ABSOLUTE-phase half, and the only automated cover a whole-beat dub slip has: each hit must be
    // the SAME fingerprinted impulse summed onto itself, so amp/preDubAmp is a small integer >= 2. A
    // write head off by N whole beats still lands on beats and keeps the count — this is why the old
    // count+peak proof stayed green — but it sums beat b with beat b+N, and no such sum is an integer
    // multiple of beat b's amplitude (FINGERPRINT above: margin 0.031 against the 0.01 tolerance here).
    // The multiple is 2 across most of the loop and 3 over the ~400 ms of over-hold above, which re-dubs
    // the beats around the press point a second time. The upper bound is load-bearing, not cosmetic: the
    // no-integer-multiple guarantee only holds up to 3 dub passes over one beat (a mis-phased 5-pass sum
    // lands within 0.008 of an integer). Holding the overdub for several loops would break that, so this
    // fails loudly on the multiple instead of quietly weakening.
    const layers = onGrid ? dubbed.amps.map((a, k) => a / base.amps[k]) : [];
    const offPhase = layers.filter(
      (m) => Math.abs(m - Math.round(m)) > 0.01 || Math.round(m) < 2 || Math.round(m) > 4,
    );
    check(
      'overdub summed at the same ABSOLUTE loop position (a whole-beat slip would not)',
      onGrid && layers.length > 0 && offPhase.length === 0,
      `layer multiples [${layers.map((m) => m.toFixed(3)).join(' ')}]`,
    );
    check('overdub actually summed', dubbed.peak > IMPULSE * 1.5, `peak ${dubbed.peak.toFixed(3)}`);

    await page.evaluate(() => window.__lf.looper.undoLastOverdub(0));
    const undone = await probe(0);
    check('undo restored the pre-dub take', undone.peak < IMPULSE * 1.5, `peak ${undone.peak.toFixed(3)}`);
    await page.evaluate(() => window.__lf.looper.undoLastOverdub(0));
    const redone = await probe(0);
    check('redo restored the dub', redone.peak > IMPULSE * 1.5, `peak ${redone.peak.toFixed(3)}`);

    // A native monitor bypasses the JS PCM bridge, so it can sound clean while recordTap gets a hole.
    // Inject that exact fault after arm: the new layer must disappear, the previous PCM + undo target
    // must survive, and the expensive wet export must refuse to race the live audio graph. Recovery
    // snapshots stay allowed because they omit the OfflineAudioContext master render.
    const rejectedDub = await page.evaluate(async ({ bpm, bars }) => {
      const lf = window.__lf;
      const before = lf.looper.exportSnapshot().tracks.find((t) => t.index === 0).pcm;
      await lf.looper.recDub(0);
      let exportError = '';
      try {
        await lf.buildExportBundle({ bpm, bars });
      } catch (error) {
        exportError = String(error);
      }
      const recoveryBuilt = (await lf.buildExportBundle({ bpm, bars }, { includeMaster: false })) !== null;
      const exportDisabled = document.querySelector('[aria-label="Export loops as a zip of WAV files"]')?.disabled === true;
      lf.pluginBridge.injectRecordLossForTest({ droppedFrames: 128 });
      await lf.looper.recDub(0);
      const after = lf.looper.exportSnapshot().tracks.find((t) => t.index === 0).pcm;
      let exact = before.length === after.length;
      for (let i = 0; exact && i < before.length; i++) exact = before[i] === after[i];
      return {
        state: lf.looper.trackInfo(0).state,
        canUndo: lf.looper.trackInfo(0).canUndo,
        exact,
        exportError,
        exportDisabled,
        recoveryBuilt,
        toast: lf.notify.toasts().some((t) => t.message.includes('overdub layer discarded')),
      };
    }, { bpm: BPM, bars: BARS });
    check(
      'plugin loss rejected the overdub and restored its exact pre-layer PCM',
      rejectedDub.state === 'PLAYING' && rejectedDub.exact,
      `state=${rejectedDub.state}, exact=${rejectedDub.exact}`,
    );
    check('a rejected overdub preserved the older undo target', rejectedDub.canUndo === true);
    check('plugin-loss rejection reached the user', rejectedDub.toast === true);
    check(
      'wet export was blocked during capture while recovery remained available',
      rejectedDub.exportDisabled && rejectedDub.exportError.includes('Finish the active recording') && rejectedDub.recoveryBuilt,
      `disabled=${rejectedDub.exportDisabled}, recovery=${rejectedDub.recoveryBuilt}, error=${rejectedDub.exportError}`,
    );
    await page.evaluate(() => window.__lf.looper.undoLastOverdub(0));
    const undoAfterRejectedDub = await probe(0);
    check(
      'undo still reached the take from before the earlier successful overdub',
      undoAfterRejectedDub.peak < IMPULSE * 1.5,
      `peak ${undoAfterRejectedDub.peak.toFixed(3)}`,
    );
    await page.evaluate(() => window.__lf.looper.undoLastOverdub(0)); // restore the dubbed loop

    // Later-track failure: wait through its boundary arm, then force one worklet underrun and stop.
    // The later stop's whole-bar deadline reaches the real rejection path once its tail arrives.
    await page.evaluate(() => window.__lf.looper.recDub(2));
    await waitFor(
      page,
      () => window.__lf.looper.trackInfo(2).state === 'RECORDING' && !window.__lf.looper.trackInfo(2).armed,
      10000,
      'track 3 to cross its boundary arm',
    );
    await page.evaluate(() => {
      window.__lf.pluginBridge.injectRecordLossForTest({ underruns: 1 });
      window.__lf.looper.playStop(2);
    });
    await waitFor(page, () => window.__lf.looper.trackInfo(2).state !== 'RECORDING', 10000, 'track 3 to reach its bar line');
    const rejectedLaterTake = await page.evaluate(() => {
      const lf = window.__lf;
      return {
        state: lf.looper.trackInfo(2).state,
        length: lf.looper.trackInfo(2).lengthFrames,
        master: lf.looper.masterLengthFrames(),
      };
    });
    check(
      'plugin loss rejected a later take without disturbing the master',
      rejectedLaterTake.state === 'EMPTY' && rejectedLaterTake.length === 0 && rejectedLaterTake.master === expectedMaster,
      `state=${rejectedLaterTake.state}, length=${rejectedLaterTake.length}, master=${rejectedLaterTake.master}`,
    );

    // playStop on a PLAYING track halts it; a second press resumes (the resume() path). State only —
    // the resumed PHASE is not observable from recorded PCM (see KNOWN LIMITS at the top).
    await page.evaluate(() => window.__lf.looper.playStop(0));
    check('playStop halted the track', (await page.evaluate(() => window.__lf.looper.trackInfo(0).state)) === 'STOPPED');
    await page.evaluate(() => window.__lf.looper.playStop(0));
    check('playStop resumed the track', (await page.evaluate(() => window.__lf.looper.trackInfo(0).state)) === 'PLAYING');

    // ---- FROM THE TOP: an idle transport re-anchors, a live one is joined -----------------------
    // F5 (docs/plans/tester-feedback.md § Work order 2): with nothing playing and nothing recording, PLAY
    // starts from the top — ONE shared start time, the master grid re-anchored to it. While anything
    // plays, PLAY joins the live phase as before. The observable is `looper.phaseValue()`: capture.ts's
    // 25 ms drain tick recomputes it from `masterStartTime` whether or not anything records, so it reports
    // the GRID itself, not a playing source. Both presses are made from INSIDE the page at a deliberate
    // non-multiple of the loop (phase 0.30..0.45), so "near 0" and "kept running" are far apart.
    console.log('\ngolden-jam: play from the top\n');
    const PHASE_WINDOW = { lo: 0.3, hi: 0.45 };
    const phaseNow = () =>
      page.evaluate(() => ({ phase: window.__lf.looper.phaseValue(), now: window.__lf.engine.ctx.currentTime }));

    await page.evaluate(() => window.__lf.looper.stopAll());
    await waitFor(
      page,
      () => [0, 1].every((i) => window.__lf.looper.stateOf(i) === 'STOPPED'),
      10000,
      'every committed lane to reach STOPPED',
    );
    const fromTop = await page.evaluate(
      ({ lo, hi }) =>
        new Promise((resolve, reject) => {
          const lf = window.__lf;
          const deadline = performance.now() + 20000;
          const tick = () => {
            const phase = lf.looper.phaseValue();
            if (phase < lo || phase > hi) {
              if (performance.now() > deadline) return void reject(new Error(`never caught phase ${lo}..${hi}`));
              return void setTimeout(tick, 5);
            }
            const at = lf.engine.ctx.currentTime;
            lf.looper.playAll();
            resolve({ at, phase, states: [lf.looper.stateOf(0), lf.looper.stateOf(1)] });
          };
          tick();
        }),
      PHASE_WINDOW,
    );
    await page.waitForTimeout(120); // a few 25 ms drain ticks, so phaseValue has been recomputed
    const afterTop = await phaseNow();
    const sinceTop = afterTop.now - fromTop.at;
    check(
      'PLAY ALL from an idle transport resumed every stopped lane',
      fromTop.states.every((s) => s === 'PLAYING'),
      `states=${fromTop.states.join('/')}`,
    );
    check(
      'PLAY ALL from an idle transport restarted the master grid from the top',
      Math.abs(afterTop.phase * loopSeconds - sinceTop) < 0.09,
      `phase ${afterTop.phase.toFixed(3)} = ${(afterTop.phase * loopSeconds).toFixed(3)} s, ${sinceTop.toFixed(3)} s after a press made at phase ${fromTop.phase.toFixed(3)}`,
    );

    // The contrast: one lane keeps playing, so stopping and resuming the OTHER must join the running
    // phase — the grid keeps its anchor and no re-anchor happens behind the playing lane.
    const joined = await page.evaluate(
      ({ lo, hi }) =>
        new Promise((resolve, reject) => {
          const lf = window.__lf;
          const deadline = performance.now() + 20000;
          const tick = () => {
            const phase = lf.looper.phaseValue();
            if (phase < lo || phase > hi) {
              if (performance.now() > deadline) return void reject(new Error(`never caught phase ${lo}..${hi}`));
              return void setTimeout(tick, 5);
            }
            const at = lf.engine.ctx.currentTime;
            lf.looper.playStop(1); // STOP lane 2 …
            const stopped = lf.looper.stateOf(1);
            lf.looper.playStop(1); // … and resume it while lane 1 still plays
            resolve({ at, phase, stopped, resumed: lf.looper.stateOf(1), other: lf.looper.stateOf(0) });
          };
          tick();
        }),
      PHASE_WINDOW,
    );
    await page.waitForTimeout(120);
    const afterJoin = await phaseNow();
    const wantPhase = (joined.phase + (afterJoin.now - joined.at) / loopSeconds) % 1;
    const phaseDrift = Math.abs((((afterJoin.phase - wantPhase) % 1) + 1.5) % 1 - 0.5);
    check(
      'a stop+resume with another lane playing kept the running phase (no re-anchor)',
      joined.stopped === 'STOPPED' && joined.resumed === 'PLAYING' && joined.other === 'PLAYING' && phaseDrift < 0.02,
      `stopped=${joined.stopped}, resumed=${joined.resumed}, phase ${afterJoin.phase.toFixed(3)} vs the running ${wantPhase.toFixed(3)}`,
    );

    // COPY duplicates the WHOLE lane into the first EMPTY lane (track 3 here: 1 + 2 hold loops). The
    // source is dirtied first so "everything follows" is a real comparison, then the copy is reversed
    // to prove the two lanes do not share a backing array. Track 3 is cleared again afterwards and the
    // source restored, so the sections below see the same session as before.
    const copied = await page.evaluate(() => {
      const lf = window.__lf;
      const snapFx = (i) => lf.looper.fxState(i).map((f) => ({ bypassed: f.bypassed, params: { ...f.params } }));
      const pcmOf = (i) => Array.from(lf.looper.exportSnapshot().tracks.find((x) => x.index === i).pcm);
      const same = (a, b) => a.length === b.length && a.every((v, k) => v === b[k]);
      const before = { vol: lf.looper.trackVolume(0), muted: lf.looper.trackMuted(0), fx: snapFx(0) };
      lf.looper.setVolume(0, 0.4);
      lf.looper.setFxBypass(0, 0, false);
      lf.looper.setFxParam(0, 0, 'cutoff', 500);
      const srcPcm = pcmOf(0);
      const target = lf.looper.copy(0);
      const out = {
        target,
        state: lf.looper.trackInfo(2).state,
        length: lf.looper.trackInfo(2).lengthFrames,
        pcmEqual: same(srcPcm, pcmOf(2)),
        vol: lf.looper.trackVolume(2),
        mutedEqual: lf.looper.trackMuted(2) === lf.looper.trackMuted(0),
        fxEqual: JSON.stringify(snapFx(2)) === JSON.stringify(snapFx(0)),
        fxDistinct: lf.looper.fxState(2)[0].params !== lf.looper.fxState(0)[0].params,
      };
      lf.looper.reverse(2);
      out.sourceUntouched = same(srcPcm, pcmOf(0));
      out.copyReversed = same(srcPcm.slice().reverse(), pcmOf(2));
      lf.looper.clear(2);
      lf.looper.copy(9); // out-of-range source: guard no-op
      out.guardNoop = lf.looper.trackInfo(2).state === 'EMPTY';
      lf.looper.setVolume(0, before.vol);
      lf.looper.setFxParam(0, 0, 'cutoff', before.fx[0].params.cutoff);
      lf.looper.setFxBypass(0, 0, before.fx[0].bypassed);
      return out;
    });
    check('copy landed in the first empty lane', copied.target === 2, `target=${copied.target}`);
    check('the copy plays at master length', copied.state === 'PLAYING' && copied.length === expectedMaster, `state=${copied.state}, length=${copied.length}`);
    check('the copy is frame-identical to its source', copied.pcmEqual);
    check('volume, mute and FX followed the copy', copied.vol === 0.4 && copied.mutedEqual && copied.fxEqual, `vol=${copied.vol}, muted=${copied.mutedEqual}, fx=${copied.fxEqual}`);
    check('the copy shares no state with its source', copied.fxDistinct && copied.sourceUntouched && copied.copyReversed, `fx=${copied.fxDistinct}, source=${copied.sourceUntouched}, reversed=${copied.copyReversed}`);
    check('copy of a missing track is a no-op', copied.guardNoop);

    // CLEAR is the blank slate for the WHOLE lane: volume, mute AND FX reset in lockstep (owner product
    // call 2026-07-11 — a re-record used to play through the previous take's invisible filter/pitch/delay).
    // clear() replaces the whole five-entry fxState array, so the lane is dirtied across TWO effects and
    // in both dimensions (bypass flag + param value) and the entire array is compared back: reading one
    // flag of one effect would pass a reset that only touched effect 0, or only touched bypass flags.
    // The reference is the PRISTINE array snapshotted here, not the app's own defaultFxStates() — clear()
    // calls that function, so asserting against it would agree by construction.
    const cleared = await page.evaluate(() => {
      const lf = window.__lf;
      // fxState() hands back the live array; deep-copy or the "before" snapshot follows the edits.
      const snapFx = () => lf.looper.fxState(0).map((f) => ({ bypassed: f.bypassed, params: { ...f.params } }));
      const pristine = snapFx();
      lf.looper.setVolume(0, 0.3);
      lf.looper.setMute(0, true);
      lf.looper.setFxBypass(0, 0, false); // filter
      lf.looper.setFxBypass(0, 3, false); // delay
      lf.looper.setFxParam(0, 0, 'cutoff', 400);
      lf.looper.setFxParam(0, 3, 'feedback', 0.9);
      const dirty = { vol: lf.looper.trackVolume(0), muted: lf.looper.trackMuted(0), fx: snapFx() };
      lf.looper.clear(0);
      return {
        pristine,
        dirty,
        state: lf.looper.trackInfo(0).state,
        vol: lf.looper.trackVolume(0),
        muted: lf.looper.trackMuted(0),
        fx: snapFx(),
      };
    });
    /** Canonical JSON for FX state — key order must not decide whether two states are equal. */
    const fxJson = (v) =>
      JSON.stringify(v, (_k, val) =>
        val && typeof val === 'object' && !Array.isArray(val)
          ? Object.fromEntries(Object.keys(val).sort().map((key) => [key, val[key]]))
          : val,
      );
    const stillDirty = cleared.fx.filter((f, k) => fxJson(f) !== fxJson(cleared.pristine[k])).length;
    check(
      'the lane was actually dirtied first',
      cleared.dirty.vol === 0.3 &&
        cleared.dirty.muted === true &&
        fxJson(cleared.dirty.fx) !== fxJson(cleared.pristine),
    );
    check('clear emptied the track', cleared.state === 'EMPTY', cleared.state);
    check('clear reset volume', cleared.vol === 1, `${cleared.vol}`);
    check('clear reset mute', cleared.muted === false);
    check(
      'clear reset ALL FX entries in lockstep',
      stillDirty === 0 && cleared.fx.every((f) => f.bypassed === true),
      `${stillDirty} of ${cleared.fx.length} entries still off-default`,
    );

    // If the last committed lane is cleared while another lane waits for the boundary, cancelling
    // that arm must finish the transition to a blank session. Otherwise the invisible old grid keeps
    // BPM locked and the next REC takes the later-track path with no count-in.
    console.log('\ngolden-jam: blank-session reset\n');
    const recDubAbort = await page.evaluate(async () => {
      const lf = window.__lf;
      // Track 0 was cleared above; track 1 still owns the master.
      await lf.looper.recDub(0);
      const armed = lf.looper.trackInfo(0).armed;
      lf.looper.clear(1);
      const masterBeforeAbort = lf.looper.masterLengthFrames();
      await lf.looper.recDub(0); // RECORDING + armed -> stopRecording() abort
      return {
        armed,
        masterBeforeAbort,
        masterAfterAbort: lf.looper.masterLengthFrames(),
        bpmLocked: lf.clock.bpmLocked(),
        states: Array.from({ length: 5 }, (_, i) => lf.looper.trackInfo(i).state),
      };
    });
    check('recDub abort started from a later-track arm', recDubAbort.armed === true);
    check(
      'the grid survived while the arm was still live',
      recDubAbort.masterBeforeAbort === expectedMaster,
      `${recDubAbort.masterBeforeAbort} frames`,
    );
    check(
      'recDub armed abort reset the blank session',
      recDubAbort.masterAfterAbort === 0 &&
        recDubAbort.bpmLocked === false &&
        recDubAbort.states.every((s) => s === 'EMPTY'),
      `master=${recDubAbort.masterAfterAbort}, locked=${recDubAbort.bpmLocked}, states=${recDubAbort.states.join('/')}`,
    );

    // A bridge loss wholly inside discarded count-in pre-roll must NOT kill a clean take. The baseline
    // refreshes after each fully-discarded capture batch, but never after the batch containing frame 0.
    await page.evaluate(async () => {
      const lf = window.__lf;
      lf.looper.setFixedLengthEnabled(false);
      await lf.looper.recDub(0);
      lf.pluginBridge.injectRecordLossForTest({ droppedFrames: 32 });
    });
    await waitFor(
      page,
      () => window.__lf.looper.trackInfo(0).state === 'RECORDING' && !window.__lf.looper.trackInfo(0).armed,
      10000,
      'the clean first take to pass count-in',
    );
    await page.evaluate(() => window.__lf.looper.playStop(0));
    await waitFor(page, () => window.__lf.looper.stateOf(0) !== 'RECORDING', 5000, 'clean first capture tail to finish');
    const cleanAfterPrerollLoss = await page.evaluate(() => {
      const lf = window.__lf;
      return { state: lf.looper.trackInfo(0).state, master: lf.looper.masterLengthFrames() };
    });
    check(
      'loss confined to discarded pre-roll did not reject the take',
      cleanAfterPrerollLoss.state === 'STOPPED' && cleanAfterPrerollLoss.master > 0,
      `state=${cleanAfterPrerollLoss.state}, master=${cleanAfterPrerollLoss.master}`,
    );
    await page.evaluate(() => window.__lf.looper.clearAll());

    // First-track failure has no prior master to preserve: reject it back to the true blank slate and
    // release the press-time BPM lock. Stop just after count-in so capture completion follows quickly.
    await page.evaluate(async () => {
      const lf = window.__lf;
      lf.looper.setFixedLengthEnabled(false);
      await lf.looper.recDub(0);
    });
    await waitFor(
      page,
      () => window.__lf.looper.trackInfo(0).state === 'RECORDING' && !window.__lf.looper.trackInfo(0).armed,
      10000,
      'the corrupt first take to pass count-in',
    );
    await page.evaluate(() => {
      window.__lf.pluginBridge.injectRecordLossForTest({ droppedFrames: 64, underruns: 1 });
      window.__lf.looper.playStop(0);
    });
    await waitFor(page, () => window.__lf.looper.stateOf(0) !== 'RECORDING', 5000, 'corrupt first capture tail to finish');
    const rejectedFirstTake = await page.evaluate(() => {
      const lf = window.__lf;
      return {
        state: lf.looper.trackInfo(0).state,
        length: lf.looper.trackInfo(0).lengthFrames,
        master: lf.looper.masterLengthFrames(),
        bpmLocked: lf.clock.bpmLocked(),
      };
    });
    check(
      'plugin loss rejected the first take back to a blank, unlocked session',
      rejectedFirstTake.state === 'EMPTY' &&
        rejectedFirstTake.length === 0 &&
        rejectedFirstTake.master === 0 &&
        rejectedFirstTake.bpmLocked === false,
      `state=${rejectedFirstTake.state}, length=${rejectedFirstTake.length}, master=${rejectedFirstTake.master}, locked=${rejectedFirstTake.bpmLocked}`,
    );

    const stopAbort = await page.evaluate(async () => {
      const lf = window.__lf;
      await lf.looper.loadSession(window.__jamSession);
      lf.looper.clear(0); // leave track 1 as the committed master
      await lf.looper.recDub(0);
      const armed = lf.looper.trackInfo(0).armed;
      lf.looper.clear(1);
      const masterBeforeAbort = lf.looper.masterLengthFrames();
      lf.looper.stop(0); // direct STOP discard branch
      return {
        armed,
        masterBeforeAbort,
        masterAfterAbort: lf.looper.masterLengthFrames(),
        bpmLocked: lf.clock.bpmLocked(),
        states: Array.from({ length: 5 }, (_, i) => lf.looper.trackInfo(i).state),
      };
    });
    check('stop abort started from a later-track arm', stopAbort.armed === true);
    check(
      'the imported grid survived while the stop arm was live',
      stopAbort.masterBeforeAbort === expectedMaster,
      `${stopAbort.masterBeforeAbort} frames`,
    );
    check(
      'stop armed abort reset the blank session',
      stopAbort.masterAfterAbort === 0 &&
        stopAbort.bpmLocked === false &&
        stopAbort.states.every((s) => s === 'EMPTY'),
      `master=${stopAbort.masterAfterAbort}, locked=${stopAbort.bpmLocked}, states=${stopAbort.states.join('/')}`,
    );

    // ---- the deferred first-track commit --------------------------------------------------------
    // Free-record from a blank slate, then press STOP. stopRecording may defer the commit to consume()'s
    // frame cap (the take's tail is still in flight); the stop intent has to survive that window or the
    // take is aborted and discarded. This is the exact shape of one of the three bugs that left all 9560
    // static checks green.
    console.log('\ngolden-jam: deferred first-track commit\n');
    await page.evaluate(() => {
      const lf = window.__lf;
      lf.looper.clearAll();
      lf.looper.setFixedLengthEnabled(false); // free record: the commit length comes from the stop press
      lf.clock.setBpmLocked(false);
    });
    check('blank slate', (await page.evaluate(() => window.__lf.looper.masterLengthFrames())) === 0);

    await page.evaluate(() => window.__lf.looper.recDub(0));
    await waitFor(
      page,
      () => window.__lf.looper.trackInfo(0).state === 'RECORDING' && !window.__lf.looper.trackInfo(0).armed,
      20000,
      'the count-in to finish',
    );
    // The deferred window is narrow and must be hit deliberately, not by sleeping: stopRecording defers
    // only while the wall clock already reads a whole bar but the drained frame count does not yet. Press
    // from INSIDE the page, a drain tick or two short of the 2-bar mark, so the press reliably lands there.
    const pressedAt = await page.evaluate(
      async ({ target }) => {
        const lf = window.__lf;
        const deadline = performance.now() + 30000;
        while (performance.now() < deadline) {
          const fill = lf.looper.fillFramesOf(0);
          if (fill >= target - 2600 && fill < target) {
            lf.looper.playStop(0);
            return { fill, stateRightAfter: lf.looper.trackInfo(0).state };
          }
          await new Promise((r) => setTimeout(r, 4));
        }
        throw new Error(`never caught the deferred window (fill=${lf.looper.fillFramesOf(0)})`);
      },
      { target: expectedMaster },
    );
    check(
      'the stop press landed inside the deferred-commit window',
      pressedAt.stateRightAfter === 'RECORDING',
      `fill=${pressedAt.fill}/${expectedMaster}, state right after the press = ${pressedAt.stateRightAfter}`,
    );
    const after = await waitFor(
      page,
      () => {
        const t = window.__lf.looper.trackInfo(0);
        return t.state !== 'RECORDING' ? { state: t.state, length: t.lengthFrames } : null;
      },
      10000,
      'the deferred commit to resolve',
    );
    check('the take survived the stop press', after.state === 'STOPPED', `state=${after.state}`);
    check('the take committed to whole bars', after.length === expectedMaster, `${after.length} frames`);

    // ---- local recovery ------------------------------------------------------------------------
    // The same browser context keeps IndexedDB across reloads. Prove the full production seam:
    // clear any prior slot, change a committed mix value, wait for the real debounced autosave, then
    // reload and let App's startup restore run through importSession. CLEAR ALL + flush + one final
    // reload proves stale audio does not resurrect.
    console.log('\ngolden-jam: local recovery\n');
    await page.evaluate(async () => {
      await window.__lf.autosave.clearSaved();
      window.__lf.looper.setVolume(0, 0.93); // change the fingerprint after deleting any earlier save
    });
    const savedRecovery = await waitFor(
      page,
      () => window.__lf.autosave.hasSaved(),
      8000,
      'the stable jam to autosave without an explicit flush',
    );
    check('stable changes autosaved without an explicit flush', savedRecovery === true);

    const recoveryPcmHash = () => {
      const pcm = window.__lf.looper.exportSnapshot().tracks[0].pcm;
      return crypto.subtle.digest('SHA-256', pcm).then((hash) =>
        Array.from(new Uint8Array(hash), (byte) => byte.toString(16).padStart(2, '0')).join(''));
    };
    const beforeRecoveryHash = await page.evaluate(recoveryPcmHash);

    page.on('dialog', (dialog) => void dialog.accept());
    await page.reload({ waitUntil: 'domcontentloaded' });
    await waitFor(page, () => typeof window.__lf !== 'undefined', 20000, 'the reloaded __lf debug hook');
    await page.evaluate(() => window.__lf.autosave.ready());
    const afterRecoveryHash = await page.evaluate(recoveryPcmHash);
    check('recovery preserves every recorded Float32 sample', beforeRecoveryHash === afterRecoveryHash,
      `${beforeRecoveryHash} -> ${afterRecoveryHash}`);
    const restored = await page.evaluate(() => ({
      master: window.__lf.looper.masterLengthFrames(),
      state: window.__lf.looper.trackInfo(0).state,
      length: window.__lf.looper.trackInfo(0).lengthFrames,
      peak: window.__lf.looper.trackPeak(0),
      volume: window.__lf.looper.trackVolume(0),
    }));
    check(
      'startup restored the committed loop without resuming the stopped track',
      restored.master === expectedMaster &&
        restored.state === 'STOPPED' &&
        restored.length === expectedMaster &&
        restored.peak > FOUND &&
        restored.volume === 0.93,
      `master=${restored.master}, state=${restored.state}, length=${restored.length}, peak=${restored.peak}, volume=${restored.volume}`,
    );

    const clearedRecovery = await page.evaluate(async () => {
      window.__lf.looper.clearAll();
      await window.__lf.autosave.flush();
      return {
        master: window.__lf.looper.masterLengthFrames(),
        saved: await window.__lf.autosave.hasSaved(),
      };
    });
    check(
      'CLEAR ALL removed the local recovery',
      clearedRecovery.master === 0 && clearedRecovery.saved === false,
      `master=${clearedRecovery.master}, saved=${clearedRecovery.saved}`,
    );

    await page.reload({ waitUntil: 'domcontentloaded' });
    await waitFor(page, () => typeof window.__lf !== 'undefined', 20000, 'the final __lf debug hook');
    await page.evaluate(() => window.__lf.autosave.ready());
    const stayedBlank = await page.evaluate(() => ({
      master: window.__lf.looper.masterLengthFrames(),
      states: Array.from({ length: 5 }, (_, i) => window.__lf.looper.trackInfo(i).state),
    }));
    check(
      'cleared recovery stayed blank after reload',
      stayedBlank.master === 0 && stayedBlank.states.every((state) => state === 'EMPTY'),
      `master=${stayedBlank.master}, states=${stayedBlank.states.join('/')}`,
    );

    const expectedLossErrors = pageErrors.filter((e) => e.includes('[looper] rejected track'));
    const retakeDrops = pageErrors.filter((e) => e.includes('[looper] retake: dropped track'));
    const unexpectedPageErrors = pageErrors.filter(
      (e) => !e.includes('[looper] rejected track') && !e.includes('[looper] retake: dropped track'),
    );
    check('the damaged retake pass was reported exactly once', retakeDrops.length === 1, `${retakeDrops.length} errors`);
    check('each injected loss reached the release-log channel', expectedLossErrors.length === 3, `${expectedLossErrors.length} errors`);
    check('no unexpected page errors during the jam', unexpectedPageErrors.length === 0, unexpectedPageErrors.slice(0, 3).join(' | '));
  } finally {
    await browser.close();
    if (vite) {
      // shell:true means vite.pid is the SHELL — a plain kill orphans the real vite (observed on
      // Windows 2026-08-12: orphaned dev servers accumulated across runs). taskkill /T takes the tree.
      if (process.platform === 'win32') {
        spawnSync('taskkill', ['/pid', String(vite.pid), '/T', '/F'], { stdio: 'ignore' });
      } else {
        vite.kill('SIGTERM');
        // vite's child esbuild/rolldown workers ignore a plain SIGTERM on the parent occasionally.
        setTimeout(() => vite.kill('SIGKILL'), 2000).unref();
      }
    }
  }
}

main()
  .then(() => {
    console.log(`\n=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
    process.exit(failed === 0 ? 0 : 1);
  })
  .catch((e) => {
    console.error(`\ngolden-jam: ${e.message}`);
    console.log(`\n=== RESULT: ${passed}/${passed + failed + 1} checks passed, ${failed + 1} failed ===`);
    process.exit(1);
  });
