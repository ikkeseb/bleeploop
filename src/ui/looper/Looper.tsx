import { For, Show, createEffect, createMemo, createSignal, onCleanup, onMount, type JSX } from 'solid-js';
import { looper, type TrackState } from '../../audio/looper/looper';
import { clock } from '../../audio/clock';
import { framesPerBar } from '../../audio/quantize';
import { engine } from '../../audio/engine';
import { registerLane, unregisterLane } from './waveform';
import { announceLooper, liveMsg, playStopGate, recDubGate } from './gates';
import { FxPanel } from './FxPanel';
import './looper.css';

/**
 * The looper UI: five full-width horizontal LANES stacked in the looper zone (composition binding:
 * `src/ui/AGENTS.md`). Each lane, left→right: an identity box
 * (mono track number + state word), the round core REC/DUB gesture, a stacked PLAY-STOP/CLR pair,
 * the recessed wave well (holding the waveform canvas + its playhead), and a right cluster of
 * FX/MUTE/undo/reverse pills over a horizontal volume slider.
 *
 * The audio engine, the waveform.ts rAF renderer (which draws each <canvas> by reading non-reactive
 * looper getters), and the per-track state machine are UNCHANGED — this is a presentation layer.
 * State colour is rationed to the lane's state token `--sc` (identity word, core ring/glow, left-edge
 * bleed); the waveform is coloured independently by waveform.ts via the global --rec/--dub/--play
 * tokens.
 */

/**
 * Display states add 'ARMED' — a later track that pressed REC but is still waiting for the master
 * loop boundary before its take begins. The engine reports this as RECORDING + `armed`; surfacing it
 * as its own state stops the lane from reading "recording" (red) for up to a full loop while nothing
 * is actually being kept (UX: the single most-bitten gap on every overdub), so it gets its own amber
 * dashed-ring-pulse treatment (data-state="armed") here.
 * LISTENING is the AUTO REC sibling: first-track REC is waiting for input rather than a known grid edge.
 */
type DisplayState = TrackState | 'ARMED' | 'LISTENING';

/** Map a display state to the lane's data-state vocab (drives --sc + core/identity/well treatment). */
const DATA_STATE: Record<DisplayState, string> = {
  EMPTY: 'empty',
  RECORDING: 'rec',
  ARMED: 'armed',
  LISTENING: 'listening',
  OVERDUBBING: 'dub',
  PLAYING: 'play',
  STOPPED: 'stop',
};

/** The identity-box state word, in the lane's state colour. */
const STATE_WORD: Record<DisplayState, string> = {
  EMPTY: 'EMPTY',
  RECORDING: '● REC',
  ARMED: 'ARMED',
  LISTENING: 'LISTEN',
  OVERDUBBING: 'OVERDUB',
  PLAYING: 'PLAYING',
  STOPPED: 'STOPPED',
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
  const vol = () => looper.trackVolume(props.index);
  const frac = () => Math.max(0, Math.min(1, vol() / 1.5));
  const dbStr = () => {
    const v = vol();
    if (v <= 0.0001) return '−∞';
    const db = 20 * Math.log10(v);
    return (db >= 0 ? '+' : '−') + Math.abs(db).toFixed(1) + ' dB';
  };

  const onInput = (e: Event) => {
    const input = e.target as HTMLInputElement;
    let v = Number(input.value) / 100;
    if (Math.abs(v - 1.0) < 0.05) v = 1.0; // 0 dB detent
    looper.setVolume(props.index, v);
    // Resync the DOM to the STORED value: inside the detent zone the snapped 1.0 equals the already-
    // stored signal value, so Solid re-renders nothing and the native thumb would drift from the
    // fill/dB readout for the rest of the drag. Writing it back also makes the detent read as a
    // magnetic snap on the thumb itself.
    input.value = String(Math.round(looper.trackVolume(props.index) * 100));
  };
  const onKeyDown = (e: KeyboardEvent) => {
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
        />
      </div>
      <span class="lp-vdb">{dbStr()}</span>
    </div>
  );
}

