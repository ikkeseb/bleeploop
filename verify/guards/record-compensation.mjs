// Executable verification of the AUTOMATIC record-latency compensation: the pure formula
// (src/audio/record-latency-math.ts computeC, median) swept across rig-like terms, and the REAL
// src/audio/record-latency.ts sampler/freeze plus the REAL looper capture window driven through the
// verify rig.
//
// Latency itself is HARDWARE and can NOT be deterministically verified — the ear/waveform on the rig is the only
// gate for "the loop lands on the click". What this PROVES is the LOGIC around it:
//   0. The real sampler sums BOTH bridge hops (hop1Lag + hop2Fill); a plugin-less slot contributes 0.
//   A. C = max(0, round((hop/sr + 128/sr − cpalOut + clickOut + trim/1000)·sr)), clickOut=max(outLat,cpalOut) [computeC]
//   B. THE LOAD-BEARING PHYSICS (physical regime, reported outLat ≥ cpalOut): modelling the full
//      record/monitor timeline with ARBITRARY input + plugin latency, the recorded transient lands on the
//      take's frame 0 (within rounding) — i.e. input+plugin CANCEL and the SIGN is right (discard C MORE
//      frames ⇒ late transient → frame 0)
//   C. C = 0 whenever no native monitor is armed / compensation is disabled, and the real count-in window
//      is then exactly today's                                              [recordCompensationFrames early return]
//   D/H. UNIFORM SHIFT on the real looper: a FIXED take with code compensation C_code, recorded through a
//      path that is really C_true late, is exactly N bars long for any C and holds the played frames shifted
//      by Δ = C_true − C_code: the loss is |Δ| at one end, never C              [startRecording + capture]
//   E. THE clickOut FLOOR (a HEURISTIC, toggle setFloorEnabled): clickOut = max(reported outLat, cpalOut).
//      This proves BOTH SIDES of its tradeoff, not just the win:
//        E1 — a Chromium/WebView2 outputLatency under-report can NOT drive C below the record-path floor (hop+quantum).
//        E2 — BUT it is NOT an unconditional lower bound: when the report is ACCURATE and cpalOut carries the native
//             monitor's PRODUCER-RING excess, cpalOut can exceed the true clickOut and the floor OVER-compensates by
//             exactly (cpalOut − reported)·sr (guitar EARLY), while the UN-floored formula equals the ideal. Which
//             regime a rig is in is a by-ear/`[rec-comp]`-log call.                                   [computeC]
//   F. OVERDUB compensation maps wet frames back by the SAME C onto the master grid     [compensatedLoopFrame]
//   G. WINDOW-MEDIAN snapshot on the real module: the sampler's rolling window is frozen at FIRST record use,
//      C is identical take-to-take and reproducible across re-arms, a new generation clears the old window,
//      settle updates only before first use, clear → C=0, and the by-ear trim stays live atop the frozen
//      snapshot.                                                [record-latency.ts sampler / generation / freeze]

let fails = 0,
  checks = 0;
function ok(name, cond, detail = '') {
  checks++;
  if (!cond) {
    fails++;
    console.log(`  FAIL  ${name}  ${detail}`);
  }
}

import { bootLooper } from '../harness/rig.ts';
import {
  SAMPLE_INTERVAL_MS,
  WORKLET_QUANTUM_FRAMES as QUANTUM,
  computeC,
  median as medianRing,
} from '../../src/audio/record-latency-math.ts';

// ── record-latency-math.ts: computeC() — the C formula (reported-latency mode) ─────────────────────
// C = max(0, round((hop/sr + 128/sr − cpalOut + clickOut + trim/1000)·sr)).
// The historical floor and 128-frame allowance are retained only for unavailable timestamps.
// They are heuristics, not physical bounds. Timestamp-mode phase cancellation has its own guard
// in verify/guards/render-cursor.mjs; the actual sampler/freeze is exercised by render-cursor.mjs.
function compFrames({ hop, cpalOut, outLat, trim = 0, sr, floorEnabled = true }) {
  return computeC(
    { hop1Frames: hop, hop2Frames: 0, cpalOutSeconds: cpalOut, baseLatency: 0, outputLatency: outLat, trimMs: trim, floorEnabled },
    sr,
  ).frames;
}

/**
 * A real record-latency module on a fresh rig, in the reported-latency regime (no output timestamps).
 * `tick(outLat, hop1, hop2)` sets what the sampler reads next and advances one sampler interval.
 */
