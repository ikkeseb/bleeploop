/**
 * OWNS: sampled native-monitor compensation and its first-use freeze. machine.ts applies C to
 * capture windows; record-latency-math.ts owns arithmetic; engine.ts measures limiter DSP delay.
 *
 * C = queued bridge tail's browser presentation delay - native presentation delay + graph + trim.
 * The sampler pairs queue occupancy with getOutputTimestamp's mapping of the next render frame.
 * Queue depletion and cursor advance cancel at callback boundaries. Freeze the median of these
 * PAIRED observations, not separate medians whose timing relationship has been discarded.
 *
 * Missing, stale or inconsistent timestamps use the reported base/output latency, queue medians and
 * 128-frame allowance. Its optional floor is a fallback heuristic. Driver/browser timestamp accuracy
 * still needs the physical rig; this mapping does not independently validate the DAC.
 *
 * First record use freezes the current monitor generation. Rearm, configuration changes and explicit
 * resnapshot reopen it; delayed native updates respect the freeze. Saved manual trim stays live, and so
 * does the bridge queue's smoothed shift since the freeze (`pluginBridge.queueFrames`).
 * Unarmed/disabled compensation remains zero. Native and synth inputs still share one record tap,
 * so simultaneous mixed-source alignment is not solved here.
 */
import { engine } from './engine';
import { pluginBridge } from './plugin-bridge';
import { readStoredNumber, writeStoredNumber } from './persist';

import {
  SAMPLE_CAPACITY,
  SAMPLE_INTERVAL_MS,
  WORKLET_QUANTUM_FRAMES,
  computeC,
  median,
  medianFinite,
  renderCursorTailSeconds,
} from './record-latency-math';

/** Persisted manual record trim (ms) — the Ableton "Driver Error Compensation" fallback. Default 0. */
const LS_OFFSET_KEY = 'lf.recordOffsetMs';
export const RECORD_TRIM_MAX_MS = 250;

let armedSlot: number | null = null; // the native-monitored slot whose wet the looper records, or null
let cpalOutSeconds = 0; // cached cpal_out for `armedSlot` (fetched at arm; near-constant per device/buffer)
let enabled = true; // master on/off — default ON (automatic). A/B by ear via `__lf.recordLatency`.
// Reported-mode fallback only: the optional floor assumes browser output cannot beat native output.
// It is a heuristic, not a measured bound, and is never applied to valid timestamp observations.
let floorEnabled = true;
let manualOffsetMs = readStoredNumber(
  LS_OFFSET_KEY,
  0,
  -RECORD_TRIM_MAX_MS,
  RECORD_TRIM_MAX_MS,
); // optional hidden by-ear trim added to C (persisted)

/**
 * A rolling window samples hop residency and reported output latency while a native monitor is armed.
 * The first record use freezes their medians; later takes reuse them. Freezing removes per-press
 * variation, but does not prove that these estimates equal physical delay. A new monitor generation
 * clears the window; explicit resnapshot retains it. cpalOutSeconds is generation-local and stops
 * accepting the delayed settle once frozen. Manual trim stays live.
 */
let snapBaseLatency = 0; // median ctx.baseLatency over the window, frozen at first record (render-FIFO part of clickOut)
let snapOutputLatency = 0; // median ctx.outputLatency over the window, frozen at first record (device part of clickOut)
let snapHopFrames = 0; // median pluginBridge queue residency (Rust → render worklet) over the window, frozen at first record
let snapRenderCursorSeconds: number | null = null;
let snapQueueFrames: number | null = null; // the bridge's smoothed fill at the freeze (queueShift's origin)
let snapFrozen = false; // true once a take has frozen the session's C terms; re-opened by arm/buffer-change/resnapshot

// ── Rolling sample window for the stabilised hop + click-output terms (median over ~1 s) ────────────────
// Cadence + capacity + median(): record-latency-math.ts (pure; the verifier imports them).
// Pre-allocated sample storage; the main-thread sampler writes no reactive signals.
const sampHop = new Float64Array(SAMPLE_CAPACITY);
const sampBase = new Float64Array(SAMPLE_CAPACITY);
const sampOut = new Float64Array(SAMPLE_CAPACITY);
const sampRenderCursor = new Float64Array(SAMPLE_CAPACITY);
const sortScratch = new Float64Array(SAMPLE_CAPACITY); // reused by median() so a snapshot allocates nothing per tick
let sampHead = 0; // next write index (ring)
let sampCount = 0; // valid sample count (saturates at SAMPLE_CAPACITY)
let sampler: ReturnType<typeof setInterval> | null = null;