function TrackLane(props: {
  index: number;
  fxTrack: number | null;
  onToggleFx: (i: number) => void;
}) {
  const track = looper.track(props.index);
  const state = () => track().state;
  const armed = () => track().armed;
  const autoArmed = () => track().autoArmed;
  const canUndo = () => track().canUndo;
  const canReverse = () => track().canReverse;
  const reversed = () => track().reversed;
  const stopping = () => track().stopAt !== null;
  const hasFreeLane = () => {
    for (let j = 0; j < looper.trackCount; j++) if (looper.track(j)().state === 'EMPTY') return true;
    return false;
  };
  // The state the UI shows: RECORDING-but-armed becomes its own 'ARMED' (waiting-for-downbeat) state.
  // Keep this memoized so unrelated public-track changes cannot rebuild coreGlyph's fresh JSX.
  const displayState = createMemo((): DisplayState => {
    if (state() !== 'RECORDING') return state();
    if (autoArmed()) return 'LISTENING';
    return armed() ? 'ARMED' : 'RECORDING';
  });
  const fxSelected = () => props.fxTrack === props.index;
  const isEmpty = () => state() === 'EMPTY';
  const muted = () => looper.trackMuted(props.index);
  // The state word says what the lane SOUNDS like: a muted take that is playing or stopped reads
  // MUTED (the loop still runs — the playhead keeps moving); a live capture keeps its own word so
  // REC/OVERDUB is never hidden behind a mute. ENDING (stop at loop end) outranks both.
  const stateWord = () =>
    stopping()
      ? 'ENDING'
      : muted() && (displayState() === 'PLAYING' || displayState() === 'STOPPED')
        ? 'MUTED'
        : track().retakePass > 0
          ? `TAKE ${track().retakePass}` // a rolling RETAKE counts its passes
          : STATE_WORD[displayState()];
  // Only ARMED shows a well message (waiting for the downbeat before the take begins). EMPTY shows nothing
  // — the bright ● core already says "press to record". The spoken 'record' action lives on the core
  // button's aria-label, untouched.
  // A FIRST take (no master yet) is armed behind the forced count-in → the well counts it down big
  // (4-3-2-1, the numeral is clock.countLeft); a LATER take waits for the loop boundary → plain text.
  const wellMsg = () => {
    if (stopping()) return 'STOPPING AT LOOP END';
    if (displayState() === 'ARMED') return looper.masterLengthFrames() > 0 ? 'WAITING FOR DOWNBEAT' : 'COUNT-IN';
    if (displayState() === 'LISTENING') return 'WAITING FOR INPUT';
    return '';
  };
  const wellCount = () => (displayState() === 'ARMED' && looper.masterLengthFrames() === 0 ? clock.countLeft() : 0);

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
  onCleanup(() => unregisterLane(props.index));

  return (
    <div
      class="lp-lane"
      classList={{ 'is-selected': looper.selectedTrack() === props.index, 'is-stopping': stopping() }}
      data-state={DATA_STATE[displayState()]}
      data-muted={muted() ? 'true' : undefined}
      role="group"
      aria-label={`Track ${props.index + 1}`}
      aria-current={looper.selectedTrack() === props.index ? 'true' : undefined}
      onPointerDown={() => looper.selectTrack(props.index)}
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
          onClick={() => void looper.recDub(props.index)}
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
            title={stopping() ? 'Stopping at loop end. Press again to stop now.' : playGate().ok ? undefined : playStopLabel()}
          >
            {playStopGlyph()} {stopping() ? 'NOW' : playStopGlyph() === '▶' ? 'PLAY' : 'STOP'}
          </button>
          <button
            class="lp-pb lp-pb--clr"
            classList={{ 'is-armed': clr.armed() }}
            disabled={isEmpty()}
            onClick={clr.trigger}
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
          <div class="lp-lane__wellmsg">
            <Show when={wellCount() > 0}>
              <span class="lp-lane__count" aria-hidden="true">
                {wellCount()}
              </span>
            </Show>
            {wellMsg()}
          </div>
        </Show>
      </div>

      {/* right cluster: FX / MUTE / undo / reverse pills over the horizontal volume slider. */}
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
            onClick={() => looper.setMute(props.index, !looper.trackMuted(props.index))}
            aria-label={`Track ${props.index + 1} mute`}
            aria-pressed={looper.trackMuted(props.index)}
          >
            MUTE
          </button>

          {/* One-level undo of the last overdub — appears only once a take has been dubbed (its own
              affordance = "you just layered, here's undo"). Toggles undo/redo. */}
          <Show when={canUndo()}>
            <button
              class="lp-pb lp-pb--undo"
              disabled={stopping()}
              onClick={() => looper.undoLastOverdub(props.index)}
              aria-label={`Track ${props.index + 1} undo or redo the last overdub`}
              title="Undo / redo the last overdub"
            >
              ↶ DUB
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
              ⧉ COPY
            </button>
          </Show>
        </div>

        <Fader index={props.index} disabled={isEmpty()} />
      </div>
    </div>
  );
}

