/**
 * OWNS: the record-latency compensation formula `C` and the window-median that stabilises its terms.
 *
 * C (frames) is how much later a natively monitored performance reaches the looper's record tap than the
 * player heard it; capture windows shift by C. With valid output timestamps:
 *   C = median paired browser queue-tail presentation delay − nativeOut + measured graph delay (limiter)
 *       + trim, clamped at zero,
 * where sampling keeps queue occupancy and render-cursor time together. Without timestamps:
 *   C = (hop1 + hop2 + 128) / sr − nativeOut + clickOut + graph delay + trim.
 * nativeOut = monitor-ring residency + the median valid callback-to-playback report (the callback period
 * at startup or when unsupported). C is 0 unless a native monitor is armed: native monitoring cancels
 * input + plugin latency, so C compensates the record path only. The first record use freezes the median
 * per monitor generation; re-arm, a buffer change and resnapshot reopen it (`record-latency.ts`).
 *
 * PURE ON PURPOSE: no engine / bridge import, so `verify/guards/record-compensation.mjs` IMPORTS this
 * instead of mirroring it (see verify/README.md). `record-latency.ts` owns the live terms (sampler, freeze,
 * arm state) and calls in here.
 */

/** Existing 128-frame bridge/render alignment heuristic, not a measured worklet-to-worklet DSP delay. */
export const WORKLET_QUANTUM_FRAMES = 128;

/** Sampler tick (ms). Deliberately incommensurate with the 5 ms drain phase (hop-1's oscillation period):
 *  nominal samples advance by 1 ms through all five phases instead of repeatedly observing one phase. */
export const SAMPLE_INTERVAL_MS = 31;
/** Rolling-window span (ms). ~1 s of samples: long enough to average out the hop-1 drain-phase swing + the
 *  hop-2 PI hunt + the `outputLatency` jitter, short enough to still reflect the current buffer config. */
export const SAMPLE_WINDOW_MS = 1000;
export const SAMPLE_CAPACITY = Math.max(1, Math.ceil(SAMPLE_WINDOW_MS / SAMPLE_INTERVAL_MS)); // ~33 samples

/** Median of the first `count` valid entries of a sample ring. Numeric (typed-array sort is numeric, unlike
 *  Array.sort). 0 for an empty window. Even counts average the two middles. Robust to the fat-tail outlier
 *  draws (702/1674-frame hop, 48/69 ms outputLatency) that a mean would let drag the estimate. `scratch`
 *  (≥ count long) keeps a continuous timer alloc-free; omitted ⇒ a fresh copy. */
export function median(buf: Float64Array, count: number, scratch?: Float64Array): number {
  if (count <= 0) return 0;
  const view = scratch ? scratch.subarray(0, count) : new Float64Array(count);
  view.set(buf.subarray(0, count)); // valid entries live at 0..count-1 (ring wrap is irrelevant to a median)
  view.sort();
  const mid = count >> 1;
  return count % 2 ? view[mid] : (view[mid - 1] + view[mid]) / 2;
}

/** Median of paired timing samples. Invalid/missing observations occupy their window slot so old
 * valid readings expire normally. Require several observations before trusting a new clock. */
export function medianFinite(buf: Float64Array, count: number, scratch: Float64Array): number {
  let valid = 0;
  for (let i = 0; i < count; i++) if (Number.isFinite(buf[i])) scratch[valid++] = buf[i];
  return valid >= 3 ? median(scratch, valid, scratch) : NaN;
}

/** Time from the current wall instant until the queued bridge tail would reach browser output.
 * This pairs queue occupancy with the next render frame's presentation time. A callback consumes
 * queued frames while advancing that cursor; their sum must stay stable. This is a frame-to-clock
 * mapping, not a replacement estimate of physical device latency. NaN selects reported fallback. */
