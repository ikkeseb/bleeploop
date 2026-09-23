/**
 * A controllable stand-in for the Web Audio surface the looper, clock and engine touch. Nothing
 * renders sound: the rig advances `currentTime` quantum by quantum, runs the real AudioWorklet
 * processors the app registered, and fires `ended` when a scheduled source's stop time passes.
 * Every scheduled source and oscillator is kept so a guard can read what the app asked the graph to
 * play and when. Methods the app does not call are left out on purpose: a new call fails loudly here
 * instead of passing silently.
 */

/** Frames of pre-delay the offline limiter render reports (engine.measureMasterLimiterLatency). */
export const offlineRender = { limiterLatencyFrames: 0 };

export class FakeParam {
  value: number;
  readonly events: { type: string; value: number; time: number }[] = [];
  constructor(value = 0) {
    this.value = value;
  }
  setValueAtTime(value: number, time: number): this {
    this.events.push({ type: 'set', value, time });
    return this;
  }
  linearRampToValueAtTime(value: number, time: number): this {
    this.events.push({ type: 'linear', value, time });
    return this;
  }
  exponentialRampToValueAtTime(value: number, time: number): this {
    this.events.push({ type: 'exp', value, time });
    return this;
  }
  setTargetAtTime(value: number, time: number): this {
    this.events.push({ type: 'target', value, time });
    return this;
  }
  cancelScheduledValues(): this {
    return this;
  }
  cancelAndHoldAtTime(): this {
    return this;
  }
}

export class FakeNode {
  readonly outputs = new Set<unknown>();
  channelCount = 2;
  channelCountMode = 'max';
  channelInterpretation = 'speakers';
  readonly context: FakeAudioContext;
  constructor(context: FakeAudioContext) {
    this.context = context;
  }
  connect<T>(target: T): T {
    this.outputs.add(target);
    return target;
  }
  disconnect(target?: unknown): void {
    if (target === undefined) this.outputs.clear();
    else this.outputs.delete(target);
  }
}

export class FakeGain extends FakeNode {
  readonly gain = new FakeParam(1);
}

export class FakeCompressor extends FakeNode {
  readonly threshold = new FakeParam(-24);
  readonly knee = new FakeParam(30);
  readonly ratio = new FakeParam(12);
  readonly attack = new FakeParam(0.003);
  readonly release = new FakeParam(0.25);
}

type EndedListener = () => void;

/** AudioScheduledSourceNode: start/stop bookkeeping and the `ended` event. */
export class FakeScheduledSource extends FakeNode {
  /** ctx.currentTime when the app created the node. */
  readonly createdAt: number;
  startTime: number | null = null;
  stopTime: number | null = null;
  /** Every stop() deadline in call order; the last one is the effective stop. */
  readonly stopCalls: number[] = [];
  ended = false;
  onended: EndedListener | null = null;
  private listeners: { fn: EndedListener; once: boolean }[] = [];

  constructor(context: FakeAudioContext) {
    super(context);
    this.createdAt = context.currentTime;
  }

  start(when = 0): void {
    if (this.startTime !== null) throw new Error('InvalidStateError: start() called twice');
    this.startTime = when;
    this.context.scheduled.add(this);
  }
  stop(when = 0): void {
    if (this.startTime === null) throw new Error('InvalidStateError: stop() before start()');
    this.stopTime = when;
    this.stopCalls.push(when);
  }
  addEventListener(type: string, fn: EndedListener, options?: { once?: boolean }): void {
    if (type === 'ended') this.listeners.push({ fn, once: options?.once === true });
  }
  removeEventListener(type: string, fn: EndedListener): void {
    if (type !== 'ended') return;
    const i = this.listeners.findIndex((l) => l.fn === fn);
    if (i >= 0) this.listeners.splice(i, 1);
  }
  /** The rig calls this once ctx time reaches the effective stop. */
  end(): void {
    this.ended = true;
    this.context.scheduled.delete(this);
    this.onended?.();
    const fired = this.listeners;
    this.listeners = fired.filter((l) => !l.once);
    for (const l of fired) l.fn();
  }
}

