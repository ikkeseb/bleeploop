import { For, Show, createEffect, createMemo, createSignal, onCleanup, onMount, type JSX } from 'solid-js';
import { clock, looper, sampleRate, type TrackState } from '../state/audio';
import { registerLane, unregisterLane } from './waveform';
import { createTwoStepConfirm, masterBars, volumeDb } from './shared';
import { announceLooper, liveMsg, playStopGate, recDubGate } from './gates';
import { DATA_STATE, createLaneView, type LaneWord } from './lane-state';
import { FxPanel } from './FxPanel';
import { mixGesture } from './mix-gesture';
import { Trim } from './Trim';
import './looper.css';

/**
 * The looper UI: five full-width horizontal LANES stacked in the looper zone (composition binding:
 * `src/ui/AGENTS.md`). Each lane, left→right: an identity box
 * (mono track number + state word), the round core REC/DUB gesture, a stacked PLAY-STOP/CLR pair,
 * the recessed wave well (holding the waveform canvas + its playhead), and a right cluster of
 * FX/MUTE/undo/reverse/copy/trim pills over a mix row (a horizontal volume slider and the pan).
 *
 * The audio engine, the waveform.ts rAF renderer (which draws each <canvas> by reading non-reactive
 * looper getters), and the per-track state machine are UNCHANGED — this is a presentation layer.
 * State colour is rationed to the lane's state token `--sc` (identity word, core ring/glow, left-edge
 * bleed); the waveform is coloured independently by waveform.ts via the global --rec/--dub/--play
 * tokens.
 */

/** The lane identity-box word for each `LaneWord` (a rolling RETAKE's TAKE adds its pass count). */
const STATE_WORD: Record<Exclude<LaneWord, 'TAKE'>, string> = {
  EMPTY: 'EMPTY',
  RECORDING: '● REC',
  ARMED: 'ARMED',
  LISTENING: 'LISTEN',
  OVERDUBBING: 'OVERDUB',
  PLAYING: 'PLAYING',
  STOPPED: 'STOPPED',
  FADING: 'FADING',
  ENDING: 'ENDING',
  MUTED: 'MUTED',
};

// ---- core glyphs. currentColor → the core's state colour.
//   ● record  (EMPTY / ARMED — pressing starts a take)
//   ⊕ overdub (PLAYING / STOPPED — pressing starts an overdub)
//   ■ end     (RECORDING / OVERDUBBING — pressing ends the live capture)
const IconRec = (): JSX.Element => (
  <svg viewBox="0 0 24 24" fill="currentColor">
    <circle cx="12" cy="12" r="6.5" />
  </svg>
);
const IconDub = (): JSX.Element => (
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4">
    <circle cx="12" cy="12" r="7" />
    <path d="M12 8.4v7.2M8.4 12h7.2" stroke-linecap="round" />
  </svg>
);
const IconEnd = (): JSX.Element => (
  <svg viewBox="0 0 24 24" fill="currentColor">
    <rect x="7" y="7" width="10" height="10" rx="1.5" />
  </svg>
);

