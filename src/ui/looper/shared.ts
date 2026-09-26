import { createSignal, onCleanup } from 'solid-js';
import { looper, type TrackState } from '../state/audio';
import { framesPerBar } from '../../audio/quantize';

/**
 * OWNS: the looper-UI helpers that the lanes (`Looper.tsx`), the waveform renderer (`waveform.ts`) and
 * the command bar (`Transport.tsx`, `SessionTools.tsx`) share. They live apart from the components so
 * the renderer never imports a component that imports the renderer.
 */

/**
 * Bar count of a committed master loop from its length in frames. This is the shared bar-math: the
 * spoken loop length (`Looper.tsx`), the command bar's loop readout and the waveform bar grid
 * (`waveform.ts`) all derive their bar count from this ONE function, so the spoken length and the drawn
 * grid can never disagree. Returns 0 when no master is defined yet (loop length still unknown → no
 * grid). BPM is locked once a master commits, so the caller can safely pass a snapshot of `clock.bpm()`.
 */
export function masterBars(masterFrames: number, bpm: number, sampleRate: number): number {
  if (masterFrames <= 0) return 0;
  return Math.max(1, Math.round(masterFrames / framesPerBar(bpm, sampleRate)));
}

/**
 * True when any track is currently in one of the given states — the shared "scan all lanes" idiom
 * behind the command bar's ▶/■ ALL (`live` = play/overdub/record) and the controls a live capture
 * locks (`capturing` = record/overdub).
 */
export function anyTrackIn(...states: TrackState[]): boolean {
  return Array.from({ length: looper.trackCount }, (_, i) => looper.track(i)().state).some((s) =>
    states.includes(s),
  );
}

/** How long a "press twice to destroy a take" confirm stays armed (lane CLR, ✕ ALL, the CLEAR key). */
export const CONFIRM_WINDOW_MS = 2500;

/**
 * Two-step confirm latch. The first `trigger()` only ARMS (opens a `windowMs` window and returns); a
 * second within it runs `action` and disarms. The window auto-closes. Shared by the per-track CLR and
 * the command bar's ✕ ALL so "press twice to destroy a take" behaves identically — no blocking confirm.
 * Registers its own `onCleanup`, so call it during component setup.
 */
export function createTwoStepConfirm(action: () => void, windowMs = CONFIRM_WINDOW_MS) {
  const [armed, setArmed] = createSignal(false);
  let timer: ReturnType<typeof setTimeout> | undefined;
  const trigger = () => {
    if (armed()) {
      clearTimeout(timer);
      setArmed(false);
      action();
    } else {
      setArmed(true);
      timer = setTimeout(() => setArmed(false), windowMs);
    }
  };
  onCleanup(() => clearTimeout(timer));
  return { armed, trigger };
}
