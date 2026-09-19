/**
 * Ambient declarations for the AudioWorklet global scope.
 *
 * `lib.dom.d.ts` describes the AudioWorkletNode (main-thread) side but NOT the
 * AudioWorkletGlobalScope that a processor module runs in — so `AudioWorkletProcessor`,
 * `registerProcessor`, and the per-render globals (`sampleRate`, `currentTime`,
 * `currentFrame`) are undeclared. Declare just what the capture processor uses.
 *
 * Scoped to this directory by intent; kept minimal on purpose.
 */

declare global {
  /** Base class every AudioWorkletProcessor extends. Available only in the worklet scope. */
  abstract class AudioWorkletProcessor {
    readonly port: MessagePort;
    constructor(options?: AudioWorkletNodeOptions);
    process(
      inputs: Float32Array[][],
      outputs: Float32Array[][],
      parameters: Record<string, Float32Array>,
    ): boolean;
  }

  /** Registers a processor class under `name` for use via `new AudioWorkletNode(ctx, name)`. */
  function registerProcessor(
    name: string,
    processorCtor: new (options?: AudioWorkletNodeOptions) => AudioWorkletProcessor,
  ): void;

  /** Sample rate of the AudioContext this worklet runs in. */
  const sampleRate: number;
  /** Current time (seconds) of the AudioContext, sampled at the start of the quantum. */
  const currentTime: number;
  /** Absolute frame index of the first sample in this render quantum. */
  const currentFrame: number;
}

export {};
