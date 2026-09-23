/**
 * `bootLooper()` loads a FRESH generation of the real `src/audio` modules (looper, capture, machine,
 * clock, engine) under plain Node and returns a rig that plays the audio thread: it renders
 * 128-frame quanta through the real capture worklet at absolute frame timestamps, advances
 * `ctx.currentTime`, fires the app's timers on that same clock and ends scheduled sources on time.
 * A guard presses the same buttons the UI does (`rig.looper.recDub(0)`) and reads what the app did:
 * track state, committed PCM, the sources it started, the clicks and LED beats the clock scheduled.
 *
 * What it cannot show: real render timing, the browser's scheduling jitter, WebView2, the native
 * host, or anything audible. Those stay with the browser probes, `pnpm verify:jam` and the rig lap.
 */
import './hooks.ts';
import {
  FakeAudioContext,
  FakeBufferSource,
  installAudioGlobals,
  offlineRender,
  type FakeOscillator,
} from './fake-audio.ts';
import { resetTone, toneLog, type DrawEntry } from './fake-tone.ts';
import { flushMicrotasks, VirtualTimers } from './timers.ts';
import type { looper as LooperApi } from '../../src/audio/looper/looper.ts';
import type { clock as ClockApi } from '../../src/audio/clock.ts';
import type { engine as EngineApi } from '../../src/audio/engine.ts';
import type * as StateModule from '../../src/audio/looper/state.ts';
import type * as NotifyModule from '../../src/notify.ts';

const QUANTUM = 128;
const srcRoot = new URL('../../src/', import.meta.url).href;

export interface RigOptions {
  /** Context sample rate (default 48000). */
  sampleRate?: number;
  /** ctx.currentTime when the engine builds its context (default 1 s), aligned down to a quantum. */
  startTime?: number;
  /** Pre-delay the offline limiter measurement reports, in frames (default 0). */
  limiterLatencyFrames?: number;
  /** Await `looper.init()` before returning (default true). */
  init?: boolean;
}

/** One metronome blip the clock scheduled. */
export interface Click {
  time: number;
  /** ctx.currentTime when the clock scheduled it. */
  at: number;
  accent: boolean;
  /** False when the clock cancelled it before it sounded (stop moved to or before its start). */
  audible: boolean;
  osc: FakeOscillator;
}

type Signal = number | ((frame: number) => number);

const timers = new VirtualTimers();
const logs: { level: string; args: unknown[] }[] = [];
let installed = false;
let generation = 0;
let current: LooperRig | null = null;

function install(): void {
  if (installed) return;
  installed = true;
  installAudioGlobals();
  timers.install();
  // App logs are kept for assertions; RIG_LOGS=1 also prints them.
  const echo = process.env.RIG_LOGS === '1';
  for (const level of ['error', 'warn', 'info', 'debug'] as const) {
    const original = console[level].bind(console);
    console[level] = (...args: unknown[]) => {
      logs.push({ level, args });
      if (echo) original(`[rig ${level}]`, ...args);
    };
  }
}

export class LooperRig {
  readonly generation: number;
  readonly looper: typeof LooperApi;
  readonly clock: typeof ClockApi;
  readonly engine: typeof EngineApi;
  readonly state: typeof StateModule;
  readonly notify: typeof NotifyModule;
  readonly timers = timers;
  /** console.error/warn/info/debug calls since boot. */
  readonly logs = logs;
  private signal: Signal = 0;
  private renderFrame: number;
  private readonly input = new Float32Array(QUANTUM);
  private readonly output = [[new Float32Array(QUANTUM)]];

  constructor(
    generation: number,
    startFrame: number,
    modules: {
      looper: typeof LooperApi;
      clock: typeof ClockApi;
      engine: typeof EngineApi;
      state: typeof StateModule;
      notify: typeof NotifyModule;
    },
  ) {
    this.generation = generation;
    this.renderFrame = startFrame;
    this.looper = modules.looper;
    this.clock = modules.clock;
    this.engine = modules.engine;
    this.state = modules.state;
    this.notify = modules.notify;
  }