/** Push one live sample (bridge queue residency + click-output latencies) into the rolling window. Runs only
 *  while a monitor is armed. Reads `pluginBridge.stats(slot)` (null ⇒ queue 0, matching a plugin-less slot)
 *  and `engine.ctx`. While the session is not yet frozen, refresh the working snapshot from the window so the
 *  DEV log / `__lf.snapshot` and the eventual freeze both reflect the warmed median. */
function sampleTick(): void {
  if (armedSlot === null) return;
  const ctx = engine.ctx;
  const readStart = performance.now();
  const renderBefore = ctx.currentTime;
  const stats = pluginBridge.stats(armedSlot);
  let cursor = NaN;
  try {
    if (stats && ctx.state === 'running' && typeof ctx.getOutputTimestamp === 'function') {
      const timestamp = ctx.getOutputTimestamp();
      const now = performance.now();
      // A render boundary or a stalled read can pair the queue with a different cursor. Skip it.
      if (ctx.currentTime === renderBefore && now - readStart <= 2) {
        cursor = renderCursorTailSeconds(renderBefore, timestamp.contextTime ?? NaN, timestamp.performanceTime ?? NaN,
          now, stats.queue, ctx.sampleRate);
      }
    }
  } catch { /* Unsupported/unavailable timestamps use the reported-latency fallback. */ }
  sampRenderCursor[sampHead] = cursor;
  sampHop[sampHead] = stats?.queue ?? 0;
  sampBase[sampHead] = ctx.baseLatency || 0;
  sampOut[sampHead] = ctx.outputLatency || 0;
  sampHead = (sampHead + 1) % SAMPLE_CAPACITY;
  if (sampCount < SAMPLE_CAPACITY) sampCount++;
  if (!snapFrozen) refreshSnapshotFromWindow();
}

/** Recompute the paired estimate and fallback term medians over the current window. Used while unfrozen (every
 *  sampler tick + monitor-generation updates) and once more at the freeze in `recordCompensationFrames`. */
function refreshSnapshotFromWindow(): void {
  const cursor = medianFinite(sampRenderCursor, sampCount, sortScratch);
  snapRenderCursorSeconds = Number.isFinite(cursor) ? cursor : null;
  snapBaseLatency = median(sampBase, sampCount, sortScratch);
  snapOutputLatency = median(sampOut, sampCount, sortScratch);
  snapHopFrames = median(sampHop, sampCount, sortScratch);
}

/** Start the rolling-window sampler (idempotent) + seed one immediate sample so the window is never empty. */
function startSampler(): void {
  if (sampler === null) sampler = setInterval(sampleTick, SAMPLE_INTERVAL_MS);
  sampleTick(); // seed immediately so a very-fast first record still has ≥1 sample
}

/** Stop the sampler and clear the window (monitor disarmed). */
function stopSampler(): void {
  if (sampler !== null) {
    clearInterval(sampler);
    sampler = null;
  }
  sampHead = 0;
  sampCount = 0;
}

/** DEV breakdown of the last computed compensation (for `__lf` inspection + measure-first). */
export interface CompensationBreakdown {
  source: 'timestamp' | 'reported';
  renderCursorSeconds: number | null;
  slot: number;
  sr: number;
  hopFrames: number; // the plugin bridge queue (the whole record-path bridge)
  workletFrames: number;
  cpalOutSeconds: number;
  baseLatency: number; // diagnostic/fallback ctx.baseLatency
  outputLatencyReported: number; // baseLatency + ctx.outputLatency (what Chromium/WebView2 reports)
  outputLatency: number; // diagnostic remainder after median hop terms, or the fallback output estimate
  outputGraphLatencySeconds: number; // measured DSP delay after masterGain, additional to device output latency
  outputFloored: boolean; // true when the floor lifted the reported value up to cpalOut (the floor is a heuristic)
  trimMs: number;
  cSeconds: number;
  frames: number;
}
let lastBreakdown: CompensationBreakdown | null = null;

/** Begin a new native-monitor configuration generation after arm or buffer-size change. Clears samples from
 * the previous configuration, re-opens the freeze exactly once, and seeds the new rolling window immediately. */
export function beginMonitorGeneration(slot: number, cpalOut: number): void {
  armedSlot = slot;
  cpalOutSeconds = Number.isFinite(cpalOut) ? Math.max(0, cpalOut) : 0;
  snapshotTerms(slot, true);
}

/** Apply a delayed `cpal_out` settle fetch to the current generation only while it is still unfrozen. Once
 * the first take freezes C, settle must be a no-op so every later take keeps the exact same compensation. */
