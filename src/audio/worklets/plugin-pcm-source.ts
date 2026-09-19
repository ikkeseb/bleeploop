/**
 * Plugin PCM source AudioWorkletProcessor — the playback end of the two-ring plugin bridge.
 *
 * Lives on the audio render thread. The mirror image of capture-processor: instead of pushing the
 * looper input into a ring, it POPS mono PCM out of a lock-free ringbuf.js RingBuffer (the hop-2
 * SharedArrayBuffer) and writes it to its single mono output. The main-thread drain fills that ring
 * from the WebView2 SharedBuffer the native CLAP host writes (hop 1). The node feeds a per-plugin gain,
 * then splits to `recordTap` at full level and `webMonitorGain → masterGain` for audible monitoring.
 * The audible branch mutes while the native monitor is armed; the record branch does not.
 *
 * HARD RULE: zero allocation inside process(). The RingBuffer view, the scratch frame, and the
 * stats view are all pre-allocated in the constructor. On underrun (ring momentarily empty) we
 * output silence and bump a stats counter — never block, never allocate.
 *
 * Loaded with `?worker&url` so it runs in the AudioWorklet global scope (ringbuf.js is pure JS +
 * SharedArrayBuffer, fine there). Layout/handshake mirrored in `src/audio/plugin-bridge.ts`.
 */
import { RingBuffer } from 'ringbuf.js';

interface PluginPcmSourceOptions {
  /** SharedArrayBuffer for the Float32 hop-2 RingBuffer (RingBuffer.getStorageForCapacity). */
  ringSab: SharedArrayBuffer;
  /** Int32Array-backed SAB: [0]=frames consumed (liveness), [1]=underrun count. */
  statsSab: SharedArrayBuffer;
}

const STAT_CONSUMED = 0;
const STAT_UNDERRUNS = 1;

class PluginPcmSource extends AudioWorkletProcessor {
  private readonly ring: RingBuffer;
  private readonly stats: Int32Array;
  /** Pre-allocated pop target (worklet quantum is fixed at 128 frames). */
  private readonly scratch: Float32Array;

  constructor(options?: AudioWorkletNodeOptions) {
    super(options);
    const opts = options?.processorOptions as unknown as PluginPcmSourceOptions;
    this.ring = new RingBuffer(opts.ringSab, Float32Array);
    this.stats = new Int32Array(opts.statsSab);
    this.scratch = new Float32Array(128);
  }

  process(_inputs: Float32Array[][], outputs: Float32Array[][]): boolean {
    const out = outputs[0]?.[0];
    if (!out) return true;
    const want = out.length; // 128
    // pop(target, length) — no allocation; returns how many frames were actually available.
    const got = this.ring.pop(this.scratch, want);
    if (got > 0) {
      if (got === want) {
        out.set(this.scratch);
      } else {
        // Partial pop: copy `got` frames WITHOUT `subarray()`, which allocates a view object
        // (HARD RULE above: zero allocation inside process()). The [got, want) tail is zeroed below.
        for (let i = 0; i < got; i++) out[i] = this.scratch[i];
      }
      Atomics.add(this.stats, STAT_CONSUMED, got);
    }
    if (got < want) {
      // Underrun: ring drained faster than the main-thread drain refilled it. Emit silence for the
      // shortfall (keeps output continuous) and count it so the bridge can report health.
      out.fill(0, got);
      Atomics.add(this.stats, STAT_UNDERRUNS, 1);
    }
    return true;
  }
}

registerProcessor('plugin-pcm-source', PluginPcmSource);
