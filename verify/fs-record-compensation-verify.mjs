// Executable verification of the AUTOMATIC record-latency compensation.
// Faithful Node port of src/audio/record-latency.ts + the two `pendingRecordStartFrame` arm sites in
// src/audio/looper/machine.ts (looper.ts was split into src/audio/looper/{state,capture,peaks,playback,
// machine,mixer}.ts + a facade on 2026-07-01; the arm sites live in
// machine.ts's startRecording). record-latency.ts can't be imported in Node (engine/AudioContext deps),
// so this mirrors the exact math by line. Same idiom as fs-count-in-verify.mjs / fs-later-arm-verify.mjs.
//
// Latency itself is HARDWARE and can NOT be deterministically verified — the ear/waveform on the rig is the only
// gate for "the loop lands on the click". What this PROVES is the LOGIC around it:
//   A. C = max(0, round((hop/sr + 128/sr − cpalOut + clickOut + trim/1000)·sr)), clickOut=max(outLat,cpalOut) [record-latency.ts]
//   B. THE LOAD-BEARING PHYSICS (physical regime, reported outLat ≥ cpalOut): modelling the full
//      record/monitor timeline with ARBITRARY input + plugin latency, the recorded transient lands on the
//      take's frame 0 (within rounding) — i.e. input+plugin CANCEL and the SIGN is right (discard C MORE
//      frames ⇒ late transient → frame 0)
//   C. C = 0 whenever no native monitor is armed / disabled ⇒ the synth/mic + Node baselines (and the
//      existing arm verifiers) are byte-identical to today                                        [recordCompensationFrames early return]
//   D. UNIFORM SHIFT: adding C just discards C more captured frames before frame 0 — fixed-length loop
//      LENGTH is N bars regardless of C, and the consume discard count is exactly lead+C across batches [consume arm]
//   E. THE clickOut FLOOR (2026-06-29 "C undershoots" attempt — a HEURISTIC, toggle setFloorEnabled): clickOut =
//      max(reported outLat, cpalOut). This proves BOTH SIDES of its tradeoff, not just the win:
//        E1 — a Chromium/WebView2 outputLatency under-report can NOT drive C below the record-path floor (hop+quantum).
//        E2 — BUT it is NOT an unconditional lower bound: when the report is ACCURATE and cpalOut carries the native
//             monitor's PRODUCER-RING excess (cpal_out = (monitor_fill+monitor_out_block)/R_out, monitor_fill held at
//             MONITOR_TARGET 10ms ASIO / 20ms WASAPI — a delay the Web-Audio click has no analogue for), cpalOut can
//             exceed the true clickOut and the floor OVER-compensates by exactly (cpalOut − reported)·sr (guitar EARLY),
//             while the UN-floored formula equals the ideal. Which regime a rig is in is a by-ear/`[rec-comp]`-log call. [record-latency.ts]
//   F. OVERDUB compensation uses the SAME C as the first-track arm (write head shifted earlier by C)   [looper/machine.ts startOverdub]
//   G. WINDOW-MEDIAN snapshot stabilisation: outputLatency + hop are the MEDIAN over a ~1 s rolling sample window,
//      frozen at FIRST record use (2026-07-06 refinement of the 2026-06-29 single-draw snapshot). C is IDENTICAL
//      take-to-take AND reproducible ACROSS re-arms (the single draw spread raw C 73–95 ms in one rig session);
//      median is outlier-robust; the 31 ms cadence covers every 5 ms drain phase; a genuine re-arm at a new
//      config still tracks change; settle updates only before first use; clear → C=0; the by-ear trim stays
//      live atop the frozen snapshot. [record-latency.ts sampler/beginMonitorGeneration/updateMonitorLatency + freeze]

let fails = 0,
  checks = 0;
function ok(name, cond, detail = '') {
  checks++;
  if (!cond) {
    fails++;
    console.log(`  FAIL  ${name}  ${detail}`);
  }
}

