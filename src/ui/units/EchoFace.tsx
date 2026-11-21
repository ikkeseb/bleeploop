import { For, Show, createEffect, createSignal, on, onCleanup, onMount } from 'solid-js';
import type { UnitState } from '../../platform';
import { clock } from '../state/audio';
import { setUnitParam } from '../state/chain';
import { FaceKnob, KnobBody, UnitFace, knobStyle, paramOf, type KnobDef } from './UnitFace';
import { echoGeometry, registerEchoGlass, type EchoView } from './echo-loop';
import { ECHO_FEEDBACK_MAX, ECHO_STEPS, barSeconds, stepOfIndex, stepSeconds, type EchoTempo } from './echo-time';
import './echo-face.css';

/**
 * The ECHO's own face (the accepted face B, "the tape") on the family's frame (`UnitFace.tsx`: its home
 * under the plate, the move and the resize, the name, the power switch, the close). The glass is the delay
 * line drawn as tape (`echo-loop.ts`): the unit's output running right to left from NOW over the last two
 * bars, the read heads at each repeat, lit as loud as it, the ruler counting beats back from NOW, OUT's
 * ladders. Under it TIME, the hero: a four-position selector ordered by length (`echo-time.ts` maps it
 * to the engine's index), each position with its length at the tempo; then FEEDBACK (0 to 95 %) and
 * LEVEL. Every control sends `SetUnitParam` and shows the feed's value (a knob follows the hand while a drag
 * holds it). On the narrow face (900x382) the
 * glass is shorter, the knobs smaller, TIME's positions print their note values alone (its length stays
 * in its read-out), every legend at its full size.
 */

const KNOBS: readonly KnobDef[] = [
  { param: 'echoFeedback', label: 'FEEDBACK', name: 'feedback', def: 0.4, max: ECHO_FEEDBACK_MAX, wide: { x: 650, y: 424, size: 92 }, narrow: { x: 610, y: 287, size: 60 } },
  { param: 'echoLevel', label: 'LEVEL', name: 'level', def: 0.5, wide: { x: 806, y: 424, size: 92 }, narrow: { x: 770, y: 287, size: 60 } },
];
/** TIME's place and size, full and narrow; its positions' dots and labels at these radii past its edge. */
const TIME = {
  wide: { x: 398, y: 386, size: 172, dots: 14, labels: 38 },
  narrow: { x: 340, y: 284, size: 84, dots: 10, labels: 30 },
};
/** TIME's positions round the knob, in the face's order (degrees from up). */
const STEP_ANGLES = [-112, -40, 40, 112];
/** The engine's default TIME (1/8, `InputSendParam::EchoTime`), FEEDBACK and LEVEL. */
const DEF = { time: 1, feedback: 0.4, level: 0.5 };
/** The glass inside its bezel, full and narrow (CSS pixels). */
const GLASS = { w: 852, wide: 232, narrow: 178 };

const ms = (s: number): number => Math.round(s * 1000);

export function EchoFace(props: { unit: () => UnitState; slotLetter: () => string; close: (keyboard: boolean) => void }) {
  const tempo = (): EchoTempo => {
    const m = clock.meter();
    return { bpm: clock.bpm(), beats: m.beats, num: m.num, den: m.den };
  };
  const step = () => stepOfIndex(paramOf(props.unit(), 'echoTime', DEF.time));
  // the draw loop's view: written here, read plain on its frames (invariant 6)
  const view: EchoView = { step: step(), feedback: DEF.feedback, level: DEF.level, tempo: tempo() };
  createEffect(() => {
    view.step = step();
    view.feedback = paramOf(props.unit(), 'echoFeedback', DEF.feedback);
    view.level = paramOf(props.unit(), 'echoLevel', DEF.level);
    view.tempo = tempo();
  });
  return (
    <UnitFace unit={props.unit} slotLetter={props.slotLetter} close={props.close} kind="echo" name="ECHO">
      {(narrow) => (
        <>
          <Glass unit={props.unit().id} view={view} narrow={narrow()} step={step()} tempo={tempo()} />
          <TimeKnob unit={props.unit} narrow={narrow()} step={step()} tempo={tempo()} />
          <For each={KNOBS}>{(k) => <FaceKnob def={k} narrow={narrow} unit={props.unit} unitName="ECHO" />}</For>
        </>
      )}
    </UnitFace>
  );
}

/** The glass: its canvas (the loop draws the tape) and its print: NOW, the bars back, TIME's span in ms,
 * the channels, OUT. */
