import { For, Show, createEffect, createMemo, createSignal, on, onCleanup, onMount, untrack } from 'solid-js';
import { clock, looper, sampleRate } from '../state/audio';
import { masterBars } from '../looper/shared';
import { DATA_STATE, createLaneView, type LaneWord } from '../looper/lane-state';
import { feed, type LaneKind } from './stage-feed';
import { startStage, type StageHandle } from './stage-loop';
import { currentStageView, nextStageView } from './stage-store';
import './stage.css';

/**
 * The stage view: a full-window performance layer over the normal UI, a visualizer first. One canvas
 * holds the look (`views.ts`: each lane a form that carries its loop, lights where it sounds and takes
 * its state's colour); a thin HUD at the edges keeps what a player with a guitar in hand must still
 * read: the lane chips (state by colour, the selected one warm-white), the tempo with the beat and the
 * bar, a message line (a refused press's reason, a wait, a pending stop) and the count-in numeral.
 * Two ghost buttons (the view switch and exit) hide when the pointer rests.
 *
 * Solid lives here and only here: the lane derivation is `lane-state.ts` (the one the looper lanes
 * read), and effects mirror it into the plain feed (`stage-feed.ts`) the draw loop reads (invariant 6).
 * Every press still goes through the transport keys and named actions, which stay live. Mounted only
 * while open (`stage-store.ts`); the normal UI stays mounted underneath, inert and hidden (app.tsx).
 */

/** How a chip names its lane's state to a screen reader. */
const SPOKEN: Record<Exclude<LaneWord, 'TAKE'>, string> = {
  EMPTY: 'empty',
  RECORDING: 'recording',
  ARMED: 'armed',
  LISTENING: 'listening for input',
  OVERDUBBING: 'overdubbing',
  PLAYING: 'playing',
  STOPPED: 'stopped',
  FADING: 'fading out',
  ENDING: 'stopping at the loop end',
  MUTED: 'muted',
};

/** The message line's colour follows what it says. */
type Tone = 'cue' | 'wait' | 'listen' | 'stop' | 'take';

const HUD_LIT_MS = 2000; // the HUD brightens this long after a change
const CONTROLS_MS = 3000; // the two buttons hide after this long without the pointer

