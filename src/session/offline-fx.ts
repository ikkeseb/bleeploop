import {
  CrossFade,
  FeedbackDelay,
  Filter,
  Gain,
  connect as toneConnect,
  PitchShift,
  Reverb,
  type ToneAudioNode,
  type BaseContext,
} from 'tone';
import {
  FX_DIVISIONS,
  FX_PARAM_DEFS,
  REVERB_DECAY_SECONDS,
  REVERB_PRE_DELAY_SECONDS,
  type FxKind,
  type FxParamDef,
  type FxState,
} from '../ui/state/fx-metadata';

/**
 * OWNS: the wet master's FX for a session export — the per-track FX chain, the shared reverb send and
 * the master limiter, built on Tone inside the export's OfflineContext (`render.ts`). The engine plays
 * its own FX (lf-engine `effects.rs`); this offline render is what export's wet master still uses, so
 * its FX may sound unlike the engine's.
 *
 * Fixed order, each individually bypassable click-free:
 *   Filter -> PitchShift -> Stutter -> Delay  (inline, in series)
 *                                        └─ send -> SHARED Reverb -> master
 *
 * PitchShift adds ~50–100 ms of processing delay INLINE whenever it is enabled (wet>0), reproduced as
 * heard in the render.
 *
 * Bypass: Delay ramps its `wet` Signal; Filter, Pitch and Stutter use a `Tone.CrossFade`
 * whose `fade` Signal ramps dry<->wet. Reverb send bypass ramps the send gain. All ramps are 20ms.
 *
 * Rhythmic params resolve against an explicit ctx-time master grid (`FxChain.setTiming`).
 *
 * Nothing here constructs a Tone node at import time; every node receives the export's context.
 */

const RAMP = 0.02; // 20ms click-free ramp for all bypass/param transitions
const MAX_FEEDBACK = 0.95; // delay self-oscillation guard

/** The loop's ctx-time grid, supplied by the offline renderer. */
export interface FxTiming {
  anchor: number;
  beatPeriod: number;
}

function divisionBeats(index: number): number {
  const division = DIVISIONS[index];
  return (4 / parseInt(division, 10)) * (division.endsWith('.') ? 1.5 : 1);
}

// One second of control signal, shared by every track in this context. Playback rate sets the
// division length without rounding it to an integer number of frames; start offset sets grid phase.
const gateBuffers = new WeakMap<BaseAudioContext, AudioBuffer>();
function gateBuffer(ctx: BaseAudioContext): AudioBuffer {
  let buffer = gateBuffers.get(ctx);
  if (!buffer) {
    buffer = ctx.createBuffer(1, ctx.sampleRate, ctx.sampleRate);
    buffer.getChannelData(0).fill(1, 0, Math.floor(ctx.sampleRate / 2));
    gateBuffers.set(ctx, buffer);
  }
  return buffer;
}

export interface FxNode {
  readonly kind: FxKind;
  readonly label: string;
  /** Tone input; for a pure send node this is the tap the chain feeds. */
  readonly input: ToneAudioNode;
  /** Tone output for inline chaining; null for a terminal send node (reverb). */
  readonly output: ToneAudioNode | null;
  readonly paramDefs: readonly FxParamDef[];
  isBypassed(): boolean;
  setBypass(b: boolean): void;
  getParam(key: string): number;
  setParam(key: string, value: number): void;
  getState(): FxState;
  setState(s: FxState): void;
  dispose(): void;
}

// Note-division choices shared by tempo-synced params.
const DIVISIONS = FX_DIVISIONS;

/** Round + clamp a raw param value to a valid index into a fixed-length choice list. */
function clampIndex(value: number, length: number): number {
  return Math.max(0, Math.min(length - 1, Math.round(value)));
}

// ── Reverb send bus ──────────────────────────────────────────────────────────────────────
interface SharedReverb {
  bus: Gain; // send target: track reverb sends connect here
  reverb: Reverb;
  ready: Promise<unknown>;
}
/**
 * Build a reverb send bus: bus -> Reverb(wet=1) -> `dest`, in `context`. Every track's chain sends into
 * the one bus. IR generation is async — await `ready` before rendering the tail.
 */
export function makeReverbBus(dest: ToneAudioNode | AudioNode, context: BaseContext): SharedReverb {
  const bus = new Gain({ gain: 1, context });
  const reverb = new Reverb({
    context,
    decay: REVERB_DECAY_SECONDS,
    preDelay: REVERB_PRE_DELAY_SECONDS,
    wet: 1,
  });
  bus.connect(reverb);
  reverb.connect(dest as ToneAudioNode);
  const ready = reverb.generate();
  return { bus, reverb, ready };
}

