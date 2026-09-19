/** DEV-only wall-clock observation immediately before the production capture worklet.
 * One timestamp per quantum, no allocation or messaging in process(). PCM passes unchanged. */
class RenderClockProcessor extends AudioWorkletProcessor {
  private readonly published: Int32Array;
  private readonly samples: Float64Array;
  private count = 0;

  constructor(options?: AudioWorkletNodeOptions) {
    super(options);
    const sab = options?.processorOptions.clockSab as SharedArrayBuffer;
    this.published = new Int32Array(sab, 0, 2);
    this.samples = new Float64Array(sab, 8);
  }

  process(inputs: Float32Array[][], outputs: Float32Array[][]): boolean {
    if (this.count * 2 + 1 < this.samples.length) {
      this.samples[this.count * 2] = currentFrame;
      this.samples[this.count * 2 + 1] = Date.now();
      Atomics.store(this.published, 0, ++this.count);
    } else {
      Atomics.store(this.published, 1, 1);
    }
    const output = outputs[0]?.[0];
    const input = inputs[0]?.[0];
    if (output) {
      if (input) output.set(input);
      else output.fill(0);
    }
    return true;
  }
}
registerProcessor('render-clock-probe', RenderClockProcessor);
