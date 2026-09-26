import { For, Show, onCleanup, onMount } from 'solid-js';
import { clock, looper, sampleRate } from '../state/audio';
import { registerLane, registerLoopProgress, unregisterLane, unregisterLoopProgress } from '../looper/waveform';
import { masterBars, volumeDb } from '../looper/shared';
import { DATA_STATE, createLaneView, type LaneWord } from '../looper/lane-state';
import './stage.css';

/**
 * The stage view: a full-window performance layer over the normal UI, readable from 1.5–3 m with a
 * guitar in hand. A header strip (BPM, the loop length, a four-segment beat bar, the loop's progress)
 * over five equal lanes, each a state pillar (lane number + a big state word on a tint of its state
 * colour) · the lane's waveform and playhead · read-only volume/MUTE/REV indicators. The selected lane
 * carries a warm-white edge; a pointer press on a lane selects it; the only control is EXIT.
 *
 * A presentation layer like the looper lanes: the lane words, well messages and count-in come from
 * `lane-state.ts` (the derivation `Looper.tsx` reads), the waveforms from the same rAF renderer
 * (`waveform.ts`, which also drives the progress fill — invariant 6), and every press still goes through
 * the transport keys and named actions, which stay live here. Mounted only while open
 * (`stage-store.ts`); the normal UI stays mounted underneath, inert and hidden (app.tsx).
 */

/** The stage word for each `LaneWord`: short, so the largest size fits the pillar. */
const STAGE_WORD: Record<Exclude<LaneWord, 'TAKE'>, string> = {
  EMPTY: 'EMPTY',
  RECORDING: 'REC',
  ARMED: 'ARMED',
  LISTENING: 'LISTEN',
  OVERDUBBING: 'DUB',
  PLAYING: 'PLAY',
  STOPPED: 'STOP',
  ENDING: 'ENDING',
  MUTED: 'MUTED',
};

function StageLane(props: { index: number }) {
  const track = looper.track(props.index);
  const { displayState, word, stopping, muted, cue, wellMsg, wellCount } = createLaneView(props.index);
  const text = () => {
    const w = word();
    return w === 'TAKE' ? `TAKE ${track().retakePass}` : STAGE_WORD[w];
  };
  const selected = () => looper.selectedTrack() === props.index;

  // Drawn by waveform.ts like the looper lane's canvas: Solid only mounts and (de)registers it.
  let canvasEl: HTMLCanvasElement | undefined;
  onMount(() => {
    if (canvasEl) registerLane(props.index, canvasEl);
  });
  onCleanup(() => {
    if (canvasEl) unregisterLane(canvasEl);
  });

  return (
    <div
      class="sv-lane"
      classList={{ 'is-selected': selected(), 'is-cued': cue() !== '', 'is-stopping': stopping() }}
      data-state={DATA_STATE[displayState()]}
      data-muted={muted() ? 'true' : undefined}
      role="group"
      aria-label={`Track ${props.index + 1}, ${text()}`}
      aria-current={selected() ? 'true' : undefined}
      onPointerDown={() => looper.selectTrack(props.index)}
    >
      <div class="sv-pillar">
        <span class="sv-num">{props.index + 1}</span>
        <span class="sv-word">{text()}</span>
      </div>

      <div class="sv-well">
        <canvas ref={canvasEl} />
        <Show when={wellMsg()}>
          <div class="sv-msg" classList={{ 'is-cue': cue() !== '' }}>
            <Show when={wellCount() > 0}>
              <span class="sv-count" aria-hidden="true">
                {wellCount()}
              </span>
            </Show>
            <span class="sv-msg__text">{wellMsg()}</span>
          </div>
        </Show>
      </div>

      <div class="sv-ind">
        <span class="sv-vol">
          <span class="sv-label">VOL</span>
          <span class="sv-vol__db">{volumeDb(looper.trackVolume(props.index))}</span>
        </span>
        <span class="sv-flags">
          <span class="sv-flag" classList={{ 'is-on': muted() }}>
            MUTE
          </span>
          <span class="sv-flag" classList={{ 'is-on': track().reversed }}>
            REV
          </span>
        </span>
      </div>
    </div>
  );
}

export function StageView(props: { onExit: () => void }) {
  const masterFrames = () => looper.masterLengthFrames();
  const bars = () => masterBars(masterFrames(), clock.bpm(), sampleRate());

  // A dialog over an inert page: focus moves into it (the root is a tabindex=-1 focus target, not a
  // control, so the transport keys stay live — transport-keys.ts's yield rule).
  let root: HTMLDivElement | undefined;
  let fill: HTMLElement | undefined;
  onMount(() => {
    queueMicrotask(() => root?.focus());
    if (fill) registerLoopProgress(fill);
  });
  onCleanup(unregisterLoopProgress);

  return (
    <div class="sv" ref={root} role="dialog" aria-modal="true" aria-label="Stage view" tabindex={-1}>
      <header class="sv-head">
        <div class="sv-stat sv-stat--bpm">
          <span class="sv-label">BPM</span>
          <span class="sv-stat__val">{clock.bpm()}</span>
        </div>
        <div class="sv-stat sv-stat--loop">
          <span class="sv-label">LOOP</span>
          <Show when={masterFrames() > 0} fallback={<span class="sv-stat__val sv-stat__val--none">—</span>}>
            <span class="sv-stat__val">
              {bars()}
              <span class="sv-unit">{bars() === 1 ? 'BAR' : 'BARS'}</span>
              <span class="sv-secs">{(masterFrames() / sampleRate()).toFixed(1)} s</span>
            </span>
          </Show>
        </div>
        <div class="sv-beats" role="img" aria-label={clock.running() ? `Beat ${clock.beat() + 1}` : 'Transport idle'}>
          <For each={[0, 1, 2, 3]}>
            {(b) => (
              <i class="sv-beat" classList={{ 'sv-beat--one': b === 0, 'is-on': clock.running() && clock.beat() === b }}>
                {b + 1}
              </i>
            )}
          </For>
        </div>
        <button type="button" class="sv-exit" tabindex="-1" aria-label="Exit stage view" title="Exit stage view (B or Esc)" onClick={props.onExit}>
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true">
            <path d="M6 6l12 12M18 6L6 18" stroke-linecap="round" />
          </svg>
        </button>
      </header>

      {/* The loop's progress: the fill's scaleX is set per frame by waveform.ts; the ticks mark the bars. */}
      <div class="sv-progress" style={{ '--bars': String(Math.max(1, bars())) }} aria-hidden="true">
        <i class="sv-progress__fill" ref={fill} />
      </div>

      <div class="sv-lanes">
        <For each={Array.from({ length: looper.trackCount }, (_, i) => i)}>{(i) => <StageLane index={i} />}</For>
      </div>
    </div>
  );
}