// ── FxNode base: the shared bypass flag + param-driven state I/O ──────────────────────────
// getState/setState here are the chain's one serialization path; per-track FxState round-trips
// through session export/import. getState emits every param in paramDefs order via getParam; setState applies
// setParam per key in that same order THEN setBypass — the missing-key fallback is defensive
// (getState always writes every key, so it never fires on real round-trips). Do not reorder.
abstract class BaseFx {
  protected bypassed = true;
  abstract readonly paramDefs: readonly FxParamDef[];
  abstract getParam(key: string): number;
  abstract setParam(key: string, value: number): void;
  abstract setBypass(b: boolean): void;

  isBypassed(): boolean { return this.bypassed; }
  getState(): FxState {
    const params: Record<string, number> = {};
    for (const p of this.paramDefs) params[p.key] = this.getParam(p.key);
    return { bypassed: this.bypassed, params };
  }
  setState(s: FxState): void {
    for (const p of this.paramDefs) this.setParam(p.key, s.params[p.key] ?? this.getParam(p.key));
    this.setBypass(s.bypassed);
  }
}

// ── Filter (biquad, lowpass) — CrossFade bypass ──────────────────────────────────────────
class FilterFx extends BaseFx implements FxNode {
  readonly kind = 'filter' as const;
  readonly label = 'Filter';
  readonly input: Gain;
  readonly output: CrossFade;
  readonly paramDefs = FX_PARAM_DEFS.filter;
  private filter: Filter;
  private xfade: CrossFade;
  private cutoff = 1200;
  private q = 2;

  constructor(context: BaseContext) {
    super();
    this.input = new Gain({ gain: 1, context });
    this.filter = new Filter({ context, type: 'lowpass', frequency: this.cutoff, Q: this.q, rolloff: -24 });
    this.xfade = new CrossFade({ fade: 0, context }); // 0 = dry (bypassed) by default
    this.input.connect(this.xfade.a); // dry path
    this.input.connect(this.filter);
    this.filter.connect(this.xfade.b); // wet path
    this.output = this.xfade;
  }
  setBypass(b: boolean) {
    this.bypassed = b;
    this.xfade.fade.rampTo(b ? 0 : 1, RAMP);
  }
  getParam(key: string) { return key === 'cutoff' ? this.cutoff : this.q; }
  setParam(key: string, value: number) {
    if (key === 'cutoff') { this.cutoff = value; this.filter.frequency.rampTo(value, RAMP); }
    else { this.q = value; this.filter.Q.rampTo(value, RAMP); }
  }
  dispose() { this.input.dispose(); this.filter.dispose(); this.xfade.dispose(); }
}

// ── PitchShift — wet bypass ──────────────────────────────────────────────────────────────
// Build PitchShift on first enable. The dry branch and crossfade stay connected throughout, so
// enabling needs no live rewiring of the dry signal. Once used, retain the processor across bypass
// and CLEAR, preserving its warm delay lines and the lane's existing FX routing.
//
// NOTE (LATENCY): `windowSize: 0.1` below buys quality at the cost of ~50–100 ms of processing delay
// INLINE in the track chain whenever wet>0 (bypassed = wet 0 = no added latency); the render keeps it.
class PitchFx extends BaseFx implements FxNode {
  readonly kind = 'pitch' as const;
  readonly label = 'Pitch';
  readonly input: Gain;
  readonly output: CrossFade;
  readonly paramDefs = FX_PARAM_DEFS.pitch;
  private pitchShift: PitchShift | null = null;
  private semitones = 0;

  constructor(context: BaseContext) {
    super();
    this.input = new Gain({ gain: 1, context });
    this.output = new CrossFade({ fade: 0, context });
    this.input.connect(this.output.a);
  }
  setBypass(b: boolean) {
    if (!b && !this.pitchShift) {
      this.pitchShift = new PitchShift({ context: this.input.context, pitch: this.semitones, windowSize: 0.1, wet: 1 });
      this.input.connect(this.pitchShift);
      this.pitchShift.connect(this.output.b);
    }
    this.bypassed = b;
    this.output.fade.rampTo(b ? 0 : 1, RAMP);
  }
  getParam() { return this.semitones; }
  setParam(_key: string, value: number) {
    this.semitones = value;
    if (this.pitchShift) this.pitchShift.pitch = value;
  }
  dispose() { this.input.dispose(); this.pitchShift?.dispose(); this.output.dispose(); }
}