function Fader(props: { index: number; disabled: boolean }) {
  // Production volume runs 0..1.5; preserve the 1.5 ceiling + 0 dB detent at 1.0.
  // A styled native <input type=range> (same idiom as the command bar's master/click sliders) over a
  // 0..150 integer domain (= vol × 100); the unity tick + dB read-out are the per-track additions.
  // It shows its gesture's value until the engine has it (`mixGesture`), and builds on what it shows.
  const vol = () => looper.mixShown(props.index, 'volume');
  const gesture = mixGesture(props.index, 'volume');
  const frac = () => Math.max(0, Math.min(1, vol() / 1.5));
  const dbStr = () => volumeDb(vol());

  const onInput = (e: Event) => {
    const input = e.target as HTMLInputElement;
    let v = Number(input.value) / 100;
    if (Math.abs(v - 1.0) < 0.05) v = 1.0; // 0 dB detent
    looper.setVolume(props.index, v);
    // Resync the DOM to the SHOWN value: inside the detent zone the snapped 1.0 equals the value already
    // shown, so Solid re-renders nothing and the native thumb would drift from the fill/dB readout for
    // the rest of the drag. Writing it back also makes the detent read as a magnetic snap on the thumb
    // itself.
    input.value = String(Math.round(vol() * 100));
  };
  const onKeyDown = (e: KeyboardEvent) => {
    gesture.onKeyDown();
    if (props.disabled) return;
    // Coarser 0.05 keyboard step (the native step is fine for drag) — Right/Up raise, Left/Down lower.
    if (e.key === 'ArrowRight' || e.key === 'ArrowUp') {
      looper.setVolume(props.index, Math.min(1.5, vol() + 0.05));
      e.preventDefault();
    } else if (e.key === 'ArrowLeft' || e.key === 'ArrowDown') {
      looper.setVolume(props.index, Math.max(0, vol() - 0.05));
      e.preventDefault();
    }
  };

  return (
    <div class="lp-lane__vol">
      <div class="lp-vbar-wrap">
        {/* unity (0 dB) tick sits behind the thumb (declared first) so the thumb reads over it at unity */}
        <div class="lp-vbar__unity" />
        <input
          class="lf-range lp-vbar"
          type="range"
          min="0"
          max="150"
          step="1"
          value={Math.round(vol() * 100)}
          style={{ '--fill': `${frac() * 100}%` }}
          disabled={props.disabled}
          aria-label={`Track ${props.index + 1} volume`}
          aria-valuetext={dbStr()}
          onInput={onInput}
          onKeyDown={onKeyDown}
          onKeyUp={gesture.onKeyUp}
          onPointerDown={gesture.onPointerDown}
          onLostPointerCapture={gesture.onLostPointerCapture}
          onBlur={gesture.onBlur}
        />
      </div>
      <span class="lp-vdb">{dbStr()}</span>
    </div>
  );
}

/** A pan as its read-out says it: `L 30`, `C` or `R 30` (percent of the way to that side). */
function panText(pan: number): string {
  const pct = Math.round(pan * 100);
  if (pct === 0) return 'C';
  return pct < 0 ? `L ${-pct}` : `R ${pct}`;
}

/** The pointer's detent around the centre, in percent: a drag inside it lands on the centre. The keys
 * step across it. */
const PAN_DETENT = 2;

/**
 * The lane's pan beside its volume fader: a native range over -100..100 (percent of the way to a side).
 * A drag snaps to the centre inside the detent; the arrow keys step by 1 and Page Up/Down by 10 (no
 * detent), Home and End go hard left and right, and 0 (while focused), a double-click or an Alt-click
 * centre it. It shows its gesture's value until the engine has it (`mixGesture`), as the fader does.
 */
