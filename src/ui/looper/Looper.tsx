import { For, Show, createEffect, createMemo, createSignal, onCleanup, onMount, type JSX } from 'solid-js';
import { clock, looper, sampleRate, type TrackState } from '../state/audio';
import { registerLane, setGridBeats, setLaneFade, unregisterLane } from './waveform';
import { masterBars, noEnterRepeat, volumeDb } from './shared';
import { announceLooper, liveMsg, playStopGate, recDubGate, reverseGate, sentence, undoGate, why } from './gates';
import { DATA_STATE, createLaneView, type LaneWord } from './lane-state';
import { FxPanel } from './FxPanel';
import { mixGesture } from './mix-gesture';
import { Edit } from './Edit';
import { inputArmed } from '../state/native-io';
import { runAction } from '../../app/actions';
import './looper.css';

/**
 * The looper UI: five lanes of equal height stacked in the looper zone (composition binding:
 * `src/ui/AGENTS.md`), each read left to right as HEAD | GLASS | MIX, the columns aligned to the pixel down
 * the five (docs/redesign/main.html while the build is open):
 * - the head: the lane number printed large over its state word (a lit legend in the state colour, no
 *   plate) and, on the selected lane, the live source (FROM A); the main key, the LIT KEY (its window lit
 *   in the state colour, the next action's glyph on its face, a round rec lamp beside the light while it
 *   captures); STOP over the recessed CLR;
 * - the glass: this lane's strip of the one recessed pane (the waveform canvas, drawn by waveform.ts)
 *   with the count-in numeral, the well messages and the start line over it;
 * - the mix: FX and MUTE (latched, LED) and REV, UNDO, EDIT (momentary) in one row, then the level
 *   fader with its printed scale, ONE value window (level and pan) and the pan knob. Under 1200 px REV and
 *   UNDO fold into EDIT's menu and the knob into the window's pan field (`narrow`, below).
 *
 * A presentation layer: the audio engine and the per-track state machine live elsewhere, and the
 * waveform.ts rAF renderer draws each <canvas> by reading non-reactive looper getters. The lane's state
 * colour `--sc` (looper.css) lights the state word and the lit key's window; the wave is coloured by
 * waveform.ts. The selection is chrome: the strip one tone up, its head the brightest (looper.css).
 */

/** The state word under the lane number for each `LaneWord` (a rolling RETAKE's TAKE adds its pass
 * count). REC and OVERDUB need no mark of their own in the word: the lit key's rec lamp carries them
 * past greyscale (D14). */
const STATE_WORD: Record<Exclude<LaneWord, 'TAKE'>, string> = {
  EMPTY: 'EMPTY',
  RECORDING: 'REC',
  ARMED: 'ARMED',
  LISTENING: 'LISTEN',
  OVERDUBBING: 'OVERDUB',
  PLAYING: 'PLAYING',
  STOPPED: 'STOPPED',
  FADING: 'FADING',
  ENDING: 'ENDING',
  MUTED: 'MUTED',
};

// ---- the lit key's glyphs, one family: a ring round the next action. currentColor → the key's legend.
//   ◉ record  (EMPTY / ARMED: pressing starts a take)
//   ⊕ overdub (PLAYING / STOPPED: pressing starts an overdub)
//   ▣ end     (RECORDING / LISTENING / OVERDUBBING: pressing ends the live capture)
const IconRec = (): JSX.Element => (
  <svg class="lp-core__glyph" viewBox="0 0 24 24">
    <circle cx="12" cy="12" r="10" />
    <circle class="fill" cx="12" cy="12" r="4.6" />
  </svg>
);
const IconDub = (): JSX.Element => (
  <svg class="lp-core__glyph" viewBox="0 0 24 24">
    <circle cx="12" cy="12" r="10" />
    <path d="M12 7.2v9.6M7.2 12h9.6" />
  </svg>
);
const IconEnd = (): JSX.Element => (
  <svg class="lp-core__glyph" viewBox="0 0 24 24">
    <circle cx="12" cy="12" r="10" />
    <rect class="fill" x="8.2" y="8.2" width="7.6" height="7.6" rx="1.2" />
  </svg>
);

/** The fader's printed scale, in percent of the 0..150 travel: its ticks, and the 0 dB mark at 100. */
const FADER_TICKS = [0, 25, 50, 75, 125, 150];

