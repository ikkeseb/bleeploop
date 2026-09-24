/**
 * Plugin PCM source AudioWorkletProcessor — the playback end of the plugin bridge.
 *
 * Lives on the audio render thread and reads the native host's hop-1 ring itself: the WebView2
 * SharedBuffer arrives by TRANSFER on the node's port, so no main-thread step sits between the Rust
 * producer and the render (a main-thread stall starved the old two-ring drain and rejected takes,
 * 2026-09-24). Each quantum copies 128 frames of mono plugin PCM to the node's single output and
 * writes the consumer half of the ring header the producer reads: the read cursor, the drift
 * controller's level and the loss counters. Header layout: `src/audio/plugin-bridge.ts`.
 *
 * ONE SETPOINT. The queue (frames written, not yet read) is held at `targetFrames`, the drift
 * controller's `TARGET_FILL_SECONDS` in `src-tauri/src/host/transport.rs`. The controller's level is
 * the queue at the start of each quantum, smoothed over ~0.3 s; it steers the producer's resample ratio so the
 * queue holds against clock drift, and nothing else. Everything that is not drift starts a SETTLE:
 * the start, a new producer epoch (Rust bumps header[7] where production jumps) and a backlog past
 * `maxLagFrames`; an underrun starts over from the start. While it settles the level reports the setpoint, so the controller
 * never learns a step as drift, and the queue's mean is measured; at its end the read cursor moves
 * once by the mean's distance from the setpoint (frames dropped, or silence inserted). The mean, not
 * one sample: the queue saws by a producer block plus a render burst (~±8 ms), and a single sample
 * put the level up to a block off (measured 2026-09-24). So the bridge's delay, which is the record
 * path's, returns to the setpoint after any step. A take that spans a move fails its loss check.
 *
 * HARD RULE: zero allocation inside process(). All state is numbers and pre-built typed-array views.
 * Loaded with `?worker&url` so it runs in the AudioWorklet global scope.
 */

interface PluginPcmSourceOptions {
  /** Int32Array-backed SAB the main thread reads: see the STAT_* indices. */
  statsSab: SharedArrayBuffer;
  headerBytes: number;
  capacityFrames: number; // power of two
  targetFrames: number;
  maxLagFrames: number;
  settleFrames: number;
}

// hop-1 header (u32 words); mirrored in `plugin-bridge.ts` and `host/transport.rs`.
const H_WRITE = 0;
const H_READ = 1;
const H_LEVEL = 3;
const H_CONSUMED = 4;
const H_UNDERRUNS = 5;
const H_DROPPED = 6;
const H_EPOCH = 7;
// Stats SAB (i32); mirrored in `plugin-bridge.ts`.
const STAT_CONSUMED = 0;
const STAT_UNDERRUNS = 1;
const STAT_DROPPED = 2;
const STAT_QUEUE = 3; // the queue after the last quantum's pop (frames)
const STAT_LEVEL_X16 = 4; // the controller's level ×16 (frames)
const STAT_LIVE = 5; // 1 once the queue first reached the setpoint

/** A settle's correction smaller than this (seconds) is left to the controller: nothing is moved. */
const SETTLE_TOLERANCE_SECONDS = 0.001;
/** The controller level's smoothing time constant (seconds). */
const LEVEL_SECONDS = 0.3;

class PluginPcmSource extends AudioWorkletProcessor {
  private readonly stats: Int32Array;
  private readonly headerWords: number;
  private readonly cap: number;
  private readonly mask: number;
  private readonly target: number;
  private readonly maxLag: number;
  private readonly settleFrames: number;
  private readonly tolerance: number;
  private readonly alpha: number;
  private header: Uint32Array | null = null;
  private data: Float32Array | null = null;
  private closed = false;
  private live = false;
  private read = 0; // u32 read cursor
  private epoch = 0;
  private consumed = 0;
  private underruns = 0;
  private dropped = 0;
  private level = 0;
  private settleLeft = 0; // render frames left in the current settle; 0 = steady
  private settleSum = 0;
  private settleN = 0;
  private silenceLeft = 0; // frames of silence still to insert (a settle that found the queue short)

  constructor(options?: AudioWorkletNodeOptions) {
    super(options);
    const opts = options?.processorOptions as unknown as PluginPcmSourceOptions;
    this.stats = new Int32Array(opts.statsSab);
    this.headerWords = opts.headerBytes / 4;
    this.cap = opts.capacityFrames;
    this.mask = opts.capacityFrames - 1;
    this.target = opts.targetFrames;
    this.maxLag = opts.maxLagFrames;
    this.settleFrames = opts.settleFrames;
    this.tolerance = Math.round(sampleRate * SETTLE_TOLERANCE_SECONDS);
    this.alpha = 128 / (sampleRate * LEVEL_SECONDS);
    this.level = opts.targetFrames;
    this.port.onmessage = (e: MessageEvent) => {
      if (e.data instanceof ArrayBuffer) {
        this.header = new Uint32Array(e.data, 0, this.headerWords);
        this.data = new Float32Array(e.data, opts.headerBytes, opts.capacityFrames);
        this.read = this.header[H_READ] >>> 0; // the producer's view of the queue until the start
      } else if (e.data === 'close') {
        // Drop the views so the mapping can be collected; process() then ends the processor.
        this.header = null;
        this.data = null;
        this.closed = true;
      }
    };
  }