function Pan(props: { index: number; disabled: boolean }) {
  const pan = () => looper.mixShown(props.index, 'pan');
  const gesture = mixGesture(props.index, 'pan');
  const pct = () => Math.round(pan() * 100);
  /** A pointer is down on it: only its drag snaps inside the detent. */
  let pointerHeld = false;
  /** An Alt-press centres, and its drag stays there until the pointer lets go. */
  let altHeld = false;
  const release = () => (pointerHeld = altHeld = false);

  const set = (v: number, input: HTMLInputElement) => {
    looper.setPan(props.index, v / 100);
    // Resync the DOM to the SHOWN value (inside the detent the thumb would drift from it), as the fader.
    input.value = String(pct());
  };
  const onInput = (e: Event) => {
    const input = e.target as HTMLInputElement;
    const v = Number(input.value);
    set(altHeld || (pointerHeld && Math.abs(v) <= PAN_DETENT) ? 0 : v, input);
  };
  const onPointerDown = (e: PointerEvent) => {
    gesture.onPointerDown();
    pointerHeld = true;
    altHeld = e.altKey;
    if (altHeld && !props.disabled) set(0, e.currentTarget as HTMLInputElement);
  };
  const onKeyDown = (e: KeyboardEvent) => {
    gesture.onKeyDown();
    if (props.disabled) return;
    const step: Record<string, number> = { ArrowRight: 1, ArrowUp: 1, ArrowLeft: -1, ArrowDown: -1, PageUp: 10, PageDown: -10 };
    let v: number;
    if (e.key in step) v = Math.max(-100, Math.min(100, pct() + step[e.key]));
    else if (e.key === 'Home') v = -100;
    else if (e.key === 'End') v = 100;
    else if (e.key === '0') v = 0;
    else return;
    e.preventDefault();
    set(v, e.currentTarget as HTMLInputElement);
  };

  return (
    <div class="lp-lane__pan">
      <div class="lp-pan-wrap">
        {/* the centre tick sits behind the thumb (declared first), as the fader's unity tick */}
        <div class="lp-pan__centre" />
        <input
          class="lf-range lp-pan"
          type="range"
          min="-100"
          max="100"
          step="1"
          value={pct()}
          disabled={props.disabled}
          aria-label={`Track ${props.index + 1} pan`}
          aria-valuetext={panText(pan())}
          onInput={onInput}
          onKeyDown={onKeyDown}
          onKeyUp={gesture.onKeyUp}
          onPointerDown={onPointerDown}
          onPointerUp={release}
          onPointerCancel={release}
          onLostPointerCapture={() => {
            release();
            gesture.onLostPointerCapture();
          }}
          onDblClick={(e) => !props.disabled && set(0, e.currentTarget)}
          onBlur={() => {
            release();
            gesture.onBlur();
          }}
        />
      </div>
      <span class="lp-pan__val">{panText(pan())}</span>
    </div>
  );
}

/** Stop a smooth scroll still running in the lane stack where it is: an instant scroll to the current
 * offset cancels it. */
function haltScroll(stack: HTMLElement | null | undefined): void {
  stack?.scrollTo({ top: stack.scrollTop, behavior: 'instant' });
}

/**
 * Scroll the lane stack just enough to show `lane` whole, from the nearer edge, and not at all when it
 * already shows; instantly under reduced motion. Only the stack scrolls: `scrollIntoView` would walk
 * every scroll container above it too, the overflow-hidden zones included. Whole pixels, rounded past
 * the edge, so a fractional lane edge never stays under the fold. A reveal still running stops first:
 * it is headed for the lane selected before, and a lane measured as in view before it lands would ride
 * out of view with it (a double press inside its first frames).
 */
function revealLane(lane: HTMLElement): void {
  const stack = lane.parentElement;
  if (!stack) return;
  haltScroll(stack);
  const view = stack.getBoundingClientRect();
  const r = lane.getBoundingClientRect();
  const dy =
    r.top < view.top ? Math.floor(r.top - view.top) : r.bottom > view.bottom ? Math.ceil(r.bottom - view.bottom) : 0;
  if (dy === 0) return;
  const still = matchMedia('(prefers-reduced-motion: reduce)').matches;
  stack.scrollBy({ top: dy, behavior: still ? 'instant' : 'smooth' });
}