async function latencySession({ sr = 48000 } = {}) {
  const rig = await bootLooper({ sampleRate: sr, init: false });
  await rig.engine.start();
  const latency = await rig.import('audio/record-latency.ts');
  const { pluginBridge } = await rig.import('audio/plugin-bridge.ts');
  let stats = null;
  pluginBridge.stats = () => stats;
  rig.ctx.getOutputTimestamp = undefined;
  rig.ctx.baseLatency = 0;
  const set = (outLat, hop1, hop2 = 0) => {
    rig.ctx.outputLatency = outLat;
    stats = hop1 === null ? null : { hop1Lag: hop1, hop2Fill: hop2 };
  };
  const tick = async (outLat, hop1, hop2 = 0) => {
    set(outLat, hop1, hop2);
    await rig.advance(SAMPLE_INTERVAL_MS / 1000);
  };
  return { rig, latency, set, tick, C: () => latency.recordCompensationFrames() };
}

// ── Section 0 — the real sampler sums BOTH bridge hops (hop1Lag + hop2Fill) ────────────────────────
{
  const sr = 48000;
  const s0 = await latencySession({ sr });
  s0.set(0.012, 130, 1440);
  s0.latency.beginMonitorGeneration(0, 0.006);
  for (let i = 0; i < 5; i++) await s0.tick(0.012, 130, 1440);
  ok('0 the sampled bridge term is hop1 + hop2', s0.C() === compFrames({ hop: 130 + 1440, cpalOut: 0.006, outLat: 0.012, sr }) &&
    s0.latency.lastCompensation().hopFrames === 1570, `C=${s0.C()} hop=${s0.latency.lastCompensation().hopFrames}`);
  const s1 = await latencySession({ sr });
  s1.set(0, null);
  s1.latency.beginMonitorGeneration(0, 0);
  ok('0 a plugin-less slot (no stats) ⇒ both hops 0', s1.C() === QUANTUM, `C=${s1.C()}`);
}

// ── Section A — formula, sign, clamp ──────────────────────────────────────────────────────────────
{
  const sr = 48000;
  // Typical Focusrite/ASIO-ish: hop ~30ms, cpalOut ~6ms, outLat ~12ms.
  const hop = Math.round(0.03 * sr); // 1440
  const c = compFrames({ hop, cpalOut: 0.006, outLat: 0.012, sr });
  const expect = Math.max(0, Math.round((hop / sr + QUANTUM / sr - 0.006 + 0.012) * sr));
  ok('A formula matches the source expression', c === expect, `got ${c} want ${expect}`);
  ok('A typical C is positive (record path lags)', c > 0, `c=${c}`);
  // hop dominates, cpalOut subtracts, outLat adds: bump each term, C moves the right way.
  ok(
    'A C rises with hop backlog',
    compFrames({ hop: hop + 480, cpalOut: 0.006, outLat: 0.012, sr }) > c,
  );
  ok(
    'A C falls as cpalOut grows (un-floored regime: cpalOut < reported clickOut)',
    compFrames({ hop, cpalOut: 0.009, outLat: 0.015, sr }) < compFrames({ hop, cpalOut: 0.004, outLat: 0.015, sr }),
  );
  ok(
    'A C rises with outputLatency',
    compFrames({ hop, cpalOut: 0.006, outLat: 0.02, sr }) > c,
  );
  // With the ROBUST FLOOR a pathological monitor latency larger than the reported click latency can no
  // longer drive C negative — the output terms cancel and C falls back to the record-path floor
  // (hop + quantum), NOT 0. (This is the 2026-06-29 "C undershoots" fix; pre-floor this expected 0.)
  ok(
    'A floor: cpalOut > reported clickOut ⇒ C = record-path floor (hop+quantum), not 0',
    compFrames({ hop: 0, cpalOut: 0.1, outLat: 0, sr }) === QUANTUM,
    `${compFrames({ hop: 0, cpalOut: 0.1, outLat: 0, sr })}`,
  );
  // Trim shifts C by trim·sr; a negative trim past the rest clamps to 0.
  ok(
    'A +trim shifts C by trim·sr',
    compFrames({ hop, cpalOut: 0.006, outLat: 0.012, trim: 5, sr }) === c + Math.round(0.005 * sr),
  );
  ok(
    'A −trim can clamp to 0',
    compFrames({ hop: 48, cpalOut: 0, outLat: 0, trim: -100, sr }) === 0,
  );
  // Sample-rate independence of the seconds it represents: C/sr ≈ same ms at 44.1k and 48k.
  const c441 = compFrames({ hop: Math.round(0.03 * 44100), cpalOut: 0.006, outLat: 0.012, sr: 44100 });
  ok('A ~same latency in ms across sr', Math.abs(c441 / 44100 - c / 48000) < 0.001, `${c441 / 44100} vs ${c / 48000}`);
}