export class FakeOscillator extends FakeScheduledSource {
  type = 'sine';
  readonly frequency = new FakeParam(440);
  /** The GainNode the oscillator was connected to (the click's envelope). */
  get envelope(): FakeGain | undefined {
    for (const o of this.outputs) if (o instanceof FakeGain) return o;
    return undefined;
  }
}

export class FakeConstantSource extends FakeScheduledSource {
  readonly offset = new FakeParam(1);
}

export class FakeAudioBuffer {
  readonly duration: number;
  readonly numberOfChannels: number;
  readonly length: number;
  readonly sampleRate: number;
  private readonly channels: Float32Array[];
  constructor(channels: number, length: number, sampleRate: number) {
    this.length = length;
    this.sampleRate = sampleRate;
    this.numberOfChannels = channels;
    this.duration = length / sampleRate;
    this.channels = Array.from({ length: channels }, () => new Float32Array(length));
  }
  getChannelData(channel: number): Float32Array {
    const data = this.channels[channel];
    if (!data) throw new Error(`IndexSizeError: channel ${channel}`);
    return data;
  }
  copyToChannel(source: Float32Array, channel: number, offset = 0): void {
    this.getChannelData(channel).set(source, offset);
  }
  copyFromChannel(target: Float32Array, channel: number, offset = 0): void {
    target.set(this.getChannelData(channel).subarray(offset, offset + target.length));
  }
}

export class FakeBufferSource extends FakeScheduledSource {
  buffer: FakeAudioBuffer | null = null;
  loop = false;
  loopStart = 0;
  loopEnd = 0;
  readonly playbackRate = new FakeParam(1);
  /** start()'s `offset` argument (seconds into the buffer). */
  offset = 0;
  /** When set, the next start() throws this (fault injection for playback-failure paths). */
  static failNextStart: Error | null = null;
  override start(when = 0, offset = 0): void {
    const fault = FakeBufferSource.failNextStart;
    if (fault) {
      FakeBufferSource.failNextStart = null;
      throw fault;
    }
    super.start(when);
    this.offset = offset;
  }
}

interface ProcessorLike {
  process(inputs: Float32Array[][], outputs: Float32Array[][], parameters: Record<string, Float32Array>): boolean;
}
type ProcessorCtor = new (options?: unknown) => ProcessorLike;

/** Processors registered by imported worklet modules (`registerProcessor`), by name. */
export const processorRegistry = new Map<string, ProcessorCtor>();

export class FakeWorkletNode extends FakeNode {
  processor: ProcessorLike | null;
  readonly name: string;
  options: { processorOptions?: unknown };
  readonly port = { postMessage(): void {}, onmessage: null as unknown, start(): void {}, close(): void {} };
  constructor(context: FakeAudioContext, name: string, options: { processorOptions?: unknown } = {}) {
    super(context);
    this.name = name;
    this.options = options;
    const ctor = processorRegistry.get(name);
    this.processor = ctor ? new ctor(options) : null;
    context.worklets.push(this);
  }
}

export class FakeAudioContext {
  currentTime: number;
  readonly sampleRate: number;
  state: 'suspended' | 'running' | 'closed' = 'suspended';
  baseLatency = 0;
  outputLatency = 0;
  readonly destination: FakeNode;
  readonly listener: Record<string, FakeParam>;
  /** Started sources that have not ended yet. */
  readonly scheduled = new Set<FakeScheduledSource>();
  readonly oscillators: FakeOscillator[] = [];
  readonly bufferSources: FakeBufferSource[] = [];
  readonly worklets: FakeWorkletNode[] = [];
  readonly audioWorklet = {
    addModule: async (url: string): Promise<void> => {
      await import(url);
    },
  };
  onstatechange: (() => void) | null = null;

  /** The rig sets these before the engine constructs its context. */
  static nextSampleRate = 48000;
  static nextStartTime = 0;
  static created: FakeAudioContext[] = [];