function Glass(props: { unit: number; view: EchoView; narrow: boolean; step: number; tempo: EchoTempo }) {
  let canvas!: HTMLCanvasElement;
  onMount(() => onCleanup(registerEchoGlass(canvas, props.unit, props.view)));
  const G = () => echoGeometry(GLASS.w, props.narrow ? GLASS.narrow : GLASS.wide);
  const span = () => 2 * barSeconds(props.tempo);
  const X = (ago: number) => G().X1 - (ago / span()) * (G().X1 - G().X0);
  const T = () => stepSeconds(props.tempo, props.step);
  // the span's read-out over TIME's bracket, kept clear of OUT
  const spanAt = () => Math.min((X(T()) + G().X1) / 2, G().X1 - 22);
  return (
    <div class="uface__bezel">
      <div class="uface__glass">
        <canvas ref={canvas} class="uface__canvas echo__canvas" aria-hidden="true" />
        <span class="echo__ago echo__now" style={{ left: `${G().X1}px`, top: `${G().print}px` }} aria-hidden="true">
          NOW
        </span>
        <For each={[1, 2]}>
          {(bars) => (
            <span class="echo__ago" style={{ left: `${X(bars * barSeconds(props.tempo))}px`, top: `${G().print}px` }} aria-hidden="true">
              −{bars}
              <span class="uface__u">{bars === 1 ? 'bar' : 'bars'}</span>
            </span>
          )}
        </For>
        <span class="echo__span" style={{ left: `${spanAt()}px`, top: `${G().spanTop}px` }} title="the time between repeats">
          {ms(T())}
          <span class="uface__u">ms</span>
        </span>
        <span class="echo__ch" style={{ left: `${G().X0 - 22}px`, top: `${G().lTop}px` }} aria-hidden="true">
          L
        </span>
        <span class="echo__ch" style={{ left: `${G().X0 - 22}px`, top: `${G().rTop}px` }} aria-hidden="true">
          R
        </span>
        <span class="echo__out" style={{ left: `${G().MX - 1}px`, top: `${G().outTop}px` }} aria-hidden="true">
          OUT
        </span>
        <span class="echo__ch" style={{ left: `${G().MX + 6}px`, top: `${G().print}px` }} aria-hidden="true">
          L
        </span>
        <span class="echo__ch" style={{ left: `${G().MX + 21}px`, top: `${G().print}px` }} aria-hidden="true">
          R
        </span>
      </div>
    </div>
  );
}

/** Pixels of vertical drag a step of TIME; a mouse wheel's notch is one step. */
const STEP_PX = 36;
const WHEEL_STEP = 50;
/** How long a step sent is the base of the next one before the feed has it (ms), each one by its own age:
 * the feed answers within frames, and a step it never reports (refused, or another's since) stops counting. */
const PENDING_MS = 500;
/** The most steps held awaiting the feed: a burst past it drops the oldest. */
const PENDING_MAX = 8;
/** TIME, the hero: a stepped knob over four positions, by length, each printed round it with its length at
 * the tempo, the one it points at lit; a position is pressable (by the pointer: the slider is TIME's one
 * focus). A slider to assistive tech and the keyboard (its value the face's position, its text the note
 * value and its length). It shows the feed's step, the hand's while a drag holds it. Each step sent is the
 * base of the next until the feed reports it, so a drag that turns back before the feed answers sends its
 * way back, and keys or a wheel pressed quickly count from where the last left it. */