// ── Section B — THE LOAD-BEARING PHYSICS: input+plugin cancel, transient lands on frame 0 ──────────
// Model the full timeline (all times seconds, ctx==wall clock for the recorded path):
//   L_mon = input + plugin + cpalOut         (player HEARS their guitar through the native monitor)
//   L_rec = input + plugin + hop + quantum   (the wet reaches the looper record tap)
//   player aligns native-monitored guitar to the HEARD click: t_play = downbeat + outLat − L_mon
//   recorded transient at the tap:           t_recorded = t_play + L_rec
//   the take's frame 0 (we discard lead + C): t_take0 = downbeat + C/sr
// PROVE: |t_recorded − t_take0| < 1 frame for ARBITRARY input/plugin (they must cancel) and the sign.
{
  const downbeat = 1.0; // scheduled count downbeat (ctx s); the grid anchors here (firstTakeDownbeatCtx)
  let worst = 0;
  for (const sr of [44100, 48000]) {
    for (const input of [0.001, 0.005, 0.012, 0.05]) {
      // unmeasurable input latency — must drop out
      for (const plugin of [0, 0.003, 0.02]) {
        for (const hopMs of [10, 30, 55]) {
          for (const cpalOut of [0.003, 0.006, 0.012]) {
            for (const outLat of [0.005, 0.012, 0.02]) {
              // Section B = the PHYSICAL regime, where the reported click latency is ≥ the monitor latency
              // (the click path / WASAPI is never faster than the native ASIO monitor). The floor is a
              // no-op here, so it tests the raw cancellation. The bogus-report regime (outLat < cpalOut,
              // where the floor actively repairs C) is proven separately in Section E.
              if (outLat < cpalOut) continue;
              const quantum = QUANTUM / sr;
              const hop = Math.round((hopMs / 1000) * sr);
              const Lmon = input + plugin + cpalOut;
              const Lrec = input + plugin + hop / sr + quantum;
              const tPlay = downbeat + outLat - Lmon;
              const tRecorded = tPlay + Lrec;
              const C = compFrames({ hop, cpalOut, outLat, sr });
              const tTake0 = downbeat + C / sr;
              const errFrames = Math.abs(tRecorded - tTake0) * sr;
              worst = Math.max(worst, errFrames);
            }
          }
        }
      }
    }
  }
  // The only residual is the round() in C (≤ 0.5 frame). If input/plugin did NOT cancel, the error would
  // be tens/hundreds of frames — this sweep would explode.
  ok('B transient lands on take frame 0 for ALL input/plugin (cancellation + sign)', worst <= 0.5, `worst=${worst.toFixed(3)} frames`);

  // Explicit sign sanity: WITHOUT compensation (C=0) the transient is C-late; WITH it, ~0.
  const sr = 48000,
    hop = Math.round(0.03 * sr),
    cpalOut = 0.006,
    outLat = 0.012,
    input = 0.01,
    plugin = 0.004;
  const Lmon = input + plugin + cpalOut;
  const Lrec = input + plugin + hop / sr + QUANTUM / sr;
  const tRecorded = downbeat + outLat - Lmon + Lrec;
  const offNoComp = (tRecorded - downbeat) * sr; // = C frames late
  const C = compFrames({ hop, cpalOut, outLat, sr });
  ok('B uncompensated take is C frames LATE', Math.abs(offNoComp - C) <= 0.5, `off=${offNoComp.toFixed(2)} C=${C}`);
  ok('B compensated take is on frame 0', Math.abs(tRecorded - (downbeat + C / sr)) * sr <= 0.5);
}