export function updateMonitorLatency(slot: number, cpalOut: number): void {
  if (slot !== armedSlot || snapFrozen) return;
  cpalOutSeconds = Number.isFinite(cpalOut) ? Math.max(0, cpalOut) : 0;
  refreshSnapshotFromWindow();
  logSnapshot(slot);
}

/**
 * (Re-)open the stabilised snapshot for this monitor session. Arm / buffer change clears the old-config window;
 * manual `resnapshot` deliberately keeps the current window. Ensures the rolling-window sampler is running,
 * RE-OPENS the freeze (`snapFrozen = false`) so the next record re-freezes from the current-config window, and
 * refreshes the working snapshot from whatever samples exist. The actual FREEZE happens lazily at first record
 * use (`recordCompensationFrames`), so the C the first take sees is the warmed-window median — see the snap*
 * state note.
 * `pluginBridge.stats` is null when no plugin is wired in the slot ⇒ queue 0.
 */
function snapshotTerms(slot: number, clearWindow = false): void {
  snapFrozen = false; // arm / buffer-change / manual resnapshot re-opens; first record will re-freeze it
  if (clearWindow) stopSampler(); // a new config must not inherit samples from the previous generation
  startSampler(); // idempotent; seeds ≥1 sample so the window is never empty
  refreshSnapshotFromWindow(); // reflect the current window in the snap* terms for the DEV log / inspection
  logSnapshot(slot);
}

function logSnapshot(slot: number): void {
  if (import.meta.env.DEV) {
    // DEV observability: this line shows generation opens / eligible settle updates + the current window median.
    console.error(
      `[rec-comp] snapshot slot=${slot} (window n=${sampCount}) hop=${Math.round(snapHopFrames)}f ` +
        `base=${(snapBaseLatency * 1000).toFixed(1)}ms out=${(snapOutputLatency * 1000).toFixed(1)}ms ` +
        `cpalOut=${(cpalOutSeconds * 1000).toFixed(1)}ms`,
    );
  }
}

/**
 * The native monitor was disarmed (or its plugin unloaded). Clears compensation →
 * `recordCompensationFrames()` returns 0 (the verified synth/mic/no-monitor baseline). Pass the `slot`
 * so a stale disarm of a non-armed slot is a no-op; omit to clear unconditionally. Idempotent.
 */
export function clearMonitor(slot?: number): void {
  if (slot === undefined || slot === armedSlot) {
    armedSlot = null;
    cpalOutSeconds = 0;
    snapRenderCursorSeconds = null;
    snapBaseLatency = 0;
    snapOutputLatency = 0;
    snapHopFrames = 0;
    snapFrozen = false;
    stopSampler(); // stop the rolling-window sampler + clear the window (no monitor ⇒ nothing to estimate)
  }
}

/** Master on/off for the compensation (A/B by ear via `__lf`). Default ON. */
export function setEnabled(on: boolean): void {
  enabled = on;
}
export function isEnabled(): boolean {
  return enabled;
}

/** Toggle the output-latency floor for rig comparison. Session-only; default on. */
export function setFloorEnabled(on: boolean): void {
  floorEnabled = on;
}
export function isFloorEnabled(): boolean {
  return floorEnabled;
}

/** Set and persist the manual record trim (ms), added to C. The fallback for a mis-reporting driver. */
export function setOffsetMs(ms: number): void {
  const finite = Number.isFinite(ms) ? ms : 0;
  manualOffsetMs = Math.max(-RECORD_TRIM_MAX_MS, Math.min(RECORD_TRIM_MAX_MS, finite));
  writeStoredNumber(LS_OFFSET_KEY, manualOffsetMs);
}
export function offsetMs(): number {
  return manualOffsetMs;
}