function Fader(props: { index: number; disabled: boolean }) {
  // Production volume runs 0..1.5; preserve the 1.5 ceiling + 0 dB detent at 1.0.
  // The `.k-fader` recipe (a native <input type=range>) over a 0..150 integer domain (= vol × 100), its
  // scale printed under the slot (ticks and the 0 dB mark). The dB read-out is the value window's
  // (TrackLane). It shows its gesture's value until the engine has it (`mixGesture`), and builds on what
  // it shows. A double-click resets it to 0 dB, as the slot fader and the pan do.
  const vol = () => looper.mixShown(props.index, 'volume');
  const gesture = mixGesture(props.index, 'volume');

  const onInput = (e: Event) => {
    const input = e.target as HTMLInputElement;
    let v = Number(input.value) / 100;
    if (Math.abs(v - 1.0) < 0.05) v = 1.0; // 0 dB detent
    looper.setVolume(props.index, v);
    // Resync the DOM to the SHOWN value: inside the detent zone the snapped 1.0 equals the value already
    // shown, so Solid re-renders nothing and the native thumb would drift from the dB read-out for the
    // rest of the drag. Writing it back also makes the detent read as a magnetic snap on the thumb itself.
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
      <input
        class="k-fader lp-vbar"
        type="range"
        min="0"
        max="150"
        step="1"
        value={Math.round(vol() * 100)}
        disabled={props.disabled}
        aria-label={`Track ${props.index + 1} volume`}
        aria-valuetext={volumeDb(vol())}
        onInput={onInput}
        onKeyDown={onKeyDown}
        onKeyUp={gesture.onKeyUp}
        onPointerDown={gesture.onPointerDown}
        onLostPointerCapture={gesture.onLostPointerCapture}
        onDblClick={(e) => {
          // One send, and none when it already shows 0 dB. Resync the thumb as `onInput` does.
          if (props.disabled || vol() === 1.0) return;
          looper.setVolume(props.index, 1.0);
          e.currentTarget.value = String(Math.round(vol() * 100));
        }}
        onBlur={gesture.onBlur}
      />
      {/* the printed scale: each mark under the thumb's centre at its value (looper.css `--at`) */}
      <div class="lp-vbar__scale" aria-hidden="true">
        <For each={FADER_TICKS}>{(m) => <i class="lp-vbar__tick" style={{ '--at': m / 150 }} />}</For>
        <i class="lp-vbar__tick lp-vbar__unity" style={{ '--at': 100 / 150 }} />
        <span class="lp-vbar__zero" style={{ '--at': 100 / 150 }}>
          0
        </span>
      </div>
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

/** The knob's printed ring: eleven ticks from -135° to 135° and the centre mark at 12 o'clock, in a
 * 52-unit box round a 32-unit knob (looper.css sizes it with the knob). */
const KNOB_TICKS = Array.from({ length: 11 }, (_, i) => {
  const a = ((-135 + 27 * i) * Math.PI) / 180;
  const r0 = 18.5;
  const r1 = i === 0 || i === 10 ? 23.5 : 22.5;
  const f = (n: number) => n.toFixed(2);
  return { x1: f(26 + r0 * Math.sin(a)), y1: f(26 - r0 * Math.cos(a)), x2: f(26 + r1 * Math.sin(a)), y2: f(26 - r1 * Math.cos(a)), major: i === 0 || i === 10 };
});
const KnobRing = (): JSX.Element => (
  <svg class="lp-knob__ring" viewBox="0 0 52 52" aria-hidden="true">
    <For each={KNOB_TICKS}>{(t) => <line class={t.major ? 'is-major' : undefined} x1={t.x1} y1={t.y1} x2={t.x2} y2={t.y2} />}</For>
    <line class="is-centre" x1="26" y1="7.5" x2="26" y2="2.5" />
  </svg>
);

/**
 * The lane's pan: the `.k-knob` turned by the pan (-135° hard left, 135° hard right), driven by a native
 * range over -100..100 (percent of the way to a side) laid over it, so it drags, steps and speaks as a
 * slider. Folded (`narrow`), the knob goes and the range lies over the value window's pan field, where
 * the pan is printed. A drag snaps to the centre inside the detent; the arrow keys step by 1 and Page
 * Up/Down by 10 (no detent), Home and End go hard left and right, and 0 (while focused), a double-click
 * or an Alt-click centre it. It shows its gesture's value until the engine has it (`mixGesture`), as the
 * fader does.
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
      <KnobRing />
      <div class="k-knob lp-knob" style={{ '--a': `${pan() * 135}deg` }} aria-disabled={props.disabled ? 'true' : undefined} />
      <input
        class="lp-pan"
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
  );
}

/** A lane volume's read-out in the value window: the figure, its unit smaller (`volumeDb`'s text whole,
 * so the read-out still says `+0.0 dB`). */
function DbText(props: { text: string }) {
  const parts = () => /^(.*?)( dB)?$/.exec(props.text) ?? [props.text, props.text];
  return (
    <>
      {parts()[1]}
      <Show when={parts()[2]}>
        <span class="lp-unit">{parts()[2]}</span>
      </Show>
    </>
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
  fxRef: (el: HTMLButtonElement) => void;
  returnFocus?: (el: HTMLElement | undefined) => void;
}) {
  const track = looper.track(props.index);
  const state = () => track().state;
  const reversed = () => track().reversed;
  // REV and UNDO hold their columns on every lane: disabled, with the engine's reason as the title, where
  // a press would be refused (gates.ts).
  const rev = createMemo(() => reverseGate(props.index));
  const undo = createMemo(() => undoGate(props.index));
  // The lane's display state, word, well message and count-in numeral: `lane-state.ts`, shared with the
  // stage view. An EMPTY lane has no well message: the lit key's ◉ already says "press to record"; the
  // spoken 'record' action lives on the key's aria-label. A guitar-first install's empty
  // looper adds one start line on the selected lane (`startLine`), naming the key and the pedal, until
  // its first loop.
  const { displayState, word, stopping, fading, muted, clearArmed, cue, wellMsg, wellCount, startLine } =
    createLaneView(props.index);
  const fxSelected = () => props.fxTrack === props.index;
  const isEmpty = () => state() === 'EMPTY';
  // Any of the lane's effects engaged in the mix the engine applied (the feed's, never a control's
  // overlay), as an input-effect plate's lamp reads its own send. The FX key's LED shows it; its lit legend says the drawer is open.
  const fxEngaged = () => !isEmpty() && looper.fxState(props.index).some((s) => !s.bypassed);
  const stateWord = () => {
    const w = word();
    return w === 'TAKE' ? `TAKE ${track().retakePass}` : STATE_WORD[w];
  };
  // What a take on this lane would hear: the live slots (GO LIVE), named on the selected lane only, the
  // one a press or a pedal records onto. Nothing live: no source.
  const source = () => {
    if (looper.selectedTrack() !== props.index) return '';
    const [a, b] = inputArmed();
    return a && b ? 'FROM A+B' : a ? 'FROM A' : b ? 'FROM B' : '';
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
      // STOPPED: recDubGate always refuses it, so the core's title says why.
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
  // A control's aria-label names its action; a refusal goes in its title (its description), as on ↺ REV.
  const recGate = createMemo(() => recDubGate(props.index));
  const playGate = createMemo(() => playStopGate(props.index));
  const playStopGlyph = () => (state() === 'STOPPED' || state() === 'EMPTY' ? '▶' : '■');
  const playStopAction = () => (stopping() ? 'stop now' : playStopGlyph() === '▶' ? 'play' : 'stop');

  // Two-step clear: a take is irreversible, so the first press only ARMS (CLR? lit) for a short window;
  // a second press within it actually clears. No blocking window.confirm, which keeps the workflow fast.
  // It is the engine's CLEAR, one arm with the key's and a pedal's: either hand may arm and the other
  // confirm, and the key reads the engine's arm off the feed (`clearArmed`).
  let clrEl: HTMLButtonElement | undefined;

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
  // A FADE reaches the wave plain (invariant 6): the frame its stop lands on while the lane fades.
  createEffect(() => setLaneFade(props.index, fading() ? track().stopAt : null));

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
        'is-fading': fading(),
        'is-cued': cue() !== '',
      }}
      data-state={DATA_STATE[displayState()]}
      data-muted={muted() ? 'true' : undefined}
      role="group"
      aria-label={`Track ${props.index + 1}`}
      aria-current={looper.selectedTrack() === props.index ? 'true' : undefined}
      onPointerDown={(e) => {
        pressed = true;
        haltScroll(laneEl?.parentElement);
        // A selection is a looper press to the engine and disarms CLEAR, so CLR's confirming press sends
        // none (it would only arm the lane again); every other press on the lane selects it.
        const confirming = clearArmed() && e.target instanceof Node && clrEl?.contains(e.target) === true;
        if (!confirming) looper.selectTrack(props.index);
      }}
    >
      {/* the head: the number over the state word (and the live source) · the lit key · STOP over CLR */}
      <div class="lp-lane__left">
        <div class="lp-lane__idbox">
          <span class="lp-lane__num">{String(props.index + 1)}</span>
          <span class="lp-lane__state">{stateWord()}</span>
          <Show when={source()}>
            <span class="lp-lane__src" title="What a take on this lane hears: the slots with live input">
              {source()}
            </span>
          </Show>
        </div>

        <button
          class="k-key lp-core"
          disabled={!recGate().ok}
          onClick={(e) => {
            // A pointer press selected the lane on pointerdown; a keyboard one (detail 0) selects here,
            // so the selected track's keys then act on the lane this take started on.
            if (e.detail === 0) looper.selectTrack(props.index);
            void looper.recDub(props.index);
          }}
          aria-label={`Track ${props.index + 1} ${recDubAria()}`}
          title={why(recGate()) ?? sentence(recDubAria())}
        >
          {/* the key's window: its light in the state colour, and while it captures the round rec lamp
              beside it (D14's carrier past greyscale) */}
          <span class="lp-core__win" aria-hidden="true">
            <i class="lp-core__lamp" />
            <i class="lp-core__bar" />
          </span>
          {coreGlyph()}
        </button>

        <div class="lp-lane__pair">
          <button
            class="k-key lp-pb lp-pb--play"
            disabled={!playGate().ok}
            onClick={() => looper.playStop(props.index)}
            aria-label={`Track ${props.index + 1} ${playStopAction()}`}
            title={
              fading()
                ? 'Fading out. Press to stop now.'
                : stopping()
                  ? 'Stopping at loop end. Press again to stop now.'
                  : why(playGate())
            }
          >
            <i class="lp-glyph" classList={{ 'lp-glyph--play': playStopGlyph() === '▶' && !stopping() }} aria-hidden="true" />
            {stopping() ? 'NOW' : playStopGlyph() === '▶' ? 'PLAY' : 'STOP'}
          </button>
          <button
            ref={clrEl}
            class="k-key k-key--clr lp-pb lp-pb--clr"
            classList={{ 'k-key--armed': clearArmed(), 'is-armed': clearArmed() }}
            disabled={isEmpty()}
            onClick={() => runAction('clear', props.index)}
            onKeyDown={noEnterRepeat}
            aria-label={
              clearArmed()
                ? `Track ${props.index + 1} clear, press again to confirm`
                : `Track ${props.index + 1} clear`
            }
          >
            {clearArmed() ? 'CLR?' : 'CLR'}
          </button>
        </div>
      </div>

      {/* the glass: this lane's strip of the one recessed pane (waveform + playhead drawn by waveform.ts) */}
      <div class="lp-lane__well">
        <canvas ref={canvasEl} />
        <Show when={wellMsg()}>
          <div class="lp-lane__wellmsg" classList={{ 'is-cue': cue() !== '' }}>
            <Show when={wellCount() > 0}>
              <span class="lp-lane__count" aria-hidden="true">
                {wellCount()}
              </span>
            </Show>
            <span class="lp-lane__cue">{wellMsg()}</span>
          </div>
        </Show>
        <Show when={startLine()}>
          <div class="lp-lane__wellmsg lp-lane__start">
            <span class="lp-lane__start-line">
              <kbd class="lp-lane__key">SPACE</kbd> or your pedal records here
            </span>
          </div>
        </Show>
      </div>

      {/* the mix: FX and MUTE (latched) | REV, UNDO, EDIT (momentary), each at one x on every lane (a key
          that does not apply is disabled, never absent); under them the fader, the value window and the
          pan. Folded (`narrow`, shared.ts), REV and UNDO live in EDIT's menu. */}
      <div class="lp-lane__right">
        <div class="lp-lane__mods">
          <button
            ref={props.fxRef}
            class="k-key k-key--latch lp-pb lp-pb--fx"
            classList={{ 'is-open': fxSelected(), 'has-fx': fxEngaged() }}
            disabled={isEmpty()}
            onClick={() => props.onToggleFx(props.index)}
            aria-label={`Track ${props.index + 1} FX`}
            aria-pressed={fxSelected()}
          >
            {/* the LED: any of the lane's effects engaged; the lit legend: its drawer open */}
            <i class="k-led" classList={{ 'is-on': fxEngaged() }} />
            FX
          </button>
          <button
            class="k-key k-key--latch lp-pb lp-pb--mute"
            disabled={isEmpty()}
            onClick={() => looper.toggleMute(props.index)}
            aria-label={`Track ${props.index + 1} mute`}
            aria-pressed={looper.trackMuted(props.index)}
          >
            <i class="k-led" />
            MUTE
          </button>

          {/* Per-track reverse: toggles forward/reversed; the swap lands on the next loop boundary
              (click-free, like undo). */}
          <button
            class="k-key k-key--mo lp-pb lp-pb--rev"
            disabled={!rev().ok}
            classList={{ 'is-on': reversed() }}
            onClick={() => looper.reverse(props.index)}
            aria-label={`Track ${props.index + 1} reverse`}
            aria-pressed={reversed()}
            title={why(rev()) ?? 'Reverse / forward (toggle)'}
          >
            REV
          </button>

          {/* One-level undo of the last overdub or trim; a second press redoes it. */}
          <button
            class="k-key k-key--mo lp-pb lp-pb--undo"
            disabled={!undo().ok}
            onClick={() => looper.undoLastOverdub(props.index)}
            aria-label={`Track ${props.index + 1} undo or redo the last overdub or trim`}
            title={why(undo()) ?? 'Undo / redo the last overdub or trim'}
          >
            UNDO
          </button>

          {/* COPY, WIDE and TRIM (and, folded, REV and UNDO) behind one key (`Edit.tsx`). */}
          <Edit index={props.index} returnFocus={props.returnFocus} />
        </div>

        {/* the mix line: the fader, ONE value window (the level and the pan), the pan knob */}
        <div class="lp-lane__mix">
          <Fader index={props.index} disabled={isEmpty()} />
          <span class="k-win lp-lane__win" classList={{ 'is-off': isEmpty() }}>
            <span class="lp-vdb">
              <DbText text={volumeDb(looper.mixShown(props.index, 'volume'))} />
            </span>
            <span class="lp-pan__val">{panText(looper.mixShown(props.index, 'pan'))}</span>
          </span>
          <Pan index={props.index} disabled={isEmpty()} />
        </div>
      </div>
    </div>
  );
}

