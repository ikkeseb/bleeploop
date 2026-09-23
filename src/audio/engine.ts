import { getContext, setContext, start as toneStart } from 'tone';

/**
 * Firefox (through at least v150, 2026) does not implement the AudioParam-based spatial
 * `AudioListener` — `ctx.listener.positionX`/`forwardX`/… are `undefined`; it ships only the
 * legacy `setPosition()`/`setOrientation()` methods. Tone.js's lazy `initializeContext()`
 * constructs a `Listener` that wraps those nine values as `Param`s and asserts each is an
 * AudioParam, so the FIRST transport/looper access (`getTransport()` via `clock.ts`'s `tp()`)
 * throws `param must be an AudioParam`. That throw silently kills every clock/looper/FX control
 * in Firefox — buttons "do nothing" — while the on-screen keyboard's native `window` keydown
 * listeners keep working (which is exactly the misleading symptom). We don't use 3D spatial
 * audio, so we satisfy Tone's assert with inert REAL AudioParams: a `ConstantSourceNode`'s
 * `.offset` IS an `instanceof AudioParam` in Firefox. Must run BEFORE `setContext()` so Tone
 * sees the patched listener when it initializes. No-op in Chromium/WebView2, where the params
 * already exist — so the verified baseline there is untouched.
 */
function polyfillSpatialListener(ctx: AudioContext): void {
  const listener = ctx.listener as unknown as Record<string, AudioParam>;
  if (listener.positionX) return; // spec-compliant engines (Chromium, WebView2): nothing to do
  const names = [
    'positionX', 'positionY', 'positionZ',
    'forwardX', 'forwardY', 'forwardZ',
    'upX', 'upY', 'upZ',
  ];
  const keepAlive: ConstantSourceNode[] = [];
  for (const name of names) {
    const node = ctx.createConstantSource(); // never started or connected — inert
    keepAlive.push(node);
    Object.defineProperty(listener, name, { value: node.offset, configurable: true, enumerable: false });
  }
  // Hold the backing nodes so they can't be GC'd out from under the AudioParams we just exposed.
  Object.defineProperty(listener, '__lfSpatialPolyfill', { value: keepAlive, configurable: true });
}

/**
 * Master compressor for the SUMMED output. Its finite ratio and attack do not enforce a 0 dBFS
 * ceiling: high summed levels can still clip at the sink. ONE configuration, shared by live and offline
 * WAV-export master render (export/render.ts mirrors the audible chain) — keep them identical.
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

/** Measure the production limiter's pre-delay in samples, independently of output hardware latency. */
export async function measureMasterLimiterLatency(sampleRate: number): Promise<number> {
  const impulseFrame = 512;
  const frames = impulseFrame + Math.ceil(sampleRate * 0.05);
  const ctx = new OfflineAudioContext(1, frames, sampleRate);
  const buffer = ctx.createBuffer(1, frames, sampleRate);
  buffer.getChannelData(0)[impulseFrame] = 0.1;
  const source = ctx.createBufferSource();
  source.buffer = buffer;
  source.connect(makeMasterLimiter(ctx)).connect(ctx.destination);
  source.start();
  const pcm = (await ctx.startRendering()).getChannelData(0);
  const first = pcm.findIndex((sample) => Math.abs(sample) > 0.000001);
  if (first < impulseFrame) throw new Error('Could not measure the master limiter latency');
  return (first - impulseFrame) / sampleRate;
}

/**
 * The single shared audio engine. ONE AudioContext that synths, the looper, FX, and (later)
 * the native VST audio node all converge on — so they share one clock and one graph.
 *
 * Master graph:
 *   instrument sources -> instrumentBus -> looperInputBus -> masterGain -> limiter -> destination
 *                                          looperInputBus -> recordTap (the looper captures HERE)
 * The looper's capture worklet taps `recordTap`, NOT `looperInputBus` directly: everything
 * on looperInputBus also flows to recordTap, so synths/mic record + sound exactly as before. The
 * indirection exists so a source can be RECORDED without being web-AUDIBLE — the native-monitored
 * plugin connects its wet to `recordTap` (record) and to a separately-muteable
 * web-monitor gain → masterGain (audible), so it isn't heard twice when the cpal-out monitor is armed.
 *
 * The context is created lazily and starts suspended; `start()` resumes it on a user gesture.
 * Tone.js adopts our context via setContext() BEFORE any Tone node is constructed.
 */