// ── Section C — C = 0 baseline (no monitor / disabled) ⇒ the count-in window is today's ─────────────
{
  const sr = 48000;
  const sc = await latencySession({ sr });
  ok('C no monitor armed ⇒ C=0', sc.C() === 0);
  sc.latency.setOffsetMs(50);
  sc.set(0.012, 9999);
  sc.latency.beginMonitorGeneration(0, 0.006);
  ok('C precondition: an armed monitor with trim ⇒ C>0', sc.C() > 0);
  sc.latency.setEnabled(false);
  ok('C compensation disabled ⇒ C=0', sc.C() === 0);
  sc.latency.setEnabled(true);
  sc.latency.clearMonitor(1);
  ok('C clearing another slot leaves the monitor armed', sc.C() > 0);
  sc.latency.clearMonitor(0);
  ok('C clearing the armed slot ⇒ C=0', sc.C() === 0);

  // On the real looper: a count-in with the monitor disabled opens exactly at the counted downbeat.
  const rig = await bootLooper({ sampleRate: sr, startTime: 10 });
  const latency = await rig.import('audio/record-latency.ts');
  latency.beginMonitorGeneration(0, 0);
  latency.setOffsetMs(50);
  latency.setEnabled(false);
  const mark = rig.draws().length;
  await rig.looper.recDub(0);
  const downbeat = rig.draws().slice(mark).find((d) => d.countLeft === 4).time + 4 * (60 / rig.clock.bpm());
  const es = rig.state.engineState;
  ok('C count-in with C disabled: the take window opens on the downbeat', (es.recording?.compensationFrames ?? 0) === 0 &&
    (es.recording?.startFrame ?? null) === Math.round(downbeat * sr), `start=${es.recording?.startFrame ?? null} downbeat=${Math.round(downbeat * sr)}`);
}

// ── Section E1 — the clickOut FLOOR repairs a WebView2 under-report ─────────────────────────────────
// The Web-Audio click exits via the OS device; a bogus Chromium/WebView2 outputLatency under-report (≈0)
// collapses +outLat, leaving a net −cpalOut that drives C too small (the lag symptom). PROVE the floor's WIN:
// a bogus-small report can't drive C below the record-path floor; outLat==cpalOut gives exactly the floor;
// it equals the un-floored formula when the report is already ≥ cpalOut. (The floor's COST — over-comp when
// the report is accurate AND cpalOut carries the monitor producer-ring excess — is proven in Section E2.)
{
  const sr = 48000;
  const hop = Math.round(0.03 * sr); // 1440
  const floorC = hop + QUANTUM; // the pure record-path latency in frames (output terms cancelled)

  // Bogus report (outputLatency under-reported to ~0) with a real ASIO monitor latency: WITHOUT the floor C
  // would be hop+quantum − cpalOut·sr (the undershoot); WITH the floor the output terms cancel ⇒ C == floor.
  for (const cpalOut of [0.006, 0.012, 0.02]) {
    ok(
      `E1 bogus outLat≈0 floors C to the record-path latency (cpalOut=${cpalOut * 1000}ms)`,
      compFrames({ hop, cpalOut, outLat: 0, sr }) === floorC,
      `${compFrames({ hop, cpalOut, outLat: 0, sr })} vs ${floorC}`,
    );
    // The floor STRICTLY beats the old (un-floored) formula in the bug regime (adds back ~cpalOut·sr).
    const unfloored = Math.max(0, Math.round((hop / sr + QUANTUM / sr - cpalOut + 0) * sr));
    ok(
      `E1 floor > un-floored C in the under-report regime (cpalOut=${cpalOut * 1000}ms)`,
      compFrames({ hop, cpalOut, outLat: 0, sr }) > unfloored,
    );
  }

  // Degenerate: reported clickOut exactly == cpalOut ⇒ output terms cancel ⇒ record-path floor.
  ok('E1 outLat == cpalOut ⇒ C is exactly the record-path floor', compFrames({ hop, cpalOut: 0.01, outLat: 0.01, sr }) === floorC);

  // No-op when reported ≥ cpalOut (floor doesn't bite): floored value == un-floored value.
  for (const [cpalOut, outLat] of [[0.006, 0.012], [0.005, 0.02], [0.008, 0.03]]) {
    const flo = compFrames({ hop, cpalOut, outLat, sr });
    const unf = Math.max(0, Math.round((hop / sr + QUANTUM / sr - cpalOut + outLat) * sr));
    ok(`E1 floor is a no-op when reported ≥ cpalOut (cpalOut=${cpalOut * 1000} outLat=${outLat * 1000})`, flo === unf, `${flo} vs ${unf}`);
  }

  // Safety: as the report degrades (outLat → 0), C never falls below the record-path floor.
  let neverBelow = true;
  for (const outLat of [0.03, 0.02, 0.012, 0.006, 0.001, 0]) {
    if (compFrames({ hop, cpalOut: 0.012, outLat, sr }) < floorC) neverBelow = false;
  }
  ok('E1 C never drops below the record-path floor as the report degrades', neverBelow);
}