/** `returnFocus` is the transport keys' (app.tsx), for a keyboard close of a lane's popover or FX drawer. */
export function Looper(props: { returnFocus?: (el: HTMLElement | undefined) => void }) {
  const master = () => looper.masterLengthFrames();
  const hasMaster = () => master() > 0;
  // The lanes' bar grid counts the meter's beats; the draw loop reads them plain (invariant 6).
  createEffect(() => setGridBeats(clock.meter().beats));

  // Master length as a musician-readable "N bar · S.S s" — used only by the SR live announcement now
  // (the visible loop readout lives in the command bar's ring-dial).
  const masterLabel = () => {
    const m = master();
    if (m <= 0) return '—';
    const bars = masterBars(m, clock.bpm(), sampleRate(), clock.meter().beats);
    const secs = m / sampleRate();
    return `${bars} bar${bars > 1 ? 's' : ''} · ${secs.toFixed(1)} s`;
  };

  // One FX drawer for the whole looper: the index of the selected track (its drawer docks under that
  // lane), or null.
  const [fxTrack, setFxTrack] = createSignal<number | null>(null);
  const toggleFx = (i: number) => setFxTrack((cur) => (cur === i ? null : i));
  // Each lane's FX key: the drawer's ✕ pressed by key (a click with detail 0, as
  // `src/app/transport-keys.ts` reads it) unmounts the focused button, so focus goes back to the key
  // that opened it, on the app's behalf so Space there still arms REC. A pointer close leaves focus
  // where the transport keys' blur put it.
  const fxPills: (HTMLButtonElement | undefined)[] = [];
  const closeFx = (i: number, e: MouseEvent) => {
    setFxTrack(null);
    if (e.detail === 0) props.returnFocus?.(fxPills[i]);
  };

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
          docked directly under it (the lane's own drawer): one row of title, the FX and the close key.
          The drawer is a SIBLING of the lane, so opening it never remounts the lane's waveform canvas. */}
      <div class="lp__lanes">
        <For each={Array.from({ length: looper.trackCount }, (_, i) => i)}>
          {(i) => (
            <>
              <TrackLane
                index={i}
                fxTrack={fxTrack()}
                onToggleFx={toggleFx}
                fxRef={(el) => (fxPills[i] = el)}
                returnFocus={props.returnFocus}
              />
              <Show when={fxTrack() === i}>
                <div class="lp-drawer" role="group" aria-label={`FX, Track ${i + 1}`}>
                  <span class="lp-drawer__title">FX · Track {i + 1}</span>
                  <FxPanel index={i} />
                  <button
                    class="k-key k-key--mo lp-drawer__close"
                    onClick={(e) => closeFx(i, e)}
                    aria-label="Close FX panel"
                    title="Close FX panel"
                  >
                    ✕
                  </button>
                </div>
              </Show>
            </>
          )}
        </For>
      </div>
    </div>
  );
}