/** Compute the shift for this take. No native monitor or disabled compensation returns zero. */
export function recordCompensationFrames(): number {
  if (!enabled || armedSlot === null) return 0; // no native monitor ⇒ no compensation (also Node/web)
  // First use freezes the samples available so far; a very fast arm-to-record can use a short window.
  if (!snapFrozen) {
    refreshSnapshotFromWindow();
    snapQueueFrames = pluginBridge.queueFrames(armedSlot);
    snapFrozen = true;
  }
  const sr = engine.ctx.sampleRate; // immutable for the context's lifetime — safe to read live
  // Reuse the frozen terms. The bridge queue is the one that moves between takes, and it is the record
  // path's delay frame for frame (loopback cable, 2026-09-24), so each take shifts the frozen cursor by
  // how far the bridge's smoothed fill has moved since the freeze (raw samples jitter too much for a
  // per-take read, which is why the window is frozen). Manual trim is read anew too.
  const liveQueue = pluginBridge.queueFrames(armedSlot);
  const queueShiftFrames = liveQueue !== null && snapQueueFrames !== null ? liveQueue - snapQueueFrames : 0;
  const queueShiftSeconds = queueShiftFrames / sr;
  // Output reports are frozen with the bridge terms. Graph DSP is measured once at engine start;
  // native monitoring bypasses that graph, so its delay is added separately from the output reports.
  const outputGraphLatencySeconds = engine.outputGraphLatencySeconds;
  const { source, hopFrames, outputLatencyReported, outputLatency, outputFloored, cSeconds, frames } = computeC(
    {
      renderCursorSeconds: snapRenderCursorSeconds === null ? null : snapRenderCursorSeconds + queueShiftSeconds,
      hopFrames: snapHopFrames + queueShiftFrames,
      cpalOutSeconds,
      baseLatency: snapBaseLatency,
      outputLatency: snapOutputLatency,
      outputGraphLatencySeconds,
      trimMs: manualOffsetMs,
      floorEnabled,
    },
    sr,
  );
  lastBreakdown = {
    source,
    renderCursorSeconds: snapRenderCursorSeconds === null ? null : snapRenderCursorSeconds + queueShiftSeconds,
    slot: armedSlot,
    sr,
    hopFrames,
    workletFrames: source === 'timestamp' ? 0 : WORKLET_QUANTUM_FRAMES,
    cpalOutSeconds,
    baseLatency: snapBaseLatency,
    outputLatencyReported,
    outputLatency,
    outputGraphLatencySeconds,
    outputFloored,
    trimMs: manualOffsetMs,
    cSeconds,
    frames,
  };
  // Measure-first: console.error reaches `tauri dev` stdout (vite forwards [console.error];
  // plain console.log does NOT) for grepping dev-asio.log. Also logged in a release build (one line per
  // take, piped to the release log) so a by-ear "slightly off" can be diagnosed without a dev build; never
  // fires on the web/synth path (armedSlot is null there), so Playwright's 0-errors check is unaffected.
  console.error(
    `[rec-comp] C=${frames}f (${(cSeconds * 1000).toFixed(1)}ms) source=${source} ` +
      `hop=${hopFrames}f cpalOut=${(cpalOutSeconds * 1000).toFixed(1)}ms ` +
      `cursor=${snapRenderCursorSeconds === null ? 'unavailable' : (snapRenderCursorSeconds * 1000).toFixed(3) + 'ms'} ` +
      `reportedOut=${(outputLatencyReported * 1000).toFixed(1)}ms ` +
      `graph=${(outputGraphLatencySeconds * 1000).toFixed(3)}ms trim=${manualOffsetMs}ms snap slot=${armedSlot} sr=${sr}`,
  );
  return frames;
}

/** DEV snapshot of the last computed compensation (for `__lf` / measurement). */
export function lastCompensation(): CompensationBreakdown | null {
  return lastBreakdown;
}

/** DEV/inspection bundle exposed on `__lf.recordLatency`. */
export const recordLatency = {
  beginMonitorGeneration,
  updateMonitorLatency,
  clearMonitor,
  setEnabled,
  isEnabled,
  setFloorEnabled,
  isFloorEnabled,
  setOffsetMs,
  offsetMs,
  recordCompensationFrames,
  lastCompensation,
  /** The armed native-monitor slot (or null) — for inspection. */
  armedSlot: () => armedSlot,
  cpalOutSeconds: () => cpalOutSeconds,
  /** The window-median click-output + hop snapshot C is computed from (ms / frames) + freeze state / window
   *  fill — for measure-first inspection. While unfrozen these track the live median; frozen once a take
   *  committed them. */
  snapshot: () => ({
    renderCursorSeconds: snapRenderCursorSeconds,
    timingSource: snapRenderCursorSeconds === null ? 'reported' : 'timestamp',
    baseLatencyMs: snapBaseLatency * 1000,
    outputLatencyMs: snapOutputLatency * 1000,
    hopFrames: snapHopFrames,
    frozen: snapFrozen,
    windowSamples: sampCount,
  }),
  /** Re-open the estimate + refresh from the current window NOW (manual A/B / refresh). No-op if no monitor
   *  is armed. Re-opens the freeze so the next record re-freezes from the freshest window — lets the rig
   *  recapture the current moment without re-arming. */
  resnapshot: () => {
    if (armedSlot !== null) snapshotTerms(armedSlot);
  },
};