class AudioEngine {
  private _ctx?: AudioContext;
  private _instrumentBus?: GainNode;
  private _looperInputBus?: GainNode;
  private _recordTap?: GainNode;
  private _masterGain?: GainNode;
  private _started = false;
  private _outputGraphLatencySeconds?: number;
  private _latencyMeasurement?: Promise<void>;

  private measureOutputGraphLatency(): Promise<void> {
    if (!this._latencyMeasurement) {
      this._latencyMeasurement = measureMasterLimiterLatency(this._ctx!.sampleRate)
        .then((seconds) => { this._outputGraphLatencySeconds = seconds; })
        .catch((error: unknown) => {
          this._latencyMeasurement = undefined; // the next start retries a transient offline-render failure
          console.error('[engine] Master limiter latency measurement failed', error);
          throw error;
        });
    }
    return this._latencyMeasurement;
  }

  private ensure(): void {
    if (this._ctx) return;
    const ctx = new AudioContext({ latencyHint: 'interactive' });
    polyfillSpatialListener(ctx); // Firefox: must precede setContext (see fn comment)
    setContext(ctx);
    if (getContext().rawContext !== ctx) {
      throw new Error('AudioEngine: Tone.js did not adopt the shared AudioContext');
    }

    const masterGain = ctx.createGain();
    const looperInputBus = ctx.createGain();
    const instrumentBus = ctx.createGain();
    // The looper's record-only tap. Fed by looperInputBus (so synths/mic record unchanged)
    // and, for a native-monitored plugin, by the plugin's wet directly (recorded but not web-audible).
    // It connects ONLY to the capture worklet (looper/capture.ts) — never to masterGain — so it is silent.
    const recordTap = ctx.createGain();

    // Master safety limiter (makeMasterLimiter — config shared with the offline export render).
    // Catches the SUMMED output (stacked loop tracks + live synths + native plugin audio). It sits AFTER
    // masterGain — i.e. downstream of the looper's record tap on looperInputBus — so recorded loops
    // are captured pre-limiter (clean) while playback is protected. The real headroom comes from
    // per-synth gain staging upstream; this is the net.
    const limiter = makeMasterLimiter(ctx);

    instrumentBus.connect(looperInputBus);
    looperInputBus.connect(masterGain);
    looperInputBus.connect(recordTap); // record-only mirror of the audible bus
    masterGain.connect(limiter);
    limiter.connect(ctx.destination);

    this._ctx = ctx;
    this._instrumentBus = instrumentBus;
    this._looperInputBus = looperInputBus;
    this._recordTap = recordTap;
    this._masterGain = masterGain;
    // `limiter` is kept alive by the masterGain -> limiter -> destination graph connection.
  }

  get ctx(): AudioContext {
    this.ensure();
    return this._ctx!;
  }
  /** Instrument sources (synths, later VST audio) connect here. */
  get instrumentBus(): GainNode {
    this.ensure();
    return this._instrumentBus!;
  }
  /** Instruments + mic converge here; audible (→ masterGain) and mirrored to recordTap. */
  get looperInputBus(): GainNode {
    this.ensure();
    return this._looperInputBus!;
  }
  /**
   * The looper's record-only tap. The capture worklet taps THIS, not looperInputBus. Carries
   * everything on looperInputBus PLUS any source that must be recorded without being web-audible (a
   * native-monitored plugin's wet). Never connected to masterGain → silent.
   */
  get recordTap(): GainNode {
    this.ensure();
    return this._recordTap!;
  }
  get masterGain(): GainNode {
    this.ensure();
    return this._masterGain!;
  }
  get started(): boolean {
    return this._started;
  }
  /** Measured DSP delay after masterGain. start() must complete before record compensation reads it. */
  get outputGraphLatencySeconds(): number {
    if (this._outputGraphLatencySeconds === undefined) {
      throw new Error('Master limiter latency is not measured yet; await engine.start() before recording');
    }
    return this._outputGraphLatencySeconds;
  }

  /** Resume audio on a user gesture. Idempotent — safe to call on every key press. */
  async start(): Promise<void> {
    this.ensure();
    if (this._ctx!.state !== 'running') await this._ctx!.resume();
    await toneStart();
    // All capture initialization awaits start(). Share one measurement across simultaneous starts,
    // retain its sample-rate-specific result, and never let a take silently assume zero DSP delay.
    await this.measureOutputGraphLatency();
    this._started = true;
  }
}

export const engine = new AudioEngine();