export function StageView(props: { onExit: () => void }) {
  const lanes = Array.from({ length: looper.trackCount }, (_, i) => ({ i, track: looper.track(i), ...createLaneView(i) }));
  type Lane = (typeof lanes)[number];

  const bars = createMemo(() => masterBars(looper.masterLengthFrames(), clock.bpm(), sampleRate()));
  /** The count-in numeral: whichever lane counts in (a first take, or any lane the engine counts in). */
  const count = createMemo(() => {
    for (const l of lanes) if (l.wellCount() > 0) return l.wellCount();
    return 0;
  });
  /** The loop moves (`feed.moving`): otherwise the views show it cued at its start. */
  const moving = createMemo(() => {
    const overLoop = looper.masterLengthFrames() > 0;
    let waits = false;
    for (const l of lanes) {
      const d = l.displayState();
      if (d === 'PLAYING' || d === 'OVERDUBBING' || d === 'RECORDING') return true;
      // An armed lane waiting for the loop's boundary rides its phase; one the engine counts in (a
      // later take from stopped loops) waits at the loop's start, where its downbeat restarts it.
      if (d === 'ARMED' && overLoop && !looper.trackCounted(l.i)) waits = true;
    }
    return waits;
  });
  /** The beat is worth showing: the loop moves, or a count-in runs. */
  const live = () => moving() || count() > 0;
  // The bar of the loop that plays (1-based; 0 with no loop): re-read on each beat from the plain loop
  // phase (invariant 6: the beat signal, never the draw loop). The phase is rounded to the nearest beat
  // first, since a beat's signal and the phase are read a moment apart and a downbeat must not read as
  // the bar before it. At rest the loop is cued at its start: bar 1.
  const loopBar = createMemo(() => {
    clock.beat();
    const beats = bars() * 4;
    if (beats === 0) return 0;
    if (!moving()) return 1;
    return Math.floor((Math.round(looper.phaseValue() * beats) % beats) / 4) + 1;
  });

  const spoken = (l: Lane): string => {
    const w = l.word();
    return w === 'TAKE' ? `recording, take ${l.track().retakePass}` : SPOKEN[w];
  };
  /** What lane `l` has to say, with its tone: a wait, a pending stop or a rolling RETAKE's pass. */
  const said = (l: Lane): string => {
    const msg = l.wellMsg();
    if (msg) return `${l.fading() || l.stopping() ? 'stop' : l.displayState() === 'LISTENING' ? 'listen' : 'wait'}|${l.i + 1} · ${msg}`;
    return l.word() === 'TAKE' ? `take|${l.i + 1} · TAKE ${l.track().retakePass}` : '';
  };
  // The message line, as "tone|text": a refused press's reason outranks everything; under a count-in
  // numeral nothing else speaks; else the selected lane's message, else the first lane that has one.
  const message = createMemo(() => {
    for (const l of lanes) if (l.cue()) return `cue|${l.i + 1} · ${l.cue()}`;
    if (count() > 0) return '';
    const own = said(lanes[looper.selectedTrack()] ?? lanes[0]);
    if (own) return own;
    for (const l of lanes) if (said(l)) return said(l);
    return '';
  });
  // The line leaves over 160 ms: its last text stays that long, then the element empties.
  const [shown, setShown] = createSignal('');
  const [leaving, setLeaving] = createSignal(false);
  let leaveTimer: ReturnType<typeof setTimeout> | undefined;
  createEffect(() => {
    const m = message();
    clearTimeout(leaveTimer);
    if (m) {
      setShown(m);
      setLeaving(false);
    } else if (untrack(shown)) {
      setLeaving(true);
      leaveTimer = setTimeout(() => {
        setShown('');
        setLeaving(false);
      }, 160);
    }
  });
  const tone = () => shown().slice(0, shown().indexOf('|')) as Tone | '';
  const text = () => shown().slice(shown().indexOf('|') + 1);

  // ── The plain feed: signals are read here, in effects, and written to what the draw loop reads ──
  // A cue dismissed while the stage was closed does not flash on the next open.
  feed.cueAt.fill(0);
  for (const l of lanes) {
    createEffect(() => {
      feed.kind[l.i] = DATA_STATE[l.displayState()] as LaneKind;
      feed.fading[l.i] = l.fading();
      feed.stopping[l.i] = l.stopping();
      feed.muted[l.i] = l.muted();
      feed.volume[l.i] = looper.trackVolume(l.i);
    });
    createEffect(() => {
      if (l.cue()) feed.cueAt[l.i] = performance.now();
    });
  }
  createEffect(() => void (feed.selected = looper.selectedTrack()));
  createEffect(() => void (feed.beatsPerLoop = bars() * 4));
  createEffect(() => void (feed.running = clock.running()));
  createEffect(() => void (feed.counting = count() > 0));
  createEffect(() => void (feed.moving = moving()));
  // Each beat the engine makes heard (and each step of a count-in), a few a second.
  createEffect(
    on(
      [clock.beat, clock.countLeft],
      ([beat]) => {
        feed.beat = beat;
        feed.beatSeq++;
      },
      { defer: true },
    ),
  );
  feed.beat = clock.beat();
  const reduced = matchMedia('(prefers-reduced-motion: reduce)');
  const onReduced = (): void => void (feed.reduced = reduced.matches);
  onReduced();
  reduced.addEventListener('change', onReduced);
  onCleanup(() => reduced.removeEventListener('change', onReduced));

  // The HUD rests at 0.7 and brightens for a moment when what it says changes (never for the beat).
  const [lit, setLit] = createSignal(false);
  let litTimer: ReturnType<typeof setTimeout> | undefined;
  createEffect(
    on(
      () => [looper.selectedTrack(), clock.bpm(), bars(), currentStageView().id, ...lanes.map((l) => l.word())].join('|'),
      () => {
        setLit(true);
        clearTimeout(litTimer);
        litTimer = setTimeout(() => setLit(false), HUD_LIT_MS);
      },
      { defer: true },
    ),
  );

  // The two buttons (and the pointer) hide when the pointer rests; a move or any key brings them back.
  const [idle, setIdle] = createSignal(false);
  let idleTimer: ReturnType<typeof setTimeout> | undefined;
  const wake = (): void => {
    if (idle()) setIdle(false);
    clearTimeout(idleTimer);
    idleTimer = setTimeout(() => setIdle(true), CONTROLS_MS);
  };

  // A dialog over an inert page: focus moves into it (the root is a tabindex=-1 focus target, not a
  // control, so the transport keys stay live: transport-keys.ts's yield rule).
  let root: HTMLDivElement | undefined;
  let canvas: HTMLCanvasElement | undefined;
  let stage: StageHandle | null = null;
  onMount(() => {
    queueMicrotask(() => root?.focus());
    if (canvas && root) stage = startStage(canvas, root, currentStageView());
    window.addEventListener('pointermove', wake);
    window.addEventListener('pointerdown', wake);
    window.addEventListener('keydown', wake);
    wake();
  });
  createEffect(() => stage?.setView(currentStageView()));
  onCleanup(() => {
    stage?.stop();
    window.removeEventListener('pointermove', wake);
    window.removeEventListener('pointerdown', wake);
    window.removeEventListener('keydown', wake);
    clearTimeout(idleTimer);
    clearTimeout(litTimer);
    clearTimeout(leaveTimer);
  });

  /** A pointer press on a lane's form selects it (the view's own hit test); the buttons keep theirs. */
  const onPointerDown = (e: PointerEvent): void => {
    if ((e.target as Element).closest('button')) return;
    const i = stage?.hit(e.clientX, e.clientY) ?? -1;
    if (i >= 0) looper.selectTrack(i);
  };

  return (
    <div
      class="sv"
      classList={{ 'is-idle': idle() }}
      ref={root}
      role="dialog"
      aria-modal="true"
      aria-label="Stage view"
      tabindex={-1}
      data-view={currentStageView().id}
      style={{ '--sv-beat': `${(60 / Math.max(20, clock.bpm())).toFixed(3)}s` }}
      onPointerDown={onPointerDown}
    >
      <canvas class="sv-canvas" ref={canvas} aria-hidden="true" />

      <div class="sv-hud" classList={{ 'is-lit': lit() }}>
        <div class="sv-chips" role="list" aria-label="Tracks">
          <For each={lanes}>
            {(l) => (
              <span
                class="sv-chip"
                classList={{ 'is-selected': looper.selectedTrack() === l.i, 'is-stopping': l.stopping() }}
                role="listitem"
                data-state={DATA_STATE[l.displayState()]}
                data-muted={l.word() === 'MUTED' ? 'true' : undefined}
                aria-label={`Track ${l.i + 1}, ${spoken(l)}`}
                aria-current={looper.selectedTrack() === l.i ? 'true' : undefined}
              >
                {l.i + 1}
              </span>
            )}
          </For>
        </div>

        <div class="sv-tempo">
          <span class="sv-bpm" aria-label={`${Math.round(clock.bpm())} BPM`}>
            {Math.round(clock.bpm())}
          </span>
          <Show when={live() || bars() > 0}>
            <span class="sv-dots" role="img" aria-label={live() ? `Beat ${clock.beat() + 1}` : 'Transport idle'}>
              <For each={[0, 1, 2, 3]}>
                {(b) => <i class="sv-dot" classList={{ 'sv-dot--one': b === 0, 'is-on': live() && clock.beat() === b }} />}
              </For>
            </span>
          </Show>
          <Show when={bars() > 0}>
            <span class="sv-bar" aria-label={`Bar ${loopBar()} of ${bars()}`}>
              {loopBar()}
              <span class="sv-bar__of"> / {bars()}</span>
            </span>
          </Show>
        </div>

        <div class="sv-count" aria-live="assertive">
          <Show when={count()} keyed>
            {(n) => <span class="sv-count__n">{n}</span>}
          </Show>
        </div>

        <div class="sv-msg" classList={{ 'is-leaving': leaving() }} data-tone={tone() || undefined} role="status" aria-live="polite">
          {text()}
        </div>

        <div class="sv-ctl" classList={{ 'is-idle': idle() }}>
          <button
            type="button"
            class="sv-btn sv-btn--view"
            tabindex="-1"
            aria-label={`Stage look: ${currentStageView().name.toLowerCase()}. Next look`}
            title="Next stage view (V)"
            onClick={nextStageView}
          >
            <span class="sv-btn__name">{currentStageView().name}</span>
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true">
              <path d="M9 6l6 6-6 6" stroke-linecap="round" stroke-linejoin="round" />
            </svg>
          </button>
          <button type="button" class="sv-btn sv-btn--exit" tabindex="-1" aria-label="Exit stage view" title="Exit stage view (B or Esc)" onClick={props.onExit}>
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true">
              <path d="M6 6l12 12M18 6L6 18" stroke-linecap="round" />
            </svg>
          </button>
        </div>
      </div>
    </div>
  );
}