function TrackLane(props: {
  index: number;
  fxTrack: number | null;
  onToggleFx: (i: number) => void;
  returnFocus?: (el: HTMLElement | undefined) => void;
}) {
  const track = looper.track(props.index);
  const state = () => track().state;
  const canUndo = () => track().canUndo;
  const canReverse = () => track().canReverse;
  const reversed = () => track().reversed;
  const hasFreeLane = () => {
    for (let j = 0; j < looper.trackCount; j++) if (looper.track(j)().state === 'EMPTY') return true;
    return false;
  };
  // The lane's display state, word, well message and count-in numeral: `lane-state.ts`, shared with the
  // stage view. EMPTY shows no well message — the bright ● core already says "press to record"; the
  // spoken 'record' action lives on the core button's aria-label.
  const { displayState, word, stopping, fading, muted, cue, wellMsg, wellCount } = createLaneView(props.index);
  const fxSelected = () => props.fxTrack === props.index;
  const isEmpty = () => state() === 'EMPTY';
  const stateWord = () => {
    const w = word();
    return w === 'TAKE' ? `TAKE ${track().retakePass}` : STATE_WORD[w];
  };

  // The core IS the REC/DUB capture gesture. Its glyph reflects the ACTION reached by pressing it now
  // (● start-record on empty/armed, ⊕ start-overdub on a playing/stopped take, ■ end the live capture);
  // the data-state colour reflects the STATE. The spoken aria carries the exact action (glyphs read
  // unpredictably across screen readers, so words not glyphs), so the core is an action button, not a
  // toggle: no aria-pressed ("stop recording, pressed" contradicts itself). MUTE and REV below are
  // toggles: one label each, the state in aria-pressed.
  // Memoized on top of the memoized displayState: the JSX it returns is a fresh DOM node every call, so
  // it must only be called when the display state actually changes (see displayState above).
  const coreGlyph = createMemo(() => {
    switch (displayState()) {
      case 'RECORDING':
      case 'LISTENING':
      case 'OVERDUBBING':
        return <IconEnd />;
      case 'PLAYING':
      case 'STOPPED':
        return <IconDub />;
      default:
        return <IconRec />; // EMPTY, ARMED
    }
  });
  const recDubAria = () => {
    switch (displayState()) {
      case 'EMPTY':
        return 'record';
      case 'ARMED':
        return 'waiting for the downbeat to start recording';
      case 'LISTENING':
        return 'cancel auto record';
      case 'RECORDING':
        return 'stop recording';
      // STOPPED never speaks from here: recDubGate always refuses it, so recDubLabel says the reason.
      case 'PLAYING':
      case 'STOPPED':
        return 'overdub';
      case 'OVERDUBBING':
        return 'stop overdub';
    }
  };
  // The refusal gates (gates.ts) own every "why not": STOPPED (decision a — play it first), reverse
  // (M-4), a pending loop-end stop, and another lane capturing (except an EMPTY lane during a rolling
  // RETAKE). Memoized per lane so the five-lane scans run once per state change.
  const recGate = createMemo(() => recDubGate(props.index));
  const recDubLabel = () => {
    const g = recGate();
    return g.ok ? recDubAria() : g.reason;
  };
  const playGate = createMemo(() => playStopGate(props.index));
  const playStopGlyph = () => (state() === 'STOPPED' || state() === 'EMPTY' ? '▶' : '■');
  const playStopAction = () => (stopping() ? 'stop now' : playStopGlyph() === '▶' ? 'play' : 'stop');
  const playStopLabel = () => {
    const g = playGate();
    return g.ok ? playStopAction() : g.reason;
  };

  // Two-step clear: a take is irreversible, so the first press only ARMS ("SURE?") for a short window;
  // a second press within it actually clears. No blocking window.confirm — keeps the workflow fast.
  const clr = createTwoStepConfirm(() => looper.clear(props.index));

  // The waveform canvas is driven entirely by the plain-TS rAF renderer in waveform.ts; Solid only
  // mounts the element and (de)registers it. No reactivity touches the draw loop (invariant 6). The
  // <canvas> is created ONCE here and never wrapped in a keyed/conditional block, so a layout change
  // (stage resize / keyboard move / FX drawer open) never remounts it.
  let canvasEl: HTMLCanvasElement | undefined;
  onMount(() => {
    if (canvasEl) registerLane(props.index, canvasEl);
  });
  onCleanup(() => {
    if (canvasEl) unregisterLane(canvasEl);
  });

  // A lane a key or a footswitch selects comes into view: at small windows the stack scrolls, and a
  // selected lane below its fold hid its refusal cue too. Not on this lane's own pointer press: it is on
  // screen already, and a scroll between pointerdown and pointerup would move the pressed button out
  // from under the pointer and lose the click; for the same reason the press stops a reveal still
  // running. A press on the lane already selected changes nothing, so its `pressed` waits for the next
  // change, which moves the selection away and clears it here.
  let laneEl: HTMLDivElement | undefined;
  let pressed = false;
  createEffect((was: boolean) => {
    const selected = looper.selectedTrack() === props.index;
    if (selected && !was && !pressed && laneEl) revealLane(laneEl);
    pressed = false;
    return selected;
  }, looper.selectedTrack() === props.index);

  return (
    <div
      ref={laneEl}
      class="lp-lane"
      classList={{
        'is-selected': looper.selectedTrack() === props.index,
        'is-stopping': stopping(),
        'is-cued': cue() !== '',
      }}
      data-state={DATA_STATE[displayState()]}
      data-muted={muted() ? 'true' : undefined}
      role="group"
      aria-label={`Track ${props.index + 1}`}
      aria-current={looper.selectedTrack() === props.index ? 'true' : undefined}
      onPointerDown={() => {
        pressed = true;
        haltScroll(laneEl?.parentElement);
        looper.selectTrack(props.index);
      }}
    >
      {/* left cluster: identity · core gesture · play-stop/clear pair */}
      <div class="lp-lane__left">
        <div class="lp-lane__idbox">
          <span class="lp-lane__num">{String(props.index + 1).padStart(2, '0')}</span>
          <span class="lp-lane__state">{stateWord()}</span>
        </div>

        <button
          class="lp-core"
          disabled={!recGate().ok}
          onClick={(e) => {
            // A pointer press selected the lane on pointerdown; a keyboard one (detail 0) selects here,
            // so the selected track's keys then act on the lane this take started on.
            if (e.detail === 0) looper.selectTrack(props.index);
            void looper.recDub(props.index);
          }}
          aria-label={`Track ${props.index + 1} ${recDubLabel()}`}
          title={recDubLabel()}
        >
          {coreGlyph()}
        </button>

        <div class="lp-lane__pair">
          <button
            class="lp-pb lp-pb--play"
            disabled={!playGate().ok}
            onClick={() => looper.playStop(props.index)}
            aria-label={`Track ${props.index + 1} ${playStopLabel()}`}
            title={
              fading()
                ? 'Fading out. Press to stop now.'
                : stopping()
                  ? 'Stopping at loop end. Press again to stop now.'
                  : playGate().ok
                    ? undefined
                    : playStopLabel()
            }
          >
            {playStopGlyph()} {stopping() ? 'NOW' : playStopGlyph() === '▶' ? 'PLAY' : 'STOP'}
          </button>
          <button
            class="lp-pb lp-pb--clr"
            classList={{ 'is-armed': clr.armed() }}
            disabled={isEmpty()}
            onClick={clr.trigger}
            onKeyDown={clr.onKeyDown}
            aria-label={
              clr.armed()
                ? `Track ${props.index + 1} clear, press again to confirm`
                : `Track ${props.index + 1} clear`
            }
          >
            {clr.armed() ? 'SURE?' : 'CLR'}
          </button>
        </div>
      </div>

      {/* the wave well — the recessed canvas surface (waveform + playhead drawn by waveform.ts). */}
      <div class="lp-lane__well">
        <canvas ref={canvasEl} />
        <Show when={wellMsg()}>
          <div class="lp-lane__wellmsg" classList={{ 'is-cue': cue() !== '' }}>
            <Show when={wellCount() > 0}>
              <span class="lp-lane__count" aria-hidden="true">
                {wellCount()}
              </span>
            </Show>
            {wellMsg()}
          </div>
        </Show>
      </div>

      {/* right cluster: FX / MUTE / undo / reverse pills over the mix row (volume and pan). */}
      <div class="lp-lane__right">
        <div class="lp-lane__mods">
          <button
            class="lp-pb lp-pb--fx"
            classList={{ 'is-on': fxSelected() }}
            disabled={isEmpty()}
            onClick={() => props.onToggleFx(props.index)}
            aria-label={`Track ${props.index + 1} FX`}
            aria-pressed={fxSelected()}
          >
            FX
          </button>
          <button
            class="lp-pb lp-pb--mute"
            classList={{ 'is-on': looper.trackMuted(props.index) }}
            disabled={isEmpty()}
            onClick={() => looper.toggleMute(props.index)}
            aria-label={`Track ${props.index + 1} mute`}
            aria-pressed={looper.trackMuted(props.index)}
          >
            MUTE
          </button>

          {/* One-level undo of the last overdub or trim — appears only once a take has been dubbed or
              trimmed (its own affordance = "you just changed it, here's undo"). Toggles undo/redo. */}
          <Show when={canUndo()}>
            <button
              class="lp-pb lp-pb--undo"
              disabled={stopping()}
              onClick={() => looper.undoLastOverdub(props.index)}
              aria-label={`Track ${props.index + 1} undo or redo the last overdub or trim`}
              title="Undo / redo the last overdub or trim"
            >
              ↶ UNDO
            </button>
          </Show>

          {/* Per-track reverse — available whenever the track has a committed loop. Toggles
              forward/reversed; the swap lands on the next loop boundary (click-free, like undo). */}
          <Show when={canReverse()}>
            <button
              class="lp-pb lp-pb--rev"
              disabled={stopping()}
              classList={{ 'is-on': reversed() }}
              onClick={() => looper.reverse(props.index)}
              aria-label={`Track ${props.index + 1} reverse`}
              aria-pressed={reversed()}
              title="Reverse / forward (toggle)"
            >
              ↺ REV
            </button>
          </Show>

          {/* Track COPY — duplicates the whole lane (loop, volume, mute, FX) into the first EMPTY lane.
              Hidden while no lane is free. */}
          <Show when={canReverse() && hasFreeLane()}>
            <button
              class="lp-pb lp-pb--copy"
              onClick={() => looper.copy(props.index)}
              aria-label={`Copy track ${props.index + 1} to the first empty lane`}
              title="Copy this lane to the first empty lane"
            >
              <span>
                ⧉<span class="lp-pb__word"> COPY</span>
              </span>
            </button>
          </Show>

          {/* ✂ TRIM — keep the first N bars, repeated across the loop; one UNDO away. */}
          <Trim index={props.index} returnFocus={props.returnFocus} />
        </div>

        {/* the mix row: the volume fader and the pan */}
        <div class="lp-lane__mix">
          <Fader index={props.index} disabled={isEmpty()} />
          <Pan index={props.index} disabled={isEmpty()} />
        </div>
      </div>
    </div>
  );
}