// ── Section E2 — the FLOOR's COST: it OVER-compensates on an ACCURATE report (the 2026-06-29 adversarial
// finding). The floor is NOT an unconditional lower bound. cpal_out = (monitor_fill + monitor_out_block)/R_out
// carries the native monitor's producer-ring setpoint (MONITOR_TARGET 10ms ASIO / 20ms WASAPI, plugin_host.rs)
// that the Web-Audio click has no analogue for, so cpal_out can EXCEED the true clickOut. When outputLatency is
// reported ACCURATELY (reported == true clickOut) but reported < cpalOut, the floor lifts clickOut up to cpalOut
// ⇒ C is (cpalOut − reported)·sr too LARGE ⇒ the take starts too late ⇒ the guitar lands EARLY. The UN-floored
// formula (setFloorEnabled(false)) is then exactly the ideal. This section PINS that behaviour so it is visible,
// not hidden (Section B `continue`s past outLat<cpalOut; pre-this-section nothing modelled the floored regime).
{
  const sr = 48000;
  const hop = Math.round(0.03 * sr); // 1440
  // Accurate report, but cpalOut inflated by the monitor ring above the true clickOut (reported == true clickOut).
  // ASIO-ish: cpalOut ≈ 10ms ring + 5.3ms device = 15.3ms; true clickOut (reported) ≈ baseLatency 2.7 + outLat 10 = 12.7ms.
  for (const [cpalOut, reported] of [[0.0153, 0.0127], [0.030, 0.0127], [0.012, 0.006]]) {
    const floored = compFrames({ hop, cpalOut, outLat: reported, sr, floorEnabled: true });
    const unfloored = compFrames({ hop, cpalOut, outLat: reported, sr, floorEnabled: false });
    const ideal = Math.max(0, Math.round((hop / sr + QUANTUM / sr - cpalOut + reported) * sr)); // uses the TRUE clickOut
    // The un-floored formula equals the ideal when the report is accurate.
    ok(`E2 un-floored == ideal on an accurate report (cpalOut=${cpalOut * 1000} rep=${reported * 1000})`, unfloored === ideal, `${unfloored} vs ${ideal}`);
    // The floor OVER-compensates by exactly (cpalOut − reported)·sr (positive ⇒ C too large ⇒ guitar EARLY).
    const overBy = Math.round((cpalOut - reported) * sr);
    ok(`E2 floor over-compensates by (cpalOut−reported)·sr (cpalOut=${cpalOut * 1000} rep=${reported * 1000})`, floored - ideal === overBy, `over ${floored - ideal} want ${overBy}`);
    ok(`E2 floored C > ideal in the ring-excess regime (cpalOut=${cpalOut * 1000})`, floored > ideal);
  }
  // setFloorEnabled is a real A/B: floored != un-floored exactly when the report is below cpalOut.
  ok('E2 toggle changes C iff report < cpalOut', compFrames({ hop, cpalOut: 0.02, outLat: 0.01, sr, floorEnabled: true }) !== compFrames({ hop, cpalOut: 0.02, outLat: 0.01, sr, floorEnabled: false }));
  ok('E2 toggle is a no-op when report ≥ cpalOut', compFrames({ hop, cpalOut: 0.006, outLat: 0.02, sr, floorEnabled: true }) === compFrames({ hop, cpalOut: 0.006, outLat: 0.02, sr, floorEnabled: false }));
}

// ── Section F — timestamped OVERDUB compensation (actual pure capture mapping) ──────────────
import { compensatedLoopFrame } from '../../src/audio/looper/grid-math.ts';
// phaseFrames here denotes a captured wet frame relative to the grid. Punch-window inclusion is
// independently measured by overdub-window.mjs; this checks its mapping onto the master buffer.
function overdubWriteHead(phaseFrames, master, C) {
  return compensatedLoopFrame(phaseFrames, C, 0, master);
}
{
  const sr = 48000,
    bpm = 120,
    bars = 4;
  const fpb = Math.round((4 * 60 * sr) / bpm);
  const master = bars * fpb;
  const C = compFrames({ hop: Math.round(0.03 * sr), cpalOut: 0.006, outLat: 0.012, sr });

  // C = 0 (no native monitor / disabled) ⇒ write head is the raw loop phase = today's behaviour.
  for (const phase of [0, 1234, fpb, master - 1, Math.round(master / 3)]) {
    ok(`F overdub C=0 ⇒ write head unchanged (phase=${phase})`, overdubWriteHead(phase, master, 0) === phase % master);
  }
  // C > 0 ⇒ the layer is shifted EARLIER by exactly C (wrapping correctly past 0).
  ok('F overdub shifts the layer earlier by C', overdubWriteHead(10000, master, C) === 10000 - C);
  ok('F overdub wraps past the loop head', overdubWriteHead(10, master, C) === ((10 - C) % master + master) % master);
  ok('F overdub wrapped value is exactly master − (C − phase)', overdubWriteHead(10, master, C) === master - (C - 10));
  // Consistency: the overdub correction uses the SAME C magnitude as the first-track arm (one source).
  const firstTrackC = compFrames({ hop: Math.round(0.03 * sr), cpalOut: 0.006, outLat: 0.012, sr });
  ok('F overdub uses the same C as the first-track arm', firstTrackC === C);
  // Result is always a valid in-loop index.
  for (const phase of [0, 1, 999, master - 1]) {
    const wh = overdubWriteHead(phase, master, C);
    ok(`F overdub write head is a valid loop index (phase=${phase})`, wh >= 0 && wh < master, `wh=${wh}`);
  }
}