// ── Stutter (tempo-synced amplitude gate) — CrossFade bypass ─────────────────────────────
class StutterFx extends BaseFx implements FxNode {
  readonly kind = 'stutter' as const;
  readonly label = 'Stutter';
  readonly input: Gain;
  readonly output: CrossFade;
  readonly paramDefs = FX_PARAM_DEFS.stutter;
  private gate: GainNode;
  private control: AudioBufferSourceNode | null = null;
  private timing: FxTiming | null = null;
  private xfade: CrossFade;
  private rate = 1; // index into DIVISIONS

  constructor(context: BaseContext) {
    super();
    this.input = new Gain({ gain: 1, context });
    const ctx = this.input.context.rawContext as unknown as BaseAudioContext;
    this.gate = ctx.createGain();
    this.gate.gain.value = 0;
    this.xfade = new CrossFade({ fade: 0, context }); // dry by default
    this.input.connect(this.xfade.a); // dry
    this.input.connect(this.gate);
    toneConnect(this.gate, this.xfade.b); // gated
    this.output = this.xfade;
  }
  setBypass(b: boolean) { this.bypassed = b; this.xfade.fade.rampTo(b ? 0 : 1, RAMP); }
  getParam() { return this.rate; }
  setParam(_key: string, value: number) {
    const next = clampIndex(value, DIVISIONS.length);
    if (next === this.rate) return;
    this.rate = next;
    this.scheduleGate();
  }
  setTiming(timing: FxTiming): void {
    if (this.timing?.anchor === timing.anchor && this.timing.beatPeriod === timing.beatPeriod) return;
    this.timing = { ...timing };
    this.scheduleGate();
  }
  private scheduleGate(): void {
    if (!this.timing) return;
    const ctx = this.input.context.rawContext as unknown as BaseAudioContext;
    const period = this.timing.beatPeriod * divisionBeats(this.rate);
    // Native scheduling, independent of Tone's lookAhead. The offline graph can start at frame 0;
    // a live replacement gets two render quanta of lead, with its offset advanced by that same lead.
    const when = ctx.currentTime + (this.input.context.isOffline ? 0 : 256 / ctx.sampleRate);
    const offset = (((when - this.timing.anchor) % period) + period) % period / period;
    const next = ctx.createBufferSource();
    next.buffer = gateBuffer(ctx);
    next.loop = true;
    next.playbackRate.value = 1 / period;
    next.connect(this.gate.gain);
    next.start(when, offset);
    if (this.control) {
      const previous = this.control;
      previous.onended = () => previous.disconnect();
      previous.stop(when);
    }
    this.control = next;
  }
  dispose() {
    this.control?.stop();
    this.control?.disconnect();
    this.gate.disconnect();
    this.input.dispose();
    this.xfade.dispose();
  }
}

// ── Delay (tempo-synced feedback delay) — wet bypass ─────────────────────────────────────
class DelayFx extends BaseFx implements FxNode {
  readonly kind = 'delay' as const;
  readonly label = 'Delay';
  readonly input: FeedbackDelay;
  readonly output: FeedbackDelay;
  readonly paramDefs = FX_PARAM_DEFS.delay;
  private delay: FeedbackDelay;
  private time = 1;
  private feedback = 0.4;
  private mix = 0.3;
  private beatPeriod = 0.5;
  private timingSet = false;

  constructor(context: BaseContext) {
    super();
    // maxDelay locks the delayTime param's ceiling at construction. Default is 1s, but a 1/4
    // note at the lowest tempo (40 BPM, clock.ts MIN_BPM) is 1.5s — rampTo("4n") would then
    // throw a RangeError. maxDelay:2 covers 1/4 down to 30 BPM.
    this.delay = new FeedbackDelay({
      context,
      delayTime: this.beatPeriod * divisionBeats(this.time),
      feedback: this.feedback,
      wet: 0,
      maxDelay: 2,
    });
    this.input = this.delay;
    this.output = this.delay;
  }
  setBypass(b: boolean) { this.bypassed = b; this.delay.wet.rampTo(b ? 0 : this.mix, RAMP); }
  getParam(key: string) { return key === 'time' ? this.time : key === 'feedback' ? this.feedback : this.mix; }
  setParam(key: string, value: number) {
    if (key === 'time') {
      this.time = clampIndex(value, DIVISIONS.length);
      this.delay.delayTime.rampTo(this.beatPeriod * divisionBeats(this.time), RAMP);
    } else if (key === 'feedback') {
      this.feedback = Math.min(MAX_FEEDBACK, Math.max(0, value));
      this.delay.feedback.rampTo(this.feedback, RAMP);
    } else {
      this.mix = Math.min(1, Math.max(0, value));
      if (!this.bypassed) this.delay.wet.rampTo(this.mix, RAMP);
    }
  }
  setTiming(timing: FxTiming): void {
    if (this.timingSet && this.beatPeriod === timing.beatPeriod) return;
    this.timingSet = true;
    this.beatPeriod = timing.beatPeriod;
    // A new master grid is installed before its sources start. Do not leave the old tempo active
    // for Tone's 100 ms lookAhead; that can outlast the imported session's 80 ms scheduling lead.
    const now = this.delay.context.immediate();
    this.delay.delayTime.cancelScheduledValues(now);
    this.delay.delayTime.setValueAtTime(this.beatPeriod * divisionBeats(this.time), now);
  }
  dispose() { this.delay.dispose(); }
}