function TimeKnob(props: { unit: () => UnitState; narrow: boolean; step: number; tempo: EchoTempo }) {
  const geo = () => TIME[props.narrow ? 'narrow' : 'wide'];
  const [held, setHeld] = createSignal<number | null>(null);
  const pos = () => held() ?? props.step;
  const enabled = () => props.unit().state === 'seated';
  // the steps sent and not yet reported, each with its time (aged on its own, never by a later send)
  let pending: { q: number; at: number }[] = [];
  let lastSeen = props.step;
  createEffect(
    on(
      () => props.step,
      (s) => {
        // a new report of the same step (a chain frame with a new unit object) answers nothing
        if (s === lastSeen) return;
        lastSeen = s;
        const i = pending.findIndex((p) => p.q === s);
        if (i >= 0) pending.splice(0, i + 1);
      },
      { defer: true },
    ),
  );
  /** Where the engine will stand once what was sent lands: a drag's last request for the whole drag,
   * otherwise the newest step sent that is still young, else the feed's. */
  const committed = (): number => {
    if (drag?.last != null) return drag.last;
    const now = performance.now();
    pending = pending.filter((p) => now - p.at < PENDING_MS);
    return pending.length > 0 ? pending[pending.length - 1].q : props.step;
  };
  const send = (p: number): number => {
    const q = Math.min(ECHO_STEPS.length - 1, Math.max(0, Math.round(p)));
    if (q !== committed()) {
      setUnitParam(props.unit().id, 'echoTime', ECHO_STEPS[q].index);
      pending.push({ q, at: performance.now() });
      if (pending.length > PENDING_MAX) pending.shift();
      if (drag) drag.last = q;
    }
    return q;
  };
  const length = (p: number) => ms(stepSeconds(props.tempo, p));
  let drag: { id: number; y: number; from: number; el: HTMLElement; last: number | null } | null = null;
  const letGo = (returnToFrame = false) => {
    const el = drag?.el;
    drag = null;
    setHeld(null);
    if (returnToFrame && el && document.activeElement === el) {
      el.closest<HTMLElement>('.uface')?.focus({ preventScroll: true });
    }
  };
  createEffect(() => {
    if (!enabled()) letGo(true);
  });
  onCleanup(() => letGo(false));
  const onDown = (e: PointerEvent) => {
    if (!enabled() || e.button !== 0) return;
    e.preventDefault();
    const el = e.currentTarget as HTMLElement;
    el.focus({ preventScroll: true });
    el.setPointerCapture(e.pointerId);
    drag = { id: e.pointerId, y: e.clientY, from: committed(), el, last: null };
    setHeld(drag.from);
  };
  const onMove = (e: PointerEvent) => {
    if (!drag || drag.id !== e.pointerId || !enabled()) return;
    const p = Math.min(ECHO_STEPS.length - 1, Math.max(0, drag.from + Math.round((drag.y - e.clientY) / STEP_PX)));
    if (p !== held()) setHeld(send(p));
  };
  const onUp = (e: PointerEvent) => {
    if (drag?.id === e.pointerId) letGo(true);
  };
  const onKey = (e: KeyboardEvent) => {
    if (!enabled()) return;
    const by = { ArrowUp: 1, ArrowRight: 1, PageUp: 1, ArrowDown: -1, ArrowLeft: -1, PageDown: -1 }[e.key];
    if (by !== undefined) send(committed() + by);
    else if (e.key === 'Home') send(0);
    else if (e.key === 'End') send(ECHO_STEPS.length - 1);
    else return;
    e.preventDefault();
    e.stopPropagation();
  };
  // a wheel's notches, however many one event carries, are one send; lines and pages normalize to pixels
  let wheelAcc = 0;
  const wheel = (e: WheelEvent) => {
    if (!enabled()) return;
    e.preventDefault();
    const dy =
      e.deltaMode === WheelEvent.DOM_DELTA_LINE
        ? e.deltaY * WHEEL_STEP
        : e.deltaMode === WheelEvent.DOM_DELTA_PAGE
          ? e.deltaY * WHEEL_STEP * ECHO_STEPS.length
          : e.deltaY;
    wheelAcc += dy;
    const n = Math.trunc(wheelAcc / WHEEL_STEP);
    if (n === 0) return;
    wheelAcc -= n * WHEEL_STEP;
    send(committed() - n);
  };
  const at = (r: number, deg: number) => {
    const q = (deg * Math.PI) / 180;
    return { left: `${(r * Math.sin(q)).toFixed(2)}px`, top: `${(-r * Math.cos(q)).toFixed(2)}px` };
  };
  return (
    <div class="uface__ctl uface__ctl--hero echo__time" style={{ '--x': `${geo().x}px`, '--y': `${geo().y}px`, '--s': `${geo().size}px` }}>
      <div class="echo__steps" aria-hidden="true">
        <For each={ECHO_STEPS}>
          {(s, i) => (
            <>
              <i class="echo__dot" classList={{ 'is-on': pos() === i() }} style={at(geo().size / 2 + geo().dots, STEP_ANGLES[i()])} />
              <span
                class="echo__st"
                classList={{ 'is-on': pos() === i(), 'is-off': !enabled() }}
                data-control
                style={at(geo().size / 2 + geo().labels, STEP_ANGLES[i()])}
                onPointerDown={(e) => e.preventDefault()}
                onClick={() => enabled() && send(i())}
              >
                {s.label}
                <Show when={!props.narrow}>
                  <small>{length(i())}</small>
                </Show>
              </span>
            </>
          )}
        </For>
      </div>
      <div
        class="uface__knob"
        role="slider"
        tabindex={0}
        aria-label="ECHO time"
        aria-valuemin={0}
        aria-valuemax={ECHO_STEPS.length - 1}
        aria-valuenow={pos()}
        aria-valuetext={`${ECHO_STEPS[pos()].label}, ${length(pos())} ms`}
        aria-disabled={!enabled()}
        data-param="echoTime"
        style={knobStyle(`${STEP_ANGLES[pos()]}deg`, geo().size, true)}
        onPointerDown={onDown}
        onPointerMove={onMove}
        onPointerUp={onUp}
        onPointerCancel={onUp}
        on:lostpointercapture={onUp}
        onKeyDown={onKey}
        onDblClick={() => enabled() && send(stepOfIndex(DEF.time))}
        ref={(el) => {
          el.addEventListener('wheel', wheel, { passive: false });
          onCleanup(() => el.removeEventListener('wheel', wheel));
        }}
      >
        <KnobBody />
      </div>
      <span class="uface__lgd" aria-hidden="true">
        TIME
      </span>
      <span class="uface__val" classList={{ 'is-adjusting': held() !== null }} aria-hidden="true">
        {ECHO_STEPS[pos()].label}
        <span class="uface__u">{length(pos())} ms</span>
      </span>
    </div>
  );
}