// ── Section G — WINDOW-MEDIAN snapshot stabilisation (record-latency.ts sampler / generation / freeze) ─
// The hop + click-output terms are the MEDIAN over a ~1 s rolling window, frozen at FIRST record use: C is
// identical take-to-take within one armed session, and reproducible per (device, sr, buffer) config across
// re-arms (a single draw spread raw C 73–95 ms in one rig session and forced a re-trim after every re-arm).
// The median math and the sampler cadence are pure; the session behaviour runs on the real module below.

// SAMPLE_INTERVAL_MS is imported from record-latency-math.ts (no port).
const DRAIN_INTERVAL_MS = 5;

{
  const sawtoothPhases = Array.from(
    { length: 5 },
    (_, sample) => (sample * SAMPLE_INTERVAL_MS) % DRAIN_INTERVAL_MS,
  );
  ok(
    'G five nominal 31 ms samples cover all five integer-ms phases of the 5 ms drain sawtooth',
    [...sawtoothPhases].sort((a, b) => a - b).join(',') === '0,1,2,3,4',
    `phases ${sawtoothPhases}`,
  );
}

// median() — the REAL record-latency-math.ts median over a plain array (it takes a Float64Array ring + count).
const median = (arr) => medianRing(Float64Array.from(arr), arr.length);
const mean = (arr) => (arr.length ? arr.reduce((a, b) => a + b, 0) / arr.length : 0);

// G median math — odd/even/single/all-equal + the load-bearing OUTLIER ROBUSTNESS (why median, not mean).
{
  ok('G median odd', median([3, 1, 2]) === 2);
  ok('G median even averages the two middles', median([1, 2, 3, 4]) === 2.5);
  ok('G median single sample', median([1386]) === 1386);
  ok('G median empty → 0', median([]) === 0);
  ok('G median all-equal', median([1300, 1300, 1300]) === 1300);
  // The 17 real per-arm hop draws from the 2026-07-06 rig log. Median ignores the 702 / 1674 tails; the mean
  // is dragged by them — this is precisely why the source medians the window rather than averaging it.
  const rigHop = [1355, 1174, 978, 1271, 1128, 1496, 1384, 1519, 1289, 1386, 1391, 1484, 1490, 1492, 1564, 1674, 702];
  ok('G median of the rig hop draws is the central value (1386)', median(rigHop) === 1386, `${median(rigHop)}`);
  const noTails = rigHop.filter((v) => v !== 702 && v !== 1674);
  ok('G removing the 702/1674 outliers barely moves the MEDIAN (≤ 6 frames)', Math.abs(median(rigHop) - median(noTails)) <= 6, `med ${median(rigHop)} vs ${median(noTails)}`);
  ok('G the same removal moves the MEAN much more than the median (median is the robust estimator)', Math.abs(mean(rigHop) - mean(noTails)) > Math.abs(median(rigHop) - median(noTails)), `dMean ${(mean(rigHop) - mean(noTails)).toFixed(1)} dMed ${median(rigHop) - median(noTails)}`);
}

// Deterministic LCG so the "sampler draws" are reproducible (no real RNG in a verifier).
function lcg(seed) {
  let s = seed >>> 0;
  return () => {
    s = (Math.imul(s, 1103515245) + 12345) & 0x7fffffff;
    return s / 0x7fffffff;
  };
}
// One draw of the jittery record-path state: hop swings ±300f around 1300 with a 12% chance of a 700/1700
// drain-stall/GC outlier; outLat jitters 48–66 ms with a rare 69 ms spike — the shape of the rig log.
function drawSample(rnd) {
  let hop = 1300 + (rnd() * 2 - 1) * 300;
  const r = rnd();
  if (r < 0.06) hop = 700;
  else if (r < 0.12) hop = 1700;
  let out = 0.054 + (rnd() * 2 - 1) * 0.009;
  if (rnd() < 0.05) out = 0.069;
  return [out, Math.round(hop)];
}