// ── Reverb send (into the shared reverb bus) — terminal send node ────────────────────────
class ReverbSendFx extends BaseFx implements FxNode {
  readonly kind = 'reverb' as const;
  readonly label = 'Reverb';
  readonly input: Gain;
  readonly output = null;
  readonly paramDefs = FX_PARAM_DEFS.reverb;
  private send: Gain;
  private amount = 0.3;

  constructor(context: BaseContext, reverbBus: Gain) {
    super();
    this.send = new Gain({ gain: 0, context }); // bypassed: no send
    this.send.connect(reverbBus);
    this.input = this.send;
  }
  setBypass(b: boolean) { this.bypassed = b; this.send.gain.rampTo(b ? 0 : this.amount, RAMP); }
  getParam() { return this.amount; }
  setParam(_key: string, value: number) {
    this.amount = Math.min(1, Math.max(0, value));
    if (!this.bypassed) this.send.gain.rampTo(this.amount, RAMP);
  }
  dispose() { this.send.dispose(); }
}

// ── The per-track chain ──────────────────────────────────────────────────────────────────
/**
 * Where a chain's output goes: `dest` takes the inline (dry) path, `reverbBus` the reverb send, both in
 * `context` (the export's OfflineContext, `render.ts`).
 */
export interface FxRouting {
  /** The render supplies its context without changing Tone's global one. */
  context: BaseContext;
  dest: ToneAudioNode | AudioNode;
  reverbBus: Gain;
}

export class FxChain {
  /** Connect the track's (raw) playback gain here. */
  readonly input: ToneAudioNode;
  /** [filter, pitch, stutter, delay, reverb] — fixed order, for UI + state. */
  readonly nodes: readonly FxNode[];
  private dryOut: Gain;

  constructor(initial: FxState[] | undefined, routing: FxRouting) {
    const context = routing.context;
    const filter = new FilterFx(context);
    const pitch = new PitchFx(context);
    const stutter = new StutterFx(context);
    const delay = new DelayFx(context);
    const reverb = new ReverbSendFx(context, routing.reverbBus);

    // Inline series: filter -> pitch -> stutter -> delay.
    filter.output.connect(pitch.input);
    pitch.output.connect(stutter.input);
    stutter.output.connect(delay.input);

    this.dryOut = new Gain({ gain: 1, context });
    delay.output.connect(this.dryOut);
    this.dryOut.connect(routing.dest as ToneAudioNode); // dry path to master
    delay.output.connect(reverb.input); // post-delay reverb send tap

    this.input = filter.input;
    this.nodes = [filter, pitch, stutter, delay, reverb];

    if (initial) this.setState(initial);
  }

  /** Snapshot all five FX states (JSON-serializable). */
  getState(): FxState[] { return this.nodes.map((n) => n.getState()); }
  /** Apply a previously-saved state array. */
  setState(states: FxState[]): void {
    states.forEach((s, i) => this.nodes[i]?.setState(s));
  }
  /** Refresh reused chains on a new jam; restarting a source on the same grid changes no FX phase. */
  setTiming(timing: FxTiming): void {
    if (!Number.isFinite(timing.anchor) || !Number.isFinite(timing.beatPeriod) || timing.beatPeriod <= 0) {
      throw new Error('FX timing requires a finite anchor and positive beat period');
    }
    (this.nodes[2] as StutterFx).setTiming(timing);
    (this.nodes[3] as DelayFx).setTiming(timing);
  }
  dispose(): void {
    for (const n of this.nodes) n.dispose();
    this.dryOut.dispose();
  }
}

/**
 * Master compressor for the SUMMED output. Its finite ratio and attack do not enforce a 0 dBFS
 * ceiling: high summed levels can still clip at the sink.
 */
export function makeMasterLimiter(ctx: BaseAudioContext): DynamicsCompressorNode {
  const limiter = ctx.createDynamicsCompressor();
  limiter.threshold.value = -1; // dBFS
  limiter.knee.value = 0; // hard knee — limit, don't soft-compress
  limiter.ratio.value = 20;
  limiter.attack.value = 0.003;
  limiter.release.value = 0.05;
  return limiter;
}