  /** The engine's context (built on first access, like the app's). */
  get ctx(): FakeAudioContext {
    return this.engine.ctx as unknown as FakeAudioContext;
  }
  get sr(): number {
    return this.ctx.sampleRate;
  }
  get tracks(): StateModule.Track[] {
    return this.state.engineState.tracks;
  }
  /** The next render frame (= round(ctx.currentTime * sr)). */
  frame(): number {
    return this.renderFrame;
  }
  now(): number {
    return this.ctx.currentTime;
  }

  /** Load a generation-local `src/` module, e.g. `rig.import('audio/looper/grid-math.ts')`. */
  import<T = Record<string, unknown>>(path: string): Promise<T> {
    return import(`${srcRoot}${path}?g=${this.generation}`) as Promise<T>;
  }

  /** What reaches the record tap from now on: a constant or a function of the absolute frame. */
  setInput(signal: Signal): void {
    this.signal = signal;
  }

  /** Render `seconds` of audio (rounded to whole quanta). `timers: false` models a blocked main
   * thread: audio keeps rendering and the worklet keeps publishing, but no timer fires. */
  async advance(seconds: number, options: { timers?: boolean } = {}): Promise<void> {
    await this.advanceFrames(Math.round((seconds * this.sr) / QUANTUM) * QUANTUM, options);
  }

  /** Render until ctx.currentTime >= `time`. */
  async advanceTo(time: number, options: { timers?: boolean } = {}): Promise<void> {
    const target = Math.ceil((time * this.sr) / QUANTUM) * QUANTUM;
    if (target > this.frame()) await this.advanceFrames(target - this.frame(), options);
  }

  async advanceFrames(frames: number, options: { timers?: boolean } = {}): Promise<void> {
    const ctx = this.ctx;
    const quanta = Math.ceil(frames / QUANTUM);
    for (let q = 0; q < quanta; q++) {
      this.renderQuantum(ctx);
      ctx.endDueSources();
      if (options.timers !== false && timers.fire(ctx.currentTime * 1000) > 0) await flushMicrotasks();
    }
    await flushMicrotasks();
  }

  /** Render up to the next capture drain tick and let it run: the ring is empty right after. */
  async untilDrained(): Promise<void> {
    const due = timers.nextDue(/buildEngine/);
    if (!Number.isFinite(due)) throw new Error('untilDrained: no capture drain timer (looper not initialised?)');
    await this.advanceTo(due / 1000);
  }

  /** A main-thread stall of `seconds`: audio renders, no timer fires, then every overdue timer fires once. */
  async stall(seconds: number): Promise<void> {
    await this.advance(seconds, { timers: false });
    timers.fire(this.ctx.currentTime * 1000);
    await flushMicrotasks();
  }

  /** Let pending promise continuations run without advancing time. */
  flush(): Promise<void> {
    return flushMicrotasks();
  }

  /**
   * Record and commit a first take: REC on `lane`, the one-bar count-in, `bars` bars of `level`, REC
   * `stopAfter` seconds past the last bar line, then wait for the tail. Returns the master length.
   */
  async recordFirstTake({ lane = 0, bars = 2, level = 0.5, stopAfter = 0.05 } = {}): Promise<number> {
    this.setInput(level);
    const mark = toneLog.draws.length;
    await this.looper.recDub(lane);
    const count = toneLog.draws.slice(mark).find((d) => d.countLeft === 4);
    if (!count) throw new Error('recordFirstTake: no count-in started');
    const beat = 60 / this.clock.bpm();
    await this.advanceTo(count.time + 4 * beat + bars * 4 * beat + stopAfter);
    await this.looper.recDub(lane);
    await this.advance(0.25);
    return this.looper.masterLengthFrames();
  }