/**
 * Bar count of a committed master loop from its length in frames. This is the shared bar-math: the SR
 * loop-length label below AND the waveform bar grid (`waveform.ts`) both derive their bar count from
 * this ONE function, so the spoken length and the drawn grid can never disagree. Returns 0 when no
 * master is defined yet (loop length still unknown → no grid). BPM is locked once a master commits, so
 * the caller can safely pass a snapshot of `clock.bpm()`.
 */
export function masterBars(masterFrames: number, bpm: number, sampleRate: number): number {
  if (masterFrames <= 0) return 0;
  return Math.max(1, Math.round(masterFrames / framesPerBar(bpm, sampleRate)));
}

/**
 * True when any track is currently in one of the given states — the shared "scan all lanes" idiom
 * behind the command bar's ▶/■ ALL (`live` = play/overdub/record) and the single-recorder REC/DUB gate
 * (`capturing` = record/overdub).
 */
export function anyTrackIn(...states: TrackState[]): boolean {
  return Array.from({ length: looper.trackCount }, (_, i) => looper.track(i)().state).some((s) =>
    states.includes(s),
  );
}

/**
 * Two-step confirm latch. The first `trigger()` only ARMS (opens a `windowMs` window and returns); a
 * second within it runs `action` and disarms. The window auto-closes. Shared by the per-track CLR and
 * the command bar's ✕ ALL so "press twice to destroy a take" behaves identically — no blocking confirm.
 * Registers its own `onCleanup`, so call it during component setup.
 */
export function createTwoStepConfirm(action: () => void, windowMs = 2500) {
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

export function Looper() {
  const master = () => looper.masterLengthFrames();
  const hasMaster = () => master() > 0;

  // Master length as a musician-readable "N bar · S.S s" — used only by the SR live announcement now
  // (the visible loop readout lives in the command bar's ring-dial).
  const masterLabel = () => {
    const m = master();
    if (m <= 0) return '—';
    const bars = masterBars(m, clock.bpm(), engine.ctx.sampleRate);
    const secs = m / engine.ctx.sampleRate;
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

  // The capture ring needs SharedArrayBuffer, which needs crossOriginIsolated (COOP/COEP). Without it
  // looper.init() bails and every looper button is silently dead — so say so loudly instead.
  const looperUnavailable = self.crossOriginIsolated !== true;

  // Screen-reader status line: operational transitions are otherwise silent to AT. A polite live region
  // announces record/arm/overdub starts + the master-loop resolution, diffed so it fires on transitions.
  let prevSnap = Array.from({ length: looper.trackCount }, () => ({
    state: 'EMPTY' as TrackState,
    armed: false,
    autoArmed: false,
    stopAt: null as number | null,
  }));
  let prevHasMaster = false;
  let prevSelected = looper.selectedTrack();
  createEffect(() => {
    const cur = Array.from({ length: looper.trackCount }, (_, i) => {
      const t = looper.track(i)();
      return { state: t.state, armed: t.armed, autoArmed: t.autoArmed, stopAt: t.stopAt };
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
      if (c.stopAt !== null && p.stopAt === null) msg = `Track ${i + 1} stopping at loop end. Press stop again to stop now.`;
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
      <Show when={looperUnavailable}>
        <div class="lp__unavailable" role="alert">
          ⚠ Looper disabled. This browser isn’t <code>crossOriginIsolated</code> (no SharedArrayBuffer).
          Synths still work, but recording won’t. Try Chrome/Edge, hard-reload, and check that COOP/COEP
          headers are served (the system lamp in the command bar reads amber when isolation is missing).
        </div>
      </Show>

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
              <TrackLane index={i} fxTrack={fxTrack()} onToggleFx={toggleFx} />
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