  process(_inputs: Float32Array[][], outputs: Float32Array[][]): boolean {
    if (this.closed) return false;
    const out = outputs[0]?.[0];
    const h = this.header;
    const d = this.data;
    if (!out) return true;
    if (!h || !d) {
      out.fill(0);
      return true;
    }
    const want = out.length; // 128
    const write = h[H_WRITE] >>> 0;
    const epoch = h[H_EPOCH] >>> 0;
    let avail = (write - this.read) >>> 0;
    if (!this.live) {
      // Start (and restart after an underrun) once a setpoint's worth is queued, from its newest
      // frames (the backlog is dropped).
      if (avail < this.target) {
        out.fill(0);
        h[H_LEVEL] = this.target;
        return true;
      }
      this.read = (write - this.target) >>> 0;
      avail = this.target;
      this.live = true;
      this.epoch = epoch;
      this.beginSettle();
      Atomics.store(this.stats, STAT_LIVE, 1);
    } else if (epoch !== this.epoch) {
      this.epoch = epoch;
      this.beginSettle();
    }
    if (avail > this.maxLag) {
      // A backlog past the cap (a suspended context, a producer burst) is dropped at once: bounded
      // latency beats a delay the slow controller would take minutes to drain.
      this.skip(avail - this.target);
      avail = this.target;
      this.beginSettle();
    }
    const queued = avail;

    let n = 0;
    if (this.silenceLeft > 0) {
      n = Math.min(this.silenceLeft, want);
      for (let i = 0; i < n; i++) out[i] = 0;
      this.silenceLeft -= n;
    }
    const take = Math.min(want - n, avail);
    let pos = this.read & this.mask;
    for (let i = 0; i < take; i++) {
      out[n + i] = d[pos];
      pos = (pos + 1) & this.mask;
    }
    this.read = (this.read + take) >>> 0;
    this.consumed = (this.consumed + take) >>> 0;
    avail -= take;
    n += take;
    if (n < want) {
      // Starved: count it once and start over, so the queue refills to the setpoint before playing on.
      // Waiting out a settle instead left it at ~0, where the next render burst starved it again.
      for (let i = n; i < want; i++) out[i] = 0;
      this.underruns = (this.underruns + 1) >>> 0;
      this.live = false;
      this.silenceLeft = 0;
      this.level = this.target;
    } else if (this.settleLeft > 0) {
      this.settleSum += queued;
      this.settleN++;
      this.settleLeft -= want;
      if (this.settleLeft <= 0) this.endSettle(avail);
    } else {
      this.level += (queued - this.level) * this.alpha;
    }

    h[H_READ] = this.read;
    h[H_LEVEL] = Math.max(0, Math.round(this.level));
    h[H_CONSUMED] = this.consumed;
    h[H_UNDERRUNS] = this.underruns;
    h[H_DROPPED] = this.dropped;
    Atomics.store(this.stats, STAT_CONSUMED, this.consumed | 0);
    Atomics.store(this.stats, STAT_UNDERRUNS, this.underruns | 0);
    Atomics.store(this.stats, STAT_DROPPED, this.dropped | 0);
    Atomics.store(this.stats, STAT_QUEUE, avail);
    Atomics.store(this.stats, STAT_LEVEL_X16, Math.round(this.level * 16));
    return true;
  }

  private beginSettle(): void {
    this.settleLeft = this.settleFrames;
    this.settleSum = 0;
    this.settleN = 0;
    this.level = this.target;
  }

  /** Move the read cursor once by the settled mean's distance from the setpoint (`avail`: what this
   * quantum's pop left, the most that can be dropped now). */
  private endSettle(avail: number): void {
    this.settleLeft = 0;
    const off = Math.round(this.settleSum / this.settleN) - this.target;
    if (off > this.tolerance) {
      this.skip(Math.min(off, avail));
    } else if (off < -this.tolerance) {
      this.silenceLeft = Math.min(-off, this.cap);
      this.underruns = (this.underruns + 1) >>> 0;
    }
    this.level = this.target;
  }

  /** Drop the oldest `frames` of the queue, counted for the take integrity check. */
  private skip(frames: number): void {
    this.read = (this.read + frames) >>> 0;
    this.dropped = (this.dropped + frames) >>> 0;
  }
}

registerProcessor('plugin-pcm-source', PluginPcmSource);
