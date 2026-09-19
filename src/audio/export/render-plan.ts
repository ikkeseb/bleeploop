// Pure offline-render duration planning. Kept outside render.ts so the FX-tail math is directly
// Node-verifiable without importing Tone/Web Audio; the actual graph still uses the live FxChain.
import {
  FX_DIVISIONS,
  FX_META,
  REVERB_DECAY_SECONDS,
  REVERB_PRE_DELAY_SECONDS,
  type FxState,
} from '../fx/metadata.ts';

const TAIL_THRESHOLD = 1e-4;

/** Resolve the real tempo-synced delay choice (4n/8n/8n./16n) to seconds at the export BPM. */
function divisionSeconds(index: number, bpm: number): number {
  const division = FX_DIVISIONS[index];
  if (!division) throw new Error(`offline render: unknown delay division index ${index}`);
  const match = /^(\d+)n(\.)?$/.exec(division);
  if (!match) throw new Error(`offline render: unsupported delay division ${division}`);
  const beats = (4 / Number(match[1])) * (match[2] ? 1.5 : 1);
  return beats * (60 / bpm);
}

/**
 * Derive the number of complete loop passes needed to warm enabled time-based FX into steady state.
 * Delay repeats until its feedback falls below 1e-4; the shared reverb uses its real fixed IR tail.
 * At least one warm-up pass is retained for the existing pitch/filter priming behaviour.
 */
export function warmupPassesForFx(
  tracks: readonly { fx: readonly FxState[] }[],
  bpm: number,
  loopSeconds: number,
): number {
  if (!Number.isFinite(bpm) || bpm <= 0) throw new Error(`offline render: invalid BPM ${bpm}`);
  if (!Number.isFinite(loopSeconds) || loopSeconds <= 0) {
    throw new Error(`offline render: invalid loop duration ${loopSeconds}`);
  }

  let tailSeconds = 0;
  for (const track of tracks) {
    for (let i = 0; i < FX_META.length; i++) {
      const state = track.fx[i];
      if (!state || state.bypassed) continue;

      if (FX_META[i].kind === 'delay' && state.params.mix > 0) {
        const delaySeconds = divisionSeconds(state.params.time, bpm);
        const feedback = state.params.feedback;
        const repeats = feedback > 0
          ? Math.max(1, Math.ceil(Math.log(TAIL_THRESHOLD) / Math.log(feedback)))
          : 1;
        tailSeconds = Math.max(tailSeconds, delaySeconds * repeats);
      } else if (FX_META[i].kind === 'reverb' && state.params.amount > 0) {
        tailSeconds = Math.max(
          tailSeconds,
          REVERB_DECAY_SECONDS + REVERB_PRE_DELAY_SECONDS,
        );
      }
    }
  }

  return Math.max(1, Math.ceil(tailSeconds / loopSeconds));
}