  constructor(options?: { sampleRate?: number }) {
    this.sampleRate = options?.sampleRate ?? FakeAudioContext.nextSampleRate;
    this.currentTime = FakeAudioContext.nextStartTime;
    this.destination = new FakeNode(this);
    this.listener = Object.fromEntries(
      ['positionX', 'positionY', 'positionZ', 'forwardX', 'forwardY', 'forwardZ', 'upX', 'upY', 'upZ'].map((k) => [
        k,
        new FakeParam(),
      ]),
    );
    FakeAudioContext.created.push(this);
  }
  async resume(): Promise<void> {
    this.state = 'running';
  }
  async suspend(): Promise<void> {
    this.state = 'suspended';
  }
  async close(): Promise<void> {
    this.state = 'closed';
  }
  getOutputTimestamp(): { contextTime: number; performanceTime: number } {
    return { contextTime: this.currentTime, performanceTime: this.currentTime * 1000 };
  }
  createGain(): FakeGain {
    return new FakeGain(this);
  }
  createDynamicsCompressor(): FakeCompressor {
    return new FakeCompressor(this);
  }
  createOscillator(): FakeOscillator {
    const osc = new FakeOscillator(this);
    this.oscillators.push(osc);
    return osc;
  }
  createConstantSource(): FakeConstantSource {
    return new FakeConstantSource(this);
  }
  createBuffer(channels: number, length: number, sampleRate: number): FakeAudioBuffer {
    return new FakeAudioBuffer(channels, length, sampleRate);
  }
  createBufferSource(): FakeBufferSource {
    const src = new FakeBufferSource(this);
    this.bufferSources.push(src);
    return src;
  }
  /** Fire `ended` for every source whose effective stop has passed. */
  endDueSources(): void {
    // Deleting the visited entry during Set iteration is safe; end() does exactly that.
    for (const s of this.scheduled) {
      if (s.stopTime !== null && s.stopTime <= this.currentTime) s.end();
    }
  }
  /** Drop recorded history and heavy buffers once a rig generation is over. */
  release(): void {
    this.scheduled.clear();
    this.oscillators.length = 0;
    for (const s of this.bufferSources) s.buffer = null;
    this.bufferSources.length = 0;
    for (const w of this.worklets) {
      w.processor = null;
      w.options = {};
    }
    this.worklets.length = 0;
  }
}

export class FakeOfflineAudioContext extends FakeAudioContext {
  readonly length: number;
  constructor(channels: number, length: number, sampleRate: number) {
    super({ sampleRate });
    void channels;
    this.length = length;
  }
  async startRendering(): Promise<FakeAudioBuffer> {
    const out = this.createBuffer(1, this.length, this.sampleRate);
    // The limiter measurement feeds a 0.1 impulse at frame 512 and looks for its first output.
    out.getChannelData(0)[512 + offlineRender.limiterLatencyFrames] = 0.1;
    return out;
  }
}

class FakeWorkletProcessor {
  readonly port = { postMessage(): void {}, onmessage: null as unknown };
}

/** Install the fakes as globals. Idempotent. */
export function installAudioGlobals(): void {
  const g = globalThis as unknown as Record<string, unknown>;
  g.AudioContext = FakeAudioContext;
  g.OfflineAudioContext = FakeOfflineAudioContext;
  g.AudioWorkletNode = FakeWorkletNode;
  g.AudioWorkletProcessor = FakeWorkletProcessor;
  g.registerProcessor = (name: string, ctor: ProcessorCtor) => processorRegistry.set(name, ctor);
  g.self = globalThis;
  g.window = globalThis;
  g.crossOriginIsolated = true;
  const store = new Map<string, string>();
  g.localStorage = {
    getItem: (k: string) => store.get(k) ?? null,
    setItem: (k: string, v: string) => void store.set(k, String(v)),
    removeItem: (k: string) => void store.delete(k),
    clear: () => store.clear(),
  };
  g.document = {
    activeElement: null,
    querySelector: () => null,
    querySelectorAll: () => [],
    addEventListener: () => {},
    removeEventListener: () => {},
  };
}