// The real module: every tick below is one sampler interval on the rig clock.
{
  const sr = 48000;
  const steady = (hop, cpalOut, outLat, floorEnabled = true) => compFrames({ hop, cpalOut, outLat, sr, floorEnabled });

  // Take-to-take invariance: the window keeps moving, the frozen C does not.
  const g1 = await latencySession({ sr });
  const rnd = lcg(20260706);
  g1.set(...drawSample(rnd));
  g1.latency.beginMonitorGeneration(0, 0.006);
  for (let i = 0; i < 34; i++) await g1.tick(...drawSample(rnd));
  const takes = [g1.C()];
  for (let t = 0; t < 5; t++) {
    for (let i = 0; i < 10; i++) await g1.tick(...drawSample(rnd));
    takes.push(g1.C());
  }
  ok('G C is identical across takes despite the live jitter (frozen at first use)', new Set(takes).size === 1, `takes ${takes}`);

  // Cross-re-arm reproducibility: each re-arm of the same config freezes a fresh window median.
  const g2 = await latencySession({ sr });
  const rnd2 = lcg(994001);
  const medianC = [];
  for (let arm = 0; arm < 20; arm++) {
    g2.set(...drawSample(rnd2));
    g2.latency.beginMonitorGeneration(0, 0.006);
    for (let i = 0; i < 33; i++) await g2.tick(...drawSample(rnd2));
    medianC.push(g2.C());
  }
  const spread = Math.max(...medianC) - Math.min(...medianC);
  // This synthetic is deliberately wider than the real per-config jitter; the rig lands ~±2 ms.
  ok('G cross-re-arm C spread is bounded (one trim holds per config)', spread <= Math.round(0.008 * sr), `spread ${spread}f (${(spread / sr * 1000).toFixed(1)}ms)`);

  // Freeze point: the median of the window AT the first record use; later window drift does not move it.
  const g3 = await latencySession({ sr });
  g3.set(0.012, 1200);
  g3.latency.beginMonitorGeneration(0, 0.006);
  for (let i = 0; i < 34; i++) await g3.tick(0.012, 1200);
  const cFrozen = g3.C();
  ok('G frozen C == compute on the window median at the freeze instant', cFrozen === steady(1200, 0.006, 0.012), `${cFrozen}`);
  for (let i = 0; i < 34; i++) await g3.tick(0.03, 3000);
  ok('G frozen C ignores post-freeze window drift', g3.C() === cFrozen);
  g3.latency.setOffsetMs(5);
  ok('G trim stays live atop a frozen snapshot', g3.C() === cFrozen + Math.round(0.005 * sr));
  g3.latency.setOffsetMs(0);

  // A new generation (re-arm / buffer change) clears the old window and re-opens the freeze.
  g3.set(0.02, 1800);
  g3.latency.beginMonitorGeneration(0, 0.012);
  for (let i = 0; i < 4; i++) await g3.tick(0.02, 1800);
  ok('G re-arm at a new config tracks it, with no sample from the old window', g3.C() === steady(1800, 0.012, 0.02), `${g3.C()}`);
  g3.latency.clearMonitor();
  ok('G clearMonitor returns to the C=0 baseline', g3.C() === 0);

  // Settle before the first take updates the open generation; the first take freezes the settled cpal_out.
  const g4 = await latencySession({ sr });
  g4.latency.setFloorEnabled(false);
  g4.set(0.025, 1300);
  g4.latency.beginMonitorGeneration(0, 0.006);
  for (let i = 0; i < 33; i++) await g4.tick(0.025, 1300);
  g4.latency.updateMonitorLatency(0, 0.009);
  g4.latency.updateMonitorLatency(1, 0.1); // a stale settle for another slot is ignored
  ok('G settle before the first take contributes its cpal_out', g4.C() === steady(1300, 0.009, 0.025, false), `${g4.C()}`);

  // Settle after the first take must not reopen or mutate the generation, even after the window drifted.
  const g5 = await latencySession({ sr });
  g5.latency.setFloorEnabled(false);
  g5.set(0.02, 1200);
  g5.latency.beginMonitorGeneration(0, 0.006);
  for (let i = 0; i < 33; i++) await g5.tick(0.02, 1200);
  const beforeLateSettle = g5.C();
  for (let i = 0; i < 33; i++) await g5.tick(0.04, 3000);
  g5.latency.updateMonitorLatency(0, 0.015);
  ok('G settle after the first take cannot change C', g5.C() === beforeLateSettle);
}

