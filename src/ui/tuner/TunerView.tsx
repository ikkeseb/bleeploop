import { For, Show, batch, createMemo, createSignal, onCleanup, onMount } from 'solid-js';
import { spell } from './pitch';
import { startTuner, type TunerHandle } from './tuner-loop';
import './tuner.css';

/**
 * The tuner: a full-window layer over the normal UI and the stage view, read from 1.5 m with a guitar in
 * hand. The note letter is the hero, its sharp beside it and the octave small; under it one cents scale,
 * flat to the left and sharp to the right, with its in-tune zone at the centre and one indicator; then
 * the cents and the frequency. In tune, the letter, the zone's mark and the indicator light warm-white and
 * the zone takes the engaged face (the app's engaged recipe, never a hue). Two lines of words: the
 * standing one (the guitar is muted, and how to leave) and, after a second of quiet, that no signal
 * arrives on the input.
 *
 * Mounted exactly while the engine says the tuner is on (`looper.tunerOn`, `src/app.tsx`): its close and
 * Escape send `SetTuner(false)` and never hide it themselves. The indicator, the cents and the frequency
 * are written by the draw loop (`tuner-loop.ts`, invariant 6); this side holds the three signals the loop
 * reports on change: the note, in tune, no signal.
 */

/** The cents scale's ticks, every ten cents; the centre is its own mark. */
const TICKS = [-50, -40, -30, -20, -10, 10, 20, 30, 40, 50];

const MUTED_LINE = 'the guitar is muted while you tune · N, Esc or the pedal closes the tuner';
const NO_SIGNAL_LINE = "no signal on the input, check the cable and the slot's input";

/** A sharp sign drawn, not set: a font's ♯ falls back to whatever the system has. Two uprights and two
 * rising bars, in `currentColor`. */
function SharpGlyph(props: { class: string }) {
  return (
    <svg class={props.class} viewBox="0 0 24 32" aria-hidden="true">
      <path d="M9 2.5v27M15 1.5v27" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" />
      <path d="M3.5 12.6l17-4v4.8l-17 4zM3.5 22.6l17-4v4.8l-17 4z" fill="currentColor" />
    </svg>
  );
}

/** A flat sign drawn, as `SharpGlyph`. */
function FlatGlyph(props: { class: string }) {
  return (
    <svg class={props.class} viewBox="0 0 24 32" aria-hidden="true">
      <path
        d="M7 1.5v28.5c4-2.6 12-6.3 12-12.2 0-3.3-2.4-5.2-5.2-5.2-2.6 0-5 1.5-6.8 3.6"
        fill="none"
        stroke="currentColor"
        stroke-width="2.8"
        stroke-linecap="round"
        stroke-linejoin="round"
      />
    </svg>
  );
}

export function TunerView(props: { onClose: () => void }) {
  const [midi, setMidi] = createSignal<number | null>(null);
  const [inTune, setInTune] = createSignal(false);
  const [noSignal, setNoSignal] = createSignal(false);
  const note = createMemo(() => {
    const m = midi();
    return m === null ? null : spell(m);
  });
  const state = () => (note() === null ? 'none' : inTune() ? 'in' : 'off');
  const spoken = () => {
    const n = note();
    if (n === null) return noSignal() ? NO_SIGNAL_LINE : 'no pitch';
    return `${n.name.replace('#', ' sharp')} ${n.octave}${inTune() ? ', in tune' : ''}`;
  };

  // A dialog over an inert page: focus moves into it (the root is a tabindex=-1 focus target, not a
  // control, so the transport keys stay live: transport-keys.ts's yield rule). app.tsx returns it.
  let root: HTMLDivElement | undefined;
  let needle: HTMLDivElement | undefined;
  let cents: HTMLSpanElement | undefined;
  let hz: HTMLSpanElement | undefined;
  let handle: TunerHandle | null = null;
  onMount(() => {
    queueMicrotask(() => root?.focus());
    if (!needle || !cents || !hz) return;
    const reduced = matchMedia('(prefers-reduced-motion: reduce)');
    handle = startTuner(
      { needle, cents, hz },
      (m, tuned, quiet) =>
        batch(() => {
          setMidi(m);
          setInTune(tuned);
          setNoSignal(quiet);
        }),
      reduced.matches,
    );
    const onReduced = (): void => handle?.setReduced(reduced.matches);
    reduced.addEventListener('change', onReduced);
    onCleanup(() => reduced.removeEventListener('change', onReduced));
  });
  onCleanup(() => handle?.stop());

  return (
    <div
      class="tn"
      ref={root}
      role="dialog"
      aria-modal="true"
      aria-label="Tuner"
      tabindex={-1}
      data-state={state()}
      data-signal={noSignal() ? 'none' : undefined}
    >
      <button type="button" class="tn-close" tabindex="-1" aria-label="Close tuner" title="Close tuner (N or Esc)" onClick={() => props.onClose()}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true">
          <path d="M6 6l12 12M18 6L6 18" stroke-linecap="round" />
        </svg>
      </button>

      <div class="tn-body">
        <div class="tn-note" aria-hidden="true">
          <span class="tn-letter">{note()?.name.charAt(0) ?? '–'}</span>
          <Show when={note()?.sharp}>
            <SharpGlyph class="tn-sharp" />
          </Show>
          <span class="tn-octave">{note()?.octave ?? ''}</span>
        </div>

        <div class="tn-scale" aria-hidden="true">
          <FlatGlyph class="tn-end tn-end--flat" />
          <div class="tn-track">
            <i class="tn-line" />
            <For each={TICKS}>{(c) => <i class="tn-tick" classList={{ 'tn-tick--end': Math.abs(c) === 50 }} style={{ left: `${50 + c}%` }} />}</For>
            <i class="tn-zone" />
            <i class="tn-centre" />
            <div class="tn-needle" ref={needle}>
              <i />
            </div>
          </div>
          <SharpGlyph class="tn-end tn-end--sharp" />
        </div>

        <div class="tn-read" aria-hidden="true">
          <span class="tn-cents">
            <span ref={cents} />
            <span class="tn-unit">{'¢'}</span>
          </span>
          <span class="tn-hz">
            <span ref={hz} /> Hz
          </span>
        </div>

        <p class="tn-quiet">{noSignal() ? NO_SIGNAL_LINE : ''}</p>
      </div>

      <p class="tn-muted">{MUTED_LINE}</p>
      <span class="tn-sr" role="status" aria-live="polite">
        {spoken()}
      </span>
    </div>
  );
}