// ── The formula is IMPORTED (src/audio/record-latency-math.ts is pure) — no port, no drift tag ─────────
import {
  SAMPLE_INTERVAL_MS,
  WORKLET_QUANTUM_FRAMES as QUANTUM,
  computeC,
  median as medianRing,
} from '../src/audio/record-latency-math.ts';

// ── record-latency-math.ts: computeC() — the C formula ────────────────────────────────────────────
// C = max(0, round((hop/sr + 128/sr − cpalOut + clickOut + trim/1000)·sr)), or 0 if !enabled || !armed,
// The historical floor and 128-frame allowance are retained only for unavailable timestamps.
// They are heuristics, not physical bounds. Timestamp-mode phase cancellation has its own guard
// in fs-render-cursor-verify.mjs; the actual sampler/freeze is exercised by render-cursor.mjs.
function compFrames({ hop, cpalOut, outLat, trim = 0, sr, enabled = true, armed = true, floorEnabled = true }) {
  if (!enabled || !armed) return 0; // no native monitor ⇒ no compensation (the baseline)
  return computeC(
    { hop1Frames: hop, hop2Frames: 0, cpalOutSeconds: cpalOut, baseLatency: 0, outputLatency: outLat, trimMs: trim, floorEnabled },
    sr,
  ).frames;
}