  /** Every metronome blip the clock scheduled, in scheduling order. */
  clicks(): Click[] {
    return this.ctx.oscillators.map((osc) => ({
      time: osc.startTime ?? NaN,
      at: osc.createdAt,
      accent: osc.frequency.value === 1500,
      audible: osc.stopTime === null || osc.stopTime > (osc.startTime ?? 0),
      osc,
    }));
  }

  /** Every LED beat the clock queued through Tone Draw, with the beat / count LED it set. */
  draws(): DrawEntry[] {
    return [...toneLog.draws];
  }

  /** Every AudioBufferSourceNode the app created, in creation order. */
  sources(): FakeBufferSource[] {
    return this.ctx.bufferSources;
  }

  /** Make the next AudioBufferSourceNode.start() throw. */
  failNextSourceStart(error = new Error('injected source start failure')): void {
    FakeBufferSource.failNextStart = error;
  }

  /** Release this generation's large buffers; its modules stay cached but inert. */
  release(): void {
    const es = this.state.engineState as unknown as Record<string, unknown>;
    for (const key of ['tracks', 'ring', 'drainScratch', 'packetScratch', 'heartbeat', 'retakeBuf']) {
      es[key] = key === 'tracks' ? [] : null;
    }
    for (const ctx of FakeAudioContext.created) ctx.release();
    FakeAudioContext.created.length = 0;
  }

  private renderQuantum(ctx: FakeAudioContext): void {
    const signal = this.signal;
    if (typeof signal === 'number') this.input.fill(signal);
    else for (let k = 0; k < QUANTUM; k++) this.input[k] = signal(this.renderFrame + k);
    const g = globalThis as unknown as Record<string, number>;
    g.currentFrame = this.renderFrame;
    g.currentTime = this.renderFrame / ctx.sampleRate;
    g.sampleRate = ctx.sampleRate;
    for (const node of ctx.worklets) {
      if (!node.processor) continue;
      const inputs = node.name === 'capture-processor' ? [[this.input]] : [[]];
      node.processor.process(inputs, this.output, {});
    }
    this.renderFrame += QUANTUM;
    ctx.currentTime = this.renderFrame / ctx.sampleRate;
  }
}

export async function bootLooper(options: RigOptions = {}): Promise<LooperRig> {
  install();
  current?.release();
  const gen = ++generation;
  const sampleRate = options.sampleRate ?? 48000;
  const startFrame = Math.floor(((options.startTime ?? 1) * sampleRate) / QUANTUM) * QUANTUM;
  FakeAudioContext.nextSampleRate = sampleRate;
  FakeAudioContext.nextStartTime = startFrame / sampleRate;
  FakeBufferSource.failNextStart = null;
  offlineRender.limiterLatencyFrames = options.limiterLatencyFrames ?? 0;
  timers.reset((startFrame / sampleRate) * 1000);
  resetTone();
  logs.length = 0;
  (globalThis as unknown as { localStorage: Storage }).localStorage.clear();
  const load = <T>(path: string) => import(`${srcRoot}${path}?g=${gen}`) as Promise<T>;
  const [looperModule, clockModule, engineModule, state, notify] = await Promise.all([
    load<{ looper: typeof LooperApi }>('audio/looper/looper.ts'),
    load<{ clock: typeof ClockApi }>('audio/clock.ts'),
    load<{ engine: typeof EngineApi }>('audio/engine.ts'),
    load<typeof StateModule>('audio/looper/state.ts'),
    load<typeof NotifyModule>('notify.ts'),
  ]);
  const rig = new LooperRig(gen, startFrame, {
    looper: looperModule.looper,
    clock: clockModule.clock,
    engine: engineModule.engine,
    state,
    notify,
  });
  current = rig;
  toneLog.observe = (entry) => {
    entry.beat = rig.clock.beat();
    entry.countLeft = rig.clock.countLeft();
  };
  if (options.init !== false) {
    await rig.looper.init();
    await rig.flush();
  }
  return rig;
}
