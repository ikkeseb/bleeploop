/**
 * The `tone` module as the rig sees it: one chainable no-op Proxy for every node class the FX chain
 * and synths construct, plus the pieces the engine and clock actually depend on — the adopted raw
 * context, the transport BPM and the Draw queue. Draw callbacks run at once and their target times
 * are logged, so a guard reads the LED schedule the clock asked for.
 */

let raw: { currentTime: number } | null = null;

export interface DrawEntry {
  /** The target ctx time the callback was queued for. */
  time: number;
  /** ctx.currentTime when it was queued. */
  at: number;
  /** Filled by the rig's observer right after the callback ran (the clock's beat / count-in LED). */
  beat?: number;
  countLeft?: number;
}

/** Every `getDraw().schedule(fn, time)` since the last reset. */
export const toneLog = { draws: [] as DrawEntry[], observe: null as ((entry: DrawEntry) => void) | null };

export function resetTone(): void {
  raw = null;
  toneLog.draws.length = 0;
  toneLog.observe = null;
  transport.bpm.value = 120;
}

const toneContext = () => ({
  rawContext: raw,
  isOffline: false,
  lookAhead: 0.1,
  immediate: () => raw?.currentTime ?? 0,
  now: () => raw?.currentTime ?? 0,
});

type Chain = ((...args: unknown[]) => Chain) & Record<string, unknown>;

function chain(): Chain {
  return new Proxy(function () {} as unknown as Chain, {
    get: (_target, key) => {
      if (typeof key === 'symbol' || key === 'then') return undefined;
      if (key === 'value') return 0;
      if (key === 'context') return toneContext();
      return chain();
    },
    apply: () => chain(),
    construct: () => chain(),
    set: () => true,
  });
}

export const setContext = (ctx: { currentTime: number }): void => {
  raw = ctx;
};
export const getContext = () => toneContext();
export const start = async (): Promise<void> => {};
export const now = (): number => raw?.currentTime ?? 0;
const transport = { bpm: { value: 120 }, start(): void {}, stop(): void {} };
export const getTransport = () => transport;
export const getDraw = () => ({
  schedule(fn: () => void, time: number): void {
    const entry: DrawEntry = { time, at: raw?.currentTime ?? NaN };
    fn();
    toneLog.observe?.(entry);
    toneLog.draws.push(entry);
  },
});
export const connect = (from: { connect?: (to: unknown) => unknown }, to: unknown): void => {
  from.connect?.(to);
};

export const AMSynth = chain();
export const CrossFade = chain();
export const FeedbackDelay = chain();
export const FMSynth = chain();
export const Filter = chain();
export const Gain = chain();
export const MembraneSynth = chain();
export const MetalSynth = chain();
export const MonoSynth = chain();
export const NoiseSynth = chain();
export const OfflineContext = chain();
export const PitchShift = chain();
export const Reverb = chain();
export const Synth = chain();
export const Vibrato = chain();
