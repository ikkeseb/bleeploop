/**
 * Capture AudioWorkletProcessor — the record tap of the looper.
 *
 * Lives on the audio render thread. On every 128-frame quantum it pushes the mono
 * input (channel 0) plus its absolute start frame into a lock-free SharedArrayBuffer ring,
 * which the main thread drains into the active track's record buffer.
 *
 * HARD RULE: zero allocation inside process(). Everything (the RingBuffer view, the timestamped
 * packet, the heartbeat view) is pre-allocated in the constructor. process() only
 * reads from `inputs`, pushes into the ring, and bumps Atomics counters.
 *
 * The ring + heartbeat SharedArrayBuffers are handed in via processorOptions at construction
 * (see capture.ts). This module is loaded with `?worker&url`, so it runs in the AudioWorklet
 * global scope, NOT the bundle — keep its imports to things that work there (ringbuf.js is
 * pure JS + SharedArrayBuffer, fine in a worklet).
 */
import { RingBuffer } from 'ringbuf.js';
import { CAPTURE_PACKET_HEADER, CAPTURE_PACKET_SIZE, CAPTURE_QUANTUM_FRAMES } from '../capture-packet';

interface CaptureProcessorOptions {
  /** SharedArrayBuffer for complete Float64 timestamp + PCM packets. */
  ringSab: SharedArrayBuffer;
  /** Int32Array-backed SAB: [0] = monotonic quantum count (liveness), [1] = overrun total (dropped FRAMES). */
  heartbeatSab: SharedArrayBuffer;
}

const HEARTBEAT_QUANTUM_INDEX = 0;
/**
 * Overrun counter: incremented by the number of frames dropped whenever ring.push() can't write the
 * whole quantum (the ring is full — a stalled/backgrounded main-thread drain). ringbuf.js push() is
 * LOSSY on full (it returns how many it wrote and silently drops the rest), which breaks the frame-exact
 * write-head invariant the looper's arm/consume math relies on. We can't un-drop the frames here, but
 * counting them makes the loss OBSERVABLE (looper.captureOverruns()) instead of silent.
 */
const HEARTBEAT_OVERRUN_INDEX = 1;

class CaptureProcessor extends AudioWorkletProcessor {
  private readonly ring: RingBuffer;
  private readonly heartbeat: Int32Array;
  /** One pre-allocated timestamp + PCM packet. Disconnected input fills its PCM region with silence. */
  private readonly packet = new Float64Array(CAPTURE_PACKET_SIZE);
  /**
   * The frame the next quantum starts at. Chromium publishes `currentFrame` to the worklet scope under
   * a try-lock on the audio graph, so while the main thread holds that lock a quantum can see the
   * previous quantum's value (then a +256 jump). A rendered quantum never repeats, so a stamp never
   * goes below this.
   */
  private nextFrame = -1;

  constructor(options?: AudioWorkletNodeOptions) {
    super(options);
    const opts = options?.processorOptions as unknown as CaptureProcessorOptions;
    this.ring = new RingBuffer(opts.ringSab, Float64Array);
    this.heartbeat = new Int32Array(opts.heartbeatSab);
    // AudioWorklet quantum is fixed at 128 frames.
  }

  process(inputs: Float32Array[][]): boolean {
    const frame = Math.max(currentFrame, this.nextFrame);
    this.nextFrame = frame + CAPTURE_QUANTUM_FRAMES;
    const input = inputs[0];
    // input may be [] when nothing is connected upstream this quantum; push silence so the
    // write head still advances at the real-time rate (keeps track lengths frame-exact).
    const channel = input?.[0];
    // Single producer: only the consumer can increase available space after this check. Never
    // publish a partial packet, which could pair one quantum's timestamp with another one's PCM.
    if (this.ring.available_write() >= CAPTURE_PACKET_SIZE) {
      this.packet[0] = frame;
      this.packet[1] = CAPTURE_QUANTUM_FRAMES;
      for (let k = 0; k < CAPTURE_QUANTUM_FRAMES; k++) this.packet[CAPTURE_PACKET_HEADER + k] = channel?.[k] ?? 0;
      this.ring.push(this.packet, CAPTURE_PACKET_SIZE);
    } else {
      Atomics.add(this.heartbeat, HEARTBEAT_OVERRUN_INDEX, CAPTURE_QUANTUM_FRAMES);
    }
    // Heartbeat: prove process() is actually being pulled. Atomics write, zero allocation.
    Atomics.add(this.heartbeat, HEARTBEAT_QUANTUM_INDEX, 1);
    return true;
  }
}

registerProcessor('capture-processor', CaptureProcessor);