export function renderCursorTailSeconds(
  contextTime: number, timestampContext: number, timestampPerformance: number,
  nowMs: number, queuedFrames: number, sampleRate: number,
): number {
  if (![contextTime, timestampContext, timestampPerformance, nowMs, queuedFrames, sampleRate].every(Number.isFinite)
    || timestampContext <= 0 || timestampPerformance <= 0 || queuedFrames < 0 || sampleRate <= 0
    || contextTime < timestampContext || timestampPerformance > nowMs + 1
    || nowMs - timestampPerformance > 250) return NaN;
  const cursorLead = (timestampPerformance - nowMs) / 1000 + contextTime - timestampContext;
  if (cursorLead < 0 || cursorLead > 1) return NaN;
  return cursorLead + queuedFrames / sampleRate;
}

export interface CompensationTerms {
  renderCursorSeconds?: number | null; // paired queue-tail presentation estimate; absent = reported fallback
  hop1Frames: number; // hop-1 (Rust→drain) residency, frames
  hop2Frames: number; // hop-2 (drain→worklet) residency, frames
  cpalOutSeconds: number; // the native monitor's output latency (s)
  baseLatency: number; // ctx.baseLatency (render FIFO), s
  outputLatency: number; // ctx.outputLatency (device), s
  outputGraphLatencySeconds?: number; // measured master-limiter DSP delay; 0 for graphless formula probes
  trimMs: number; // the by-ear trim
  floorEnabled: boolean; // floor the click-path latency at cpalOut (a heuristic; A/B lever)
}

export interface Compensation {
  source: 'timestamp' | 'reported';
  hopFrames: number; // hop1 + hop2
  outputLatencyReported: number; // baseLatency + outputLatency (what Chromium/WebView2 reports)
  outputLatency: number; // timestamp diagnostic remainder, or effective reported fallback output latency
  outputFloored: boolean; // the floor lifted the reported value up to cpalOut
  cSeconds: number;
  frames: number; // max(0, round(cSeconds * sr))
}

/**
 *   C = paired queue-tail presentation − cpal_out + outputGraphLatency (+ trim)
 *   Fallback: hop_bridge + worklet_quantum − cpal_out + clickOut + outputGraphLatency (+ trim)
 *
 * The click-path output latency (`baseLatency + outputLatency`) lands only on the click side of the offset
 * (the record tap is captured pre-output). The FLOOR is a heuristic bet, not a proven bound: it can offset an
 * under-report of output latency, but adds (cpal_out - reported) when the report is accurate; when it bites, cpal_out cancels out exactly and
 * C = hop + quantum + outputGraphLatency. Graph DSP delay is added separately: the limiter sits before
 * AudioDestinationNode, outside the reported output latencies, and the native monitor bypasses it.
 * Clamped ≥ 0 because the current capture arm cannot move before its requested start.
 */
export function computeC(terms: CompensationTerms, sr: number): Compensation {
  const hopFrames = terms.hop1Frames + terms.hop2Frames;
  const outputLatencyReported = terms.baseLatency + terms.outputLatency;
  const timestamped = terms.renderCursorSeconds != null && Number.isFinite(terms.renderCursorSeconds) && terms.renderCursorSeconds >= 0;
  const outputLatency = timestamped ? terms.renderCursorSeconds! - hopFrames / sr : terms.floorEnabled
    ? Math.max(outputLatencyReported, terms.cpalOutSeconds)
    : outputLatencyReported;
  const outputFloored = !timestamped && outputLatency > outputLatencyReported;
  const recordOutputSeconds = timestamped ? terms.renderCursorSeconds!
    : hopFrames / sr + WORKLET_QUANTUM_FRAMES / sr + outputLatency;
  const cSeconds =
    recordOutputSeconds -
    terms.cpalOutSeconds +
    (terms.outputGraphLatencySeconds ?? 0) +
    terms.trimMs / 1000;
  const frames = Math.max(0, Math.round(cSeconds * sr));
  return { source: timestamped ? 'timestamp' : 'reported', hopFrames, outputLatencyReported, outputLatency, outputFloored, cSeconds, frames };
}