// ── Section D/H — UNIFORM WINDOW SHIFT on the real looper: content offset == residual Δ, loss |Δ| (NOT C) ──
// A FIXED take with code compensation C_code (a native monitor + trim) records a performance that reaches
// the record tap C_true frames after the heard downbeat. The input carries each performance frame's code
// and is silent outside the played N bars, so the committed loop shows exactly which played frames landed
// where: take index j holds performance frame p = j − Δ (Δ = C_true − C_code). Frames with p<0 are
// pre-downbeat bleed, p ≥ N·fpb post-boundary bleed. An earlier review misread a big C as a "commit
// truncation bug"; this pins the truth: the take stays N bars and loses only |Δ|, whatever C is.
{
  const sr = 48000;
  const bpm = 120;
  const bars = 2;
  const fpb = (4 * 60 * sr) / bpm;
  const L = bars * fpb;
  const code = (p) => ((p % 8192) + 1) / 16384;
  const D_SMALL = 662; // ≈ 15 ms @48k — the by-ear residual trimmed on the rig
  for (const cTarget of [1440, 4939]) {
    for (const delta of [0, D_SMALL, -D_SMALL]) {
      const rig = await bootLooper({ sampleRate: sr, startTime: 10 });
      rig.clock.setBpm(bpm);
      const latency = await rig.import('audio/record-latency.ts');
      latency.beginMonitorGeneration(0, 0);
      latency.setOffsetMs(((cTarget - QUANTUM) / sr) * 1000);
      rig.looper.setFixedLengthEnabled(true);
      rig.looper.setFixedLengthBars(bars);
      let downbeatFrame = Infinity;
      let cTrue = 0;
      rig.setInput((f) => {
        const p = f - cTrue - downbeatFrame;
        return p >= 0 && p < L ? code(p) : 0;
      });
      const mark = rig.draws().length;
      await rig.looper.recDub(0);
      const es = rig.state.engineState;
      const cCode = es.recording?.compensationFrames ?? 0;
      // The heard downbeat, from the count the clock scheduled (not from the window under test).
      downbeatFrame = Math.round((rig.draws().slice(mark).find((d) => d.countLeft === 4).time + 4 * (60 / bpm)) * sr);
      cTrue = cCode + delta;
      const tag = `C_code=${cCode} C_true=${cTrue} Δ=${delta}`;
      await rig.advanceTo((downbeatFrame + L + Math.max(cCode, cTrue)) / sr + 0.1);
      const t = rig.tracks[0];
      ok(`H precondition: the compensation is the target (${tag})`, cCode === cTarget);
      ok(`H loop length is exactly N·fpb regardless of C (${tag})`, rig.looper.masterLengthFrames() === L &&
        rig.looper.trackInfo(0).state === 'PLAYING', `master=${rig.looper.masterLengthFrames()}`);
      let shiftOk = true, headBleed = 0, tailGain = 0, present = 0;
      for (let j = 0; j < L; j++) {
        const p = j - delta;
        const want = p >= 0 && p < L ? code(p) : 0;
        if (t.record[j] !== want) shiftOk = false;
        if (p < 0) headBleed++;
        else if (p >= L) tailGain++;
        else present++;
      }
      ok(`H content is shifted by exactly Δ (p at take index p+Δ) (${tag})`, shiftOk);
      ok(`H captured played frames == L − |Δ| (loss bounded by |Δ|, not C) (${tag})`, present === L - Math.abs(delta));
      if (delta === 0) ok(`H perfect comp ⇒ the window is the played N bars exactly (${tag})`, headBleed === 0 && tailGain === 0);
      else if (delta > 0) {
        ok(`H under-comp: the take starts with Δ frames of silence before the downbeat (${tag})`,
          headBleed === delta && t.record[delta - 1] === 0 && t.record[delta] === code(0));
        ok(`H under-comp: the last Δ played frames are lost off the tail (${tag})`, t.record[L - 1] === code(L - 1 - delta));
      } else {
        ok(`H over-comp: the first |Δ| played frames are lost off the head (${tag})`, t.record[0] === code(-delta));
        ok(`H over-comp: the take ends with |Δ| frames of post-boundary silence (${tag})`,
          tailGain === -delta && t.record[L + delta - 1] === code(L - 1) && t.record[L + delta] === 0);
      }
    }
  }
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails === 0 ? 0 : 1);
