// src/session/render.ts
// WAV-export v1: render the master mix WET — through each track's FX chain (filter → pitch →
// stutter → delay + the shared reverb send) and the master limiter (`offline-fx.ts`) — inside an
// explicitly owned OfflineAudioContext: source → track gain (volume/mute) → FxChain → master →
// limiter → out. The engine plays its own FX, so this render may sound unlike it. Stereo: the FX
// (reverb, delay, pitch) produce the stereo image; the dry paths stay centred.
//
// Warm-up passes are derived from the enabled delay/reverb tails and the FINAL period is kept: the
// retained slice is steady-state and loops even when the FX tail spans several short loop periods.
// Enabled PitchShift latency is reproduced as heard.
import { Gain, OfflineContext, getContext, setContext } from 'tone';
import { FxChain, makeMasterLimiter, makeReverbBus } from './offline-fx';
import type { ExportSnapshot } from '../ui/state/looper-types';
import { warmupPassesForFx } from './render-plan';
import { finalPeriod } from './wav';
import { framesPerBar } from '../ui/state/quantize';

/** Rendered stereo master, exactly masterLengthFrames per channel. */
export interface WetMaster {
  left: Float32Array;
  right: Float32Array;
}

/**
 * Render the wet, stereo master for `snap` at `bpm`, with rhythmic FX on the same frame-derived
 * beat period as the loop. Dotted-eighth stutter repeats every three one-bar loops; flattening
 * one loop retains one phase of that longer pattern, rather than its complete three-bar cycle.
 *
 * Every export node receives its own context explicitly, so async reverb preparation and failures
 * cannot change Tone's global context. Tone still builds its shared noise buffer (the reverb's) on its
 * global context: the app loads Tone with none (`src/main.tsx`), so the offline context stands in as
 * the global while it renders.
 */
export async function renderWetMaster(
  snap: ExportSnapshot,
  bpm: number,
  masterLevel: number,
): Promise<WetMaster> {
  const master = snap.masterLengthFrames;
  const sr = snap.sampleRate;
  const warmupPasses = warmupPassesForFx(snap.tracks, bpm, master / sr);
  // All warm-up passes + the retained final pass + a quantum of rounding slack.
  const duration = ((warmupPasses + 1) * master + 256) / sr;

  const offline = new OfflineContext(2, duration, sr);
  const cleanup: (() => void)[] = [];
  const global = getContext();
  if (typeof (global.rawContext as { createBuffer?: unknown }).createBuffer !== 'function') {
    setContext(offline);
    cleanup.push(() => setContext(global));
  }
  try {
    offline.transport.bpm.value = bpm;
    const raw = offline.rawContext;
    const masterGain = new Gain({ gain: masterLevel, context: offline });
    cleanup.push(() => masterGain.dispose());
    const limiter = makeMasterLimiter(raw as unknown as BaseAudioContext);
    cleanup.push(() => limiter.disconnect());
    masterGain.connect(limiter as unknown as AudioNode);
    limiter.connect(raw.destination as unknown as AudioNode);

    const reverbBus = makeReverbBus(masterGain, offline);
    cleanup.push(() => { reverbBus.bus.dispose(); reverbBus.reverb.dispose(); });

    for (const t of snap.tracks) {
      const buf = raw.createBuffer(1, master, sr);
      buf.getChannelData(0).set(t.pcm.subarray(0, master));
      const src = raw.createBufferSource();
      cleanup.push(() => src.disconnect());
      src.buffer = buf;
      src.loop = true;
      src.loopStart = 0;
      src.loopEnd = buf.duration;
      const gain = new Gain({ gain: t.muted ? 0 : t.volume, context: offline });
      cleanup.push(() => gain.dispose());
      src.connect(gain.input as unknown as AudioNode);
      const chain = new FxChain(t.fx, { context: offline, dest: masterGain, reverbBus: reverbBus.bus });
      cleanup.push(() => chain.dispose());
      chain.setTiming({ anchor: 0, beatPeriod: framesPerBar(bpm, sr) / sr / 4 });
      gain.connect(chain.input);
      src.start(0);
    }

    await reverbBus.ready;
    const rendered = await offline.render();
    return {
      left: finalPeriod(rendered.getChannelData(0), master, warmupPasses),
      right: finalPeriod(rendered.getChannelData(1), master, warmupPasses),
    };
  } finally {
    // Cleanup must neither replace the render error nor prevent the remaining nodes being released.
    cleanup.unshift(() => { offline.dispose(); });
    for (const dispose of cleanup.reverse()) {
      try { dispose(); }
      catch (error) { console.error('[export] offline graph cleanup failed', error); }
    }
  }
}