/** `returnFocus` is the transport keys' (app.tsx), for a keyboard close of a lane's popover. */
export function Looper(props: { returnFocus?: (el: HTMLElement | undefined) => void }) {
  const master = () => looper.masterLengthFrames();
  const hasMaster = () => master() > 0;

  // Master length as a musician-readable "N bar · S.S s" — used only by the SR live announcement now
  // (the visible loop readout lives in the command bar's ring-dial).
  const masterLabel = () => {
    const m = master();
    if (m <= 0) return '—';
    const bars = masterBars(m, clock.bpm(), sampleRate());
    const secs = m / sampleRate();
    return `${bars} bar${bars > 1 ? 's' : ''} · ${secs.toFixed(1)} s`;
  };

  // One FX drawer for the whole looper: the index of the selected track (its drawer docks under that
  // lane), or null.
  const [fxTrack, setFxTrack] = createSignal<number | null>(null);
  const toggleFx = (i: number) => setFxTrack((cur) => (cur === i ? null : i));

  // If the track whose FX drawer is open gets cleared back to EMPTY, close the drawer.
  createEffect(() => {
    const i = fxTrack();
    if (i !== null && looper.track(i)().state === 'EMPTY') setFxTrack(null);
  });

  // Screen-reader status line: operational transitions are otherwise silent to AT. A polite live region
  // announces record/arm/overdub starts + the master-loop resolution, diffed so it fires on transitions.
  let prevSnap = Array.from({ length: looper.trackCount }, () => ({
    state: 'EMPTY' as TrackState,
    armed: false,
    autoArmed: false,
    stopAt: null as number | null,
    fading: false,
  }));
  let prevHasMaster = false;
  let prevSelected = looper.selectedTrack();
  createEffect(() => {
    const cur = Array.from({ length: looper.trackCount }, (_, i) => {
      const t = looper.track(i)();
      return { state: t.state, armed: t.armed, autoArmed: t.autoArmed, stopAt: t.stopAt, fading: t.fading === true };
    });
    const masterNow = hasMaster();
    let msg = '';
    // Selection first (1–5 keys are otherwise silent to AT: "Space now targets track 3" needs saying);
    // a state transition in the same tick overrides it below.
    const selected = looper.selectedTrack();
    if (selected !== prevSelected) msg = `Track ${selected + 1} selected`;
    prevSelected = selected;
    for (let i = 0; i < looper.trackCount; i++) {
      const p = prevSnap[i];
      const c = cur[i];
      const wasArmed = p.state === 'RECORDING' && p.armed;
      const isArmed = c.state === 'RECORDING' && c.armed;
      const wasListening = p.state === 'RECORDING' && p.autoArmed;
      const isListening = c.state === 'RECORDING' && c.autoArmed;
      const wasLiveRec = p.state === 'RECORDING' && !p.armed && !p.autoArmed;
      const isLiveRec = c.state === 'RECORDING' && !c.armed && !c.autoArmed;
      if (c.fading && !p.fading) msg = `Track ${i + 1} fading out. Press stop to stop now.`;
      else if (c.stopAt !== null && p.stopAt === null) msg = `Track ${i + 1} stopping at loop end. Press stop again to stop now.`;
      else if (c.state === 'STOPPED' && p.stopAt !== null) msg = `Track ${i + 1} stopped`;
      else if (isListening && !wasListening) msg = `Track ${i + 1} listening for input`;
      else if (isArmed && !wasArmed) msg = `Track ${i + 1} armed, waiting for the downbeat`;
      else if (isLiveRec && !wasLiveRec) msg = `Recording track ${i + 1}`;
      else if (c.state === 'OVERDUBBING' && p.state !== 'OVERDUBBING') msg = `Overdubbing track ${i + 1}`;
      else if (c.state === 'PLAYING' && p.state === 'RECORDING') msg = `Track ${i + 1} take recorded`;
      else if (c.state === 'PLAYING' && p.state === 'OVERDUBBING') msg = `Track ${i + 1} overdub committed`;
    }
    if (masterNow && !prevHasMaster) msg = `Loop length set: ${masterLabel()}`;
    prevSnap = cur;
    prevHasMaster = masterNow;
    if (msg) announceLooper(msg);
  });

  return (
    <div class="lp">
      {/* Visually-hidden status line for screen readers — looper transitions are otherwise silent. */}
      <div class="lp__sr-status" role="status" aria-live="polite">
        {liveMsg()}
      </div>

      {/* The five lanes. Each item renders its lane plus — when its FX is selected — an FX drawer
          docked directly under it (the lane's own drawer). The drawer is a SIBLING of the lane, so
          opening it never remounts the lane's waveform canvas. */}
      <div class="lp__lanes">
        <For each={Array.from({ length: looper.trackCount }, (_, i) => i)}>
          {(i) => (
            <>
              <TrackLane index={i} fxTrack={fxTrack()} onToggleFx={toggleFx} returnFocus={props.returnFocus} />
              <Show when={fxTrack() === i}>
                <div class="lp-drawer" role="group" aria-label={`FX, Track ${i + 1}`}>
                  <div class="lp-drawer__head">
                    <span class="lp-drawer__title">FX · Track {i + 1}</span>
                    <button
                      class="lp-drawer__close"
                      onClick={() => setFxTrack(null)}
                      aria-label="Close FX panel"
                      title="Close FX panel"
                    >
                      ✕
                    </button>
                  </div>
                  <FxPanel index={i} />
                </div>
              </Show>
            </>
          )}
        </For>
      </div>
    </div>
  );
}