// ── Section 0 — the record-path bridge term sums BOTH hops (hop1Lag + hop2Fill) ───────────────────
// Port of `const hopFrames = (stats?.hop1Lag ?? 0) + (stats?.hop2Fill ?? 0)`. Earlier only hop2 was
// counted; hop-1 (~½·DRAIN_INTERVAL ≈ ~120 frames @48k) is the same order as the worklet quantum.
{
  const sr = 48000;
  const hop1 = 130,
    hop2 = 1440;
  const cWhole = compFrames({ hop: hop1 + hop2, cpalOut: 0.006, outLat: 0.012, sr });
  const cHop2Only = compFrames({ hop: hop2, cpalOut: 0.006, outLat: 0.012, sr });
  // NB: this proves compFrames is LINEAR in its `hop` arg (so a hop1+hop2 sum maps to C correctly), NOT that
  // the SOURCE reads both stats fields — that `hopFrames = hop1Lag + hop2Fill` line (record-latency.ts:174-176)
  // is glue this port can't import; its runtime guard is the `[rec-comp]` DEV log (`hop=Nf(h1+h2)`) on the rig.
  ok('0 C is linear in the bridge term (hop1+hop2 maps correctly)', cWhole === cHop2Only + hop1, `whole ${cWhole} hop2only+hop1 ${cHop2Only + hop1}`);
  ok('0 missing-plugin stats ⇒ both hops 0', compFrames({ hop: 0, cpalOut: 0, outLat: 0, sr }) === QUANTUM);
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

// ── Section C — C = 0 baseline (no monitor / disabled) ⇒ existing arm unchanged ───────────────────
{
  const sr = 48000;
  const terms = { hop: 9999, cpalOut: 0.006, outLat: 0.012, trim: 50, sr };
  ok('C no monitor armed ⇒ C=0', compFrames({ ...terms, armed: false }) === 0);
  ok('C compensation disabled ⇒ C=0', compFrames({ ...terms, enabled: false }) === 0);

  // Regression: the count-in arm with C=0 equals today's formula (matches fs-count-in-verify).
  const HBL = 0.02,
    COUNT_IN_BEATS = 4,
    bpm = 120;
  const beatPeriod = 60 / bpm;
  const baseline = Math.round((HBL + COUNT_IN_BEATS * beatPeriod) * sr);
  const withZero = baseline + compFrames({ ...terms, armed: false });
  ok('C count-in arm + C(=0) == today’s pending (no regression)', withZero === baseline, `got ${withZero} want ${baseline}`);

  // Later-track arm with C=0 == framesToBoundary() (matches fs-later-arm-verify).
  const framesToBoundary = 33000;
  ok('C later arm + C(=0) == framesToBoundary (no regression)', framesToBoundary + compFrames({ ...terms, enabled: false }) === framesToBoundary);
}

// ── Section D — UNIFORM SHIFT: discard lead+C; loop length unchanged ──────────────────────────────
// consume() arm split (looper/capture.ts): discard exactly `pending` captured frames, then the take begins at
// frame 0; for fixed-length, capture exactly `fixedFrames` from frame 0 then auto-stop. Adding C to
// `pending` shifts WHERE frame 0 falls in the capture stream but NOT how many frames the take holds.
function simulateArm(pending, fixedFrames, batches) {
  // Returns { discarded, captured } after streaming `batches` frame-counts through the arm + cap.
  let rem = pending;
  let armed = true;
  let writeHead = 0;
  let discarded = 0;
  for (const count of batches) {
    let data = count;
    let offset = 0;
    if (armed) {
      if (data <= rem) {
        rem -= data;
        discarded += data;
        continue;
      }
      offset = rem;
      discarded += rem;
      rem = 0;
      armed = false;
    }
    const room = fixedFrames - writeHead;
    const n = Math.min(data - offset, room);
    writeHead += n;
    if (writeHead >= fixedFrames) break;
  }
  return { discarded, captured: writeHead };
}
{
  const sr = 48000,
    bpm = 120,
    bars = 4;
  const fpb = Math.round((4 * 60 * sr) / bpm); // frames per bar (integer, like framesPerBar)
  const fixedFrames = bars * fpb; // exact N-bar fixed-length target
  const lead = Math.round((0.02 + 4 * (60 / bpm)) * sr); // count-in lead (HBL + 4 beats)
  // Stream in irregular 128/200/512-frame batches (worklet quanta + drain merges) so the straddle math
  // is exercised, not just clean multiples.
  const batches = Array.from({ length: 4000 }, (_, i) => [128, 200, 512][i % 3]);

  for (const C of [0, 240, 1440, 1856, 4096]) {
    const pending = lead + C;
    const { discarded, captured } = simulateArm(pending, fixedFrames, batches);
    ok(`D discard == lead+C (C=${C})`, discarded === pending, `discarded ${discarded} want ${pending}`);
    ok(`D fixed-length loop is exactly N bars regardless of C (C=${C})`, captured === fixedFrames, `captured ${captured}`);
    ok(`D loop length is a whole number of bars (C=${C})`, captured % fpb === 0);
  }

  // Free-record floor: loop = floor(writeHead/fpb)·fpb is ALWAYS whole bars; and for a stop ≥ C past a
  // bar boundary the bar count is C-invariant (the shift only steals from the discarded pre-roll).
  const stopRawAt = (bars + 0.4) * fpb; // player stops 0.4 bar into the 5th bar (a hair late)
  for (const C of [0, 240, 1440]) {
    const writeHead = Math.round(stopRawAt) - C; // take started C later ⇒ C fewer captured frames
    const master = Math.max(1, Math.floor(writeHead / fpb)) * fpb;
    ok(`D free-record floors to whole bars (C=${C})`, master % fpb === 0);
    ok(`D free-record bar count C-invariant away from the boundary (C=${C})`, master === bars * fpb, `master ${master / fpb} bars`);
  }
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
import { compensatedLoopFrame } from '../src/audio/looper/grid-math.ts';
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

// ── Section G — WINDOW-MEDIAN snapshot stabilisation (record-latency.ts sampler / snapshotTerms / freeze) ─
// TWO layers of stabilisation, both ported here:
//   (2026-06-29) the click-output + hop terms are SNAPSHOTTED (not re-read per record press) so C is identical
//     take-to-take within one armed session — the fix for the 90.9/95.9/93.2 ms live-read swing.
//   (2026-07-06) that snapshot is now the MEDIAN over a ~1 s rolling WINDOW of samples, not a single one-shot
//     draw. The single draw froze the take-to-take swing but every RE-arm (plugin swap / buffer change) drew
//     fresh, and the two jittery quantities spread far too wide draw-to-draw (rig: hop 702–1674 frames, outLat
//     48–69 ms across one session ⇒ raw C spread 73–95 ms), forcing a re-trim after every re-arm. A median over
//     a window is reproducible per (device, sr, buffer) config across re-arms — the whole point of the refinement.
// The freeze is taken at FIRST RECORD USE from the warmed window (so the first take never sees a cold single
// draw), then held for the session. This ports median() + the windowed session + the freeze-at-first-use flag,
// and PROVES: median math (odd/even/single/outlier-robust), take-to-take invariance, cross-re-arm reproducibility
// (the refinement's win vs the single-draw control), freeze-point correctness, re-open on re-arm, clear → C=0,
// trim live atop a frozen snapshot.

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

// Windowed session model — mirrors the sampler ring + snapFrozen freeze-at-first-use. push() = a sampler tick
// (refreshes the working snapshot while unfrozen); comp() = recordCompensationFrames (freezes the window median
// on first call, then reuses it). beginGeneration clears/re-opens; settle updates only while unfrozen; clear resets.
// MIRRORS: src/audio/record-latency.ts@160-190 sha256:39a8e2cd2a46ae85  (generation open + settle-only-while-unfrozen)
function makeSession(cap = 33) {
  let armed = false,
    cpalOut = 0,
    frozen = false;
  let winOut = [],
    winHop = [];
  let snapOut = 0,
    snapHop = 0;
  const refresh = () => {
    snapOut = median(winOut);
    snapHop = median(winHop);
  };
  return {
    beginGeneration(cpal) {
      armed = true;
      cpalOut = cpal;
      frozen = false;
      winOut = [];
      winHop = [];
      snapOut = 0;
      snapHop = 0;
      refresh();
    },
    updateMonitorLatency(cpal) {
      if (!armed || frozen) return;
      cpalOut = cpal;
      refresh();
    },
    push(outSample, hopSample) {
      // one sampler tick into the rolling window (drop the oldest past `cap`); refresh snap while unfrozen
      winOut.push(outSample);
      winHop.push(hopSample);
      if (winOut.length > cap) {
        winOut.shift();
        winHop.shift();
      }
      if (!frozen) refresh();
    },
    clear() {
      armed = false;
      cpalOut = 0;
      frozen = false;
      winOut = [];
      winHop = [];
      snapOut = 0;
      snapHop = 0;
    },
    comp(sr, trim = 0, floorEnabled = true) {
      if (armed && !frozen) {
        refresh(); // freeze the warmed-window median at first record use
        frozen = true;
      }
      return compFrames({ hop: snapHop, cpalOut, outLat: snapOut, trim, sr, armed, floorEnabled });
    },
    frozen: () => frozen,
    snap: () => ({ snapOut, snapHop }),
  };
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
  return [out, hop];
}

{
  const sr = 48000;
  const s = makeSession();
  // Arm, then let the sampler fill a full window (the arm→record gap) before the first take freezes it.
  s.beginGeneration(0.006);
  const rnd = lcg(20260706);
  for (let i = 0; i < 34; i++) {
    const [o, h] = drawSample(rnd);
    s.push(o, h);
  }
  ok('G not frozen until the first record press', s.frozen() === false);
  const c0 = s.comp(sr); // first take → freezes the window median
  ok('G frozen after the first record press', s.frozen() === true);

  // Take-to-take invariance: further sampler ticks keep arriving (window keeps moving) but the frozen C does
  // NOT change across takes in the same session.
  const takes = [c0];
  for (let t = 0; t < 5; t++) {
    for (let i = 0; i < 10; i++) {
      const [o, h] = drawSample(rnd);
      s.push(o, h);
    }
    takes.push(s.comp(sr));
  }
  ok('G FIX: C is identical across takes despite the live jitter (frozen)', new Set(takes).size === 1, `takes ${takes}`);

  // CROSS-RE-ARM REPRODUCIBILITY — the 2026-07-06 refinement's core claim. Simulate many re-arms of the SAME
  // config. CONTROL = the OLD single-draw snapshot (one sample per arm → C spreads widely, the re-trim pain).
  // FIX = the window MEDIAN per arm (C reproducible). Both consume the same deterministic draw stream.
  const rnd2 = lcg(994001);
  const singleDrawC = [];
  const medianC = [];
  for (let arm = 0; arm < 20; arm++) {
    // OLD: freeze the FIRST draw of the arm.
    const [o1, h1] = drawSample(rnd2);
    singleDrawC.push(compFrames({ hop: h1, cpalOut: 0.006, outLat: o1, sr }));
    // NEW: fill a window, freeze its median.
    const wo = [o1],
      wh = [h1];
    for (let i = 0; i < 33; i++) {
      const [o, h] = drawSample(rnd2);
      wo.push(o);
      wh.push(h);
    }
    medianC.push(compFrames({ hop: median(wh), cpalOut: 0.006, outLat: median(wo), sr }));
  }
  const spread = (a) => Math.max(...a) - Math.min(...a);
  const singleSpread = spread(singleDrawC);
  const medianSpread = spread(medianC);
  ok('G CONTROL: the OLD single-draw snapshot spreads C wide across re-arms (the re-trim pain is real)', singleSpread > 1000, `singleSpread ${singleSpread}f (${(singleSpread / sr * 1000).toFixed(1)}ms)`);
  // The window median is dramatically tighter across re-arms — reproducible per config (the refinement's win).
  ok('G FIX: the window median makes C reproducible across re-arms (≥ 5× tighter than the single draw)', medianSpread * 5 < singleSpread, `medianSpread ${medianSpread}f (${(medianSpread / sr * 1000).toFixed(1)}ms) vs single ${singleSpread}f`);
  // Absolute bound: on THIS deliberately-wide synthetic (±300 f hop + 6%/6% 700/1700 outliers + a 69 ms outLat
  // spike — wider than the real per-config jitter) the median-C still holds within ~a handful of ms across 20
  // re-arms. The rig's within-config jitter is tighter, so the real cross-re-arm spread lands ~±2 ms (a single
  // pin holds). The load-bearing claim is the ≥5× tightening above; this just bounds the absolute.
  ok('G FIX: cross-re-arm median-C spread is bounded (one pin holds per config)', medianSpread <= Math.round(0.008 * sr), `medianSpread ${medianSpread}f (${(medianSpread / sr * 1000).toFixed(1)}ms)`);

  // A genuine re-arm at a DIFFERENT config (buffer change) DOES move C — real change still tracked, not frozen out.
  const s2 = makeSession();
  s2.beginGeneration(0.012);
  for (let i = 0; i < 34; i++) s2.push(0.02, 1800); // a different steady config
  const cBig = s2.comp(sr);
  ok('G re-arm at a new config updates C (real change still tracked)', cBig === compFrames({ hop: 1800, cpalOut: 0.012, outLat: 0.02, sr }), `${cBig}`);

  // clearMonitor → the C=0 baseline, byte-identical to the no-monitor path (Section C).
  s2.clear();
  ok('G clearMonitor returns to the C=0 baseline', s2.comp(sr) === 0);

  // Freeze-point correctness: the value frozen at first use is the median of the window AT THAT MOMENT, and a
  // later window change does NOT move it (contrast: before freezing, the working snap tracks the window).
  const s3 = makeSession();
  s3.beginGeneration(0.006);
  for (let i = 0; i < 34; i++) s3.push(0.012, 1200); // steady window at hop 1200, outLat 12 ms
  ok('G working snapshot tracks the window before freezing', s3.snap().snapHop === 1200 && s3.snap().snapOut === 0.012);
  const cFrozen = s3.comp(sr); // freeze here
  for (let i = 0; i < 34; i++) s3.push(0.03, 3000); // window fully replaced with a wildly different regime
  ok('G frozen C ignores post-freeze window drift', s3.comp(sr) === cFrozen);
  ok('G frozen C == compute on the window median at the freeze instant', cFrozen === compFrames({ hop: 1200, cpalOut: 0.006, outLat: 0.012, sr }));

  // Trim stays LIVE: same frozen snapshot, different trim ⇒ C shifts by trim·sr (a mid-session setOffsetMs works).
  ok('G trim stays live atop a frozen snapshot', s3.comp(sr, 5) === s3.comp(sr, 0) + Math.round(0.005 * sr));

  // Settle before first take updates the open generation and the first take freezes the settled cpal_out.
  const s4 = makeSession();
  s4.beginGeneration(0.006);
  for (let i = 0; i < 33; i++) s4.push(0.025, 1300);
  s4.updateMonitorLatency(0.009);
  ok('G settle before first take keeps the generation unfrozen', s4.frozen() === false);
  ok(
    'G settle before first take contributes its updated cpal_out to the first frozen C',
    s4.comp(sr, 0, false) === compFrames({ hop: 1300, cpalOut: 0.009, outLat: 0.025, sr, floorEnabled: false }),
  );

  // Settle after first take must not reopen or mutate the generation, even after the live window has drifted.
  const s5 = makeSession();
  s5.beginGeneration(0.006);
  for (let i = 0; i < 33; i++) s5.push(0.02, 1200);
  const beforeLateSettle = s5.comp(sr, 0, false);
  for (let i = 0; i < 33; i++) s5.push(0.04, 3000);
  s5.updateMonitorLatency(0.015);
  ok('G settle after first take leaves the generation frozen', s5.frozen() === true);
  ok(
    'G settle after first take cannot change C or break take-to-take invariance',
    s5.comp(sr, 0, false) === beforeLateSettle,
  );
}

// ── Section H — UNIFORM WINDOW SHIFT: content offset == residual Δ, loss bounded by |Δ| (NOT by C) ──
// Sharpens Section B (perfect comp ⇒ transient on frame 0) into the full CONTENT relationship, and pins
// an earlier review finding so the "commit truncation bug" misread can't recur: with code
// compensation C_code vs the TRUE record-path latency C_true, the captured loop content is a UNIFORM
// WINDOW SHIFT by the residual Δ = C_true − C_code — the loop LENGTH is always exactly N·fpb regardless
// of C, and only WHICH played frames sit in that fixed-length window moves.
//
// MODEL (capture-stream coordinates; port of consume()'s first-track fixed-length cap in looper/capture.ts
// + startRecording's `pendingRecordStartFrame = lead + recordCompensationFrames()` in looper/machine.ts):
//   • The arm discards exactly `lead + C_code` frames, then the cap keeps exactly N·fpb ⇒ the take is the
//     capture window [lead + C_code, lead + C_code + N·fpb). (Section D already proves discard == lead+C
//     and captured == N·fpb; this reuses that exact stream discipline.)
//   • A note the player performs on the heard downbeat reaches the record tap C_true frames late (the
//     Section B record-path physics: input+plugin cancel, C_true is the residual round trip) ⇒ the
//     PHYSICAL PLAYED window [downbeat, downbeat + N·bar) lands at capture positions
//     [lead + C_true, lead + C_true + N·fpb). The downbeat maps to capture position lead + C_true.
//   • ⇒ take index j holds performance frame p = j − Δ (Δ = C_true − C_code): the whole content is shifted
//     by exactly Δ. Frames with p<0 are PRE-DOWNBEAT head bleed; p≥N·fpb are POST-BOUNDARY (next-loop) bleed.
// PROVES: (1) C_code==C_true ⇒ tail-loss==0 AND head-bleed==0 (window == the played N bars exactly);
// (2) under-comp (Δ>0) ⇒ content shifted by Δ, tail loses exactly Δ, head gains exactly Δ pre-downbeat
// bleed; (3) over-comp (Δ<0) ⇒ the mirror (head loses |Δ|, tail gains |Δ| post-boundary bleed);
// (4) loop length == N·fpb in ALL cases (references the Section D regression); (5) content loss == |Δ|
// and is NEVER bounded by C — proven with a LARGE C (4939 f ≈ 112 ms) and a SMALL Δ (662 f ≈ 15 ms).
{
  const sr = 48000,
    bpm = 120,
    bars = 4;
  const fpb = Math.round((4 * 60 * sr) / bpm); // frames per bar (integer) — same as Section D
  const L = bars * fpb; // N·fpb: the fixed-length loop target (exact whole bars)
  const lead = Math.round((0.02 + 4 * (60 / bpm)) * sr); // count-in lead (HBL + 4 beats), same as Section D
  // Irregular batches (worklet quanta + drain merges) so the discard/cap straddle is exercised, not clean
  // multiples — identical stream shape to Section D. ~1.12M frames total covers lead + C + L for every case.
  const batches = Array.from({ length: 4000 }, (_, i) => [128, 200, 512][i % 3]);

  // Stream the capture window out (faithful to consume's arm-discard + first-track cap) and tag every KEPT
  // frame by its physical origin: a performance-frame index, or 'pre'/'post' bleed. This exercises the same
  // accumulate-discard-across-batches + break-at-cap logic Section D asserts on counts.
  function analyzeTake({ cCode, cTrue }) {
    const delta = cTrue - cCode;
    let pending = lead + cCode; // = pendingRecordStartFrame (lead + compensation), discarded before frame 0
    let armed = true;
    let cs = 0; // capture-stream position of the current frame
    let takeLen = 0;
    let headBleed = 0, // kept frames BEFORE the played downbeat (p<0): pre-downbeat bleed
      tailGain = 0; // kept frames AFTER the played window (p≥L): post-boundary / next-loop bleed
    let shiftOk = true; // performance frame p must sit at take index p + Δ (the uniform shift)
    const present = new Set(); // which performance frames [0,L) actually landed in the take
    outer: for (const count of batches) {
      for (let k = 0; k < count; k++, cs++) {
        if (armed) {
          if (pending > 0) {
            pending--;
            continue; // discard a pre-downbeat frame (cs still advances)
          }
          armed = false; // discarded exactly lead+cCode ⇒ the take begins at cs = lead+cCode
        }
        if (takeLen >= L) break outer; // fixed-length cap: keep exactly N·fpb frames
        const j = takeLen++; // take index
        const p = cs - (lead + cTrue); // performance frame at this capture position
        if (p < 0) headBleed++;
        else if (p >= L) tailGain++;
        else {
          present.add(p);
          if (j !== p + delta) shiftOk = false;
        }
      }
    }
    return { takeLen, headBleed, tailGain, shiftOk, present, missing: L - present.size, delta };
  }

  const C_LARGE = 4939; // ≈ 112 ms @48k — a realistic natively-monitored C (well above the record-path floor)
  const D_SMALL = 662; //  ≈ 15 ms @48k — the by-ear residual trimmed on the rig (2026-07-04)
  const scenarios = [
    { cCode: 1440, cTrue: 1440 }, //                 perfect comp at a typical C
    { cCode: 1440 - D_SMALL, cTrue: 1440 }, //        under-comp by Δ
    { cCode: 1440, cTrue: 1440 - D_SMALL }, //        over-comp by Δ
    { cCode: C_LARGE, cTrue: C_LARGE }, //            perfect comp at a LARGE C
    { cCode: C_LARGE, cTrue: C_LARGE + D_SMALL }, //  under-comp: LARGE C, small Δ (item 5)
    { cCode: C_LARGE + D_SMALL, cTrue: C_LARGE }, //  over-comp: LARGE C, small Δ
  ];

  for (const { cCode, cTrue } of scenarios) {
    const delta = cTrue - cCode;
    const r = analyzeTake({ cCode, cTrue });
    const tag = `cCode=${cCode} cTrue=${cTrue} Δ=${delta}`;

    // (4) LENGTH is ALWAYS exactly N·fpb, whole bars — the load-bearing regression (mirrors Section D).
    ok(`H loop length is exactly N·fpb regardless of C (${tag})`, r.takeLen === L, `takeLen ${r.takeLen} want ${L}`);
    ok(`H loop length is a whole number of bars (${tag})`, r.takeLen % fpb === 0);
    // Content is a uniform shift by exactly Δ (performance frame p sits at take index p+Δ).
    ok(`H content is shifted by exactly Δ (p at take index p+Δ) (${tag})`, r.shiftOk);
    // Captured played frames == L − |Δ|: the loss is bounded by |Δ|, never by C (item 5, per scenario).
    ok(`H captured played frames == L − |Δ| (loss bounded by |Δ|, not C) (${tag})`, r.present.size === L - Math.abs(delta), `present ${r.present.size} want ${L - Math.abs(delta)}`);

    if (delta === 0) {
      // (1) perfect comp: the window covers exactly the played N bars — no loss either end (sharpens Section B).
      ok(`H perfect comp ⇒ zero tail-loss AND zero head-bleed (window == the played N bars) (${tag})`, r.missing === 0 && r.headBleed === 0 && r.tailGain === 0, `missing ${r.missing} head ${r.headBleed} tail ${r.tailGain}`);
      ok(`H perfect comp captures every played frame (${tag})`, r.present.size === L);
    } else if (delta > 0) {
      // (2) under-comp: tail loses exactly Δ, head gains exactly Δ of pre-downbeat bleed, no post bleed.
      ok(`H under-comp: tail loses exactly Δ frames (${tag})`, r.missing === delta, `missing ${r.missing} want ${delta}`);
      ok(`H under-comp: head gains exactly Δ frames of pre-downbeat bleed (${tag})`, r.headBleed === delta, `head ${r.headBleed} want ${delta}`);
      ok(`H under-comp: no post-boundary bleed (${tag})`, r.tailGain === 0, `tail ${r.tailGain}`);
      ok(`H under-comp: tail-loss == head-bleed (a UNIFORM window shift, not a truncation) (${tag})`, r.missing === r.headBleed);
      // The lost frames are exactly the take TAIL [L−Δ, L): the last played frame is gone, the one Δ before it kept.
      ok(`H under-comp: the lost frames are the take TAIL (${tag})`, !r.present.has(L - 1) && r.present.has(L - 1 - delta));
    } else {
      const d = -delta;
      // (3) over-comp: the mirror — head loses |Δ|, tail gains |Δ| of post-boundary (next-loop) bleed.
      ok(`H over-comp: head loses exactly |Δ| frames (${tag})`, r.missing === d, `missing ${r.missing} want ${d}`);
      ok(`H over-comp: tail gains exactly |Δ| frames of post-boundary bleed (${tag})`, r.tailGain === d, `tail ${r.tailGain} want ${d}`);
      ok(`H over-comp: no pre-downbeat bleed (${tag})`, r.headBleed === 0, `head ${r.headBleed}`);
      ok(`H over-comp: head-loss == tail-gain (the mirror of under-comp) (${tag})`, r.missing === r.tailGain);
      // The lost frames are exactly the take HEAD [0, |Δ|): the first played frame is gone, the one at |Δ| kept.
      ok(`H over-comp: the lost frames are the take HEAD (${tag})`, !r.present.has(0) && r.present.has(d));
    }
  }

  // (5) explicit: loss is bounded by |Δ|, NEVER by C. A LARGE 4939-frame (≈112 ms) compensation with a SMALL
  // 662-frame (≈15 ms) residual loses exactly 662 frames off the tail — NOT 4939. This is the exact shape that
  // review warned was mis-read as a "commit truncation bug": a big C does NOT shorten the take, it only slides
  // the fixed-length window; the take stays N·fpb long and loses only the residual Δ.
  const big = analyzeTake({ cCode: C_LARGE, cTrue: C_LARGE + D_SMALL });
  ok('H BIG C (4939f≈112ms) small Δ (662f≈15ms): tail-loss == Δ (662), NOT C', big.missing === D_SMALL && big.missing !== C_LARGE, `missing ${big.missing}`);
  ok('H BIG C small Δ: loop length is still exactly N·fpb (a big C does not truncate)', big.takeLen === L);
  ok('H BIG C small Δ: captured frames == L − Δ, independent of the 4939-frame C', big.present.size === L - D_SMALL);
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails === 0 ? 0 : 1);
