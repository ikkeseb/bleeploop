import { For, Show, createSignal, onCleanup, onMount } from 'solid-js';
import { Portal } from 'solid-js/web';
import type { UnitParamId, UnitState } from '../../platform';
import { setUnitParam, toggleUnit } from '../state/chain';
import { EscapeCloses } from '../looper/shared';
import type { RowPopover } from '../instrument/RowPopover';
import { registerOttGlass } from './ott-loop';
import './ott-face.css';

/**
 * The OTT's own face (the accepted `docs/redesign/ott-open.html`): opened from its plate (Parameters, or
 * a press on its name), it hangs under the slot row over the lanes. The glass on top shows each band's
 * gain over the last three seconds, NOW and its level, from the feed's `unitMeters` (drawn by
 * `ott-loop.ts`, never through a signal); under it the name, the power lamp (the unit's bypass) and the
 * four programmed knobs, DEPTH the hero. A knob sends `SetUnitParam` and shows the feed's value, its own
 * only while a drag holds it. Non-modal, as the row's popovers: Escape, its close or a press outside
 * closes it, a keyboard close hands focus back to the plate's name. The face is 900x540 at the chrome's
 * size step (`--ui`); where the window cannot hold that under the row (1000x700, 960x600), it reflows to
 * its narrow face, 900x382: a shorter glass (5 px a dB), smaller knobs, the same legends at full size.
 * Its type is never scaled below the base (a scale under 1 only past the app's minimum window).
 */

/** The face's two layouts (CSS pixels at scale 1): the accepted face, and its narrow reflow. */
interface Layout {
  w: number;
  h: number;
  /** The glass's 0 dB line and pixels per dB (`ott-loop.ts` reads them off the canvas). */
  y0: number;
  px: number;
  knobs: readonly KnobDef[];
}
/** Where the plate's centre falls on the face, as the mockup hangs it (the face's left on the lanes' glass). */
const TIE_AT = 333;

interface KnobDef {
  param: UnitParamId;
  label: string;
  def: number;
  x: number;
  y: number;
  size: number;
  hero?: boolean;
  tri?: 'up' | 'dn';
}
const WIDE: Layout = {
  w: 900,
  h: 540,
  y0: 120,
  px: 7,
  knobs: [
    { param: 'ottDepth', label: 'DEPTH', def: 0.5, x: 318, y: 386, size: 172, hero: true },
    { param: 'ottTime', label: 'TIME', def: 0.5, x: 528, y: 424, size: 92 },
    { param: 'ottUpward', label: 'UP', def: 1, x: 690, y: 424, size: 92, tri: 'up' },
    { param: 'ottDownward', label: 'DOWN', def: 1, x: 812, y: 424, size: 92, tri: 'dn' },
  ],
};
const NARROW: Layout = {
  w: 900,
  h: 382,
  y0: 92,
  px: 5,
  knobs: [
    { param: 'ottDepth', label: 'DEPTH', def: 0.5, x: 300, y: 275, size: 96, hero: true },
    { param: 'ottTime', label: 'TIME', def: 0.5, x: 500, y: 287, size: 60 },
    { param: 'ottUpward', label: 'UP', def: 1, x: 655, y: 287, size: 60, tri: 'up' },
    { param: 'ottDownward', label: 'DOWN', def: 1, x: 790, y: 287, size: 60, tri: 'dn' },
  ],
};
const NAMES: Record<string, string> = { ottDepth: 'depth', ottTime: 'time', ottUpward: 'upward', ottDownward: 'downward' };

const paramOf = (u: UnitState, k: KnobDef): number => {
  const params = 'params' in u ? u.params : undefined;
  return params?.[k.param] ?? k.def;
};

export function OttFace(props: {
  pop: RowPopover;
  unit: () => UnitState;
  /** The plate's element: the face hangs from it. */
  plate: () => HTMLElement | undefined;
  slotLetter: string;
}) {
  const [box, setBox] = createSignal({ left: 0, top: 0, scale: 1, tieX: 0, tieY: 0, layout: WIDE });
  const place = () => {
    const r = props.plate()?.getBoundingClientRect();
    if (!r) return;
    const top = r.bottom + 19;
    // the chrome's size step (`--ui`, a step up on the wide display); the accepted face where the window
    // holds it under the row, else the narrow reflow; a scale under the step only past the minimum window
    const ui = Number(getComputedStyle(document.documentElement).getPropertyValue('--ui')) || 1;
    const room = (l: Layout) => Math.min((window.innerWidth - 16) / l.w, (window.innerHeight - top - 8) / l.h);
    const layout = room(WIDE) >= ui ? WIDE : NARROW;
    const scale = Math.max(0.3, Math.min(ui, room(layout)));
    const centre = r.left + r.width / 2;
    const left = Math.max(8, Math.min(centre - TIE_AT * scale, window.innerWidth - 8 - layout.w * scale));
    setBox({ left, top, scale, tieX: centre - 16, tieY: r.bottom, layout });
  };
  onMount(() => {
    place();
    window.addEventListener('resize', place);
    onCleanup(() => window.removeEventListener('resize', place));
  });
  const on = () => !props.unit().bypass;
  const id = () => props.unit().id;
  return (
    <Portal>
      <EscapeCloses close={() => props.pop.close(true)} />
      <div class="slot-pop__backdrop ott-backdrop" onClick={() => props.pop.close(false)}>
        <i class="ott-tie" aria-hidden="true" style={{ left: `${box().tieX}px`, top: `${box().tieY}px` }} />
        <div
          class="ott"
          classList={{ 'ott--byp': !on(), 'ott--narrow': box().layout === NARROW }}
          role="dialog"
          aria-label={`OTT, slot ${props.slotLetter}`}
          id={`ott-face-${id()}`}
          tabindex={-1}
          style={{ left: `${box().left}px`, top: `${box().top}px`, scale: String(box().scale) }}
          ref={(el) => queueMicrotask(() => el.focus())}
          onClick={(e) => e.stopPropagation()}
        >
          <Glass unit={id()} y0={box().layout.y0} px={box().layout.px} />
          <span class="ott__wm" aria-hidden="true">
            OTT
          </span>
          <button
            type="button"
            class="ott__pwr"
            classList={{ 'is-on': on() }}
            aria-pressed={on()}
            aria-label={`OTT in slot ${props.slotLetter} on`}
            title={on() ? 'On: press to bypass' : 'Bypassed: press to switch on'}
            disabled={props.unit().state !== 'seated'}
            onClick={() => toggleUnit(id())}
          />
          <span class="ott__maker" aria-hidden="true">
            BLEEPLOOP
          </span>
          <For each={box().layout.knobs}>{(k) => <Knob def={k} unit={props.unit} />}</For>
          <button type="button" class="ott__close" aria-label="Close the OTT" onClick={(e) => props.pop.close(e.detail === 0)}>
            <svg width="12" height="12" viewBox="0 0 12 12" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" aria-hidden="true">
              <path d="M2 2l8 8M10 2L2 10" />
            </svg>
          </button>
        </div>
      </div>
    </Portal>
  );
}

/** The glass: its canvas, the printed scale and band names, the three read-outs the loop writes. */
function Glass(props: { unit: number; y0: number; px: number }) {
  const values: HTMLElement[] = [];
  let canvas!: HTMLCanvasElement;
  onMount(() => onCleanup(registerOttGlass(canvas, values, props.unit)));
  // the scale and the bands' places, as the loop draws them (ott-loop.ts: X0 40, the 0 dB line at `y0`,
  // `px` a dB)
  const W = 852;
  const band = (i: number) => 40 + ((W - 6 - 40) / 3) * i;
  const dbs = [12, 6, 0, -6, -12];
  return (
    <div class="ott__bezel">
      <div class="ott__glass">
        <canvas ref={canvas} class="ott__canvas" data-y0={props.y0} data-px={props.px} aria-hidden="true" />
        <For each={dbs}>
          {(d) => (
            <span class="ott__db" classList={{ 'is-zero': d === 0 }} style={{ top: `${props.y0 - d * props.px}px` }} aria-hidden="true">
              {d > 0 ? `+${d}` : d < 0 ? `−${-d}` : '0'}
            </span>
          )}
        </For>
        <For each={['LOW', 'MID', 'HIGH']}>
          {(name, i) => (
            <>
              <span class="ott__band" style={{ left: `${Math.round(band(i())) + 12}px` }} aria-hidden="true">
                {name}
              </span>
              <span
                class="ott__gv"
                style={{ left: `${Math.round(band(i() + 1)) - 12 - 6 - 10}px` }}
                ref={(el) => (values[i()] = el)}
                title={`${name.toLowerCase()} band gain now`}
              >
                <span>0.0</span>
                <span class="ott__u">dB</span>
              </span>
            </>
          )}
        </For>
        <For each={[['88', 'Hz'], ['2.5', 'kHz']] as const}>
          {([v, u], i) => (
            <span class="ott__hz" style={{ left: `${Math.round(band(i() + 1))}px` }} aria-hidden="true">
              {v}
              <span class="ott__u">{u}</span>
            </span>
          )}
        </For>
      </div>
    </div>
  );
}

/** Pixels of vertical drag for a knob's whole travel; Shift drags four times finer. */
const DRAG_PX = 220;

/** A programmed knob: its body, knurl, cap and pointer, the hero's LED ring or a small one's printed ticks,
 * its legend and value on the face's one row. A slider to assistive tech and the keyboard. */
function Knob(props: { def: KnobDef; unit: () => UnitState }) {
  const k = props.def;
  const [held, setHeld] = createSignal<number | null>(null);
  const value = () => held() ?? paramOf(props.unit(), k);
  const pct = () => Math.round(value() * 100);
  // only a seated unit takes its params (the batch drops one for any other: a waiting, parked or leaving unit)
  const enabled = () => props.unit().state === 'seated';
  const send = (v: number) => {
    const x = Math.round(Math.min(1, Math.max(0, v)) * 100) / 100;
    setUnitParam(props.unit().id, k.param, x);
    return x;
  };
  let drag: { id: number; y: number; from: number } | null = null;
  const onDown = (e: PointerEvent) => {
    if (!enabled() || e.button !== 0) return;
    e.preventDefault();
    (e.currentTarget as HTMLElement).focus();
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    drag = { id: e.pointerId, y: e.clientY, from: value() };
    setHeld(value());
  };
  const onMove = (e: PointerEvent) => {
    if (!drag || drag.id !== e.pointerId) return;
    const v = drag.from + ((drag.y - e.clientY) / DRAG_PX) * (e.shiftKey ? 0.25 : 1);
    setHeld(send(v));
  };
  const onUp = (e: PointerEvent) => {
    if (drag?.id !== e.pointerId) return;
    drag = null;
    setHeld(null);
  };
  const onKey = (e: KeyboardEvent) => {
    if (!enabled()) return;
    const step = { ArrowUp: 0.01, ArrowRight: 0.01, ArrowDown: -0.01, ArrowLeft: -0.01, PageUp: 0.1, PageDown: -0.1 }[e.key];
    if (step !== undefined) send(value() + step);
    else if (e.key === 'Home') send(0);
    else if (e.key === 'End') send(1);
    else return;
    e.preventDefault();
    e.stopPropagation();
  };
  const wheel = (e: WheelEvent) => {
    if (!enabled()) return;
    e.preventDefault();
    send(value() + (e.deltaY < 0 ? 0.01 : -0.01));
  };
  const angle = () => `${-135 + 270 * value()}deg`;
  // the hero's ring of LED dots, lit up to its value; a small knob's eleven printed ticks
  const RING = 31;
  const ringR = k.size / 2 + 13;
  const ringW = k.size + 40;
  const tickPad = 18;
  const tickW = k.size + tickPad * 2;
  return (
    <div class="ott__ctl" classList={{ 'ott__ctl--hero': k.hero }} style={{ '--x': `${k.x}px`, '--y': `${k.y}px`, '--s': `${k.size}px` }}>
      <div
        class="ott__knob"
        role="slider"
        tabindex={0}
        aria-label={`OTT ${NAMES[k.param]}`}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={pct()}
        aria-valuetext={`${pct()} %`}
        aria-disabled={!enabled()}
        data-param={k.param}
        style={{ '--a': angle(), '--kd': `${((360 * 1.35) / (Math.PI * k.size)).toFixed(3)}deg`, '--pw': k.hero ? '3px' : '2.5px' }}
        onPointerDown={onDown}
        onPointerMove={onMove}
        onPointerUp={onUp}
        onPointerCancel={onUp}
        onKeyDown={onKey}
        onDblClick={() => enabled() && send(k.def)}
        ref={(el) => {
          el.addEventListener('wheel', wheel, { passive: false });
          onCleanup(() => el.removeEventListener('wheel', wheel));
        }}
      >
        <Show
          when={k.hero}
          fallback={
            <svg class="ott__ticks" width={tickW} height={tickW} aria-hidden="true">
              <For each={Array.from({ length: 11 }, (_, i) => i)}>
                {(i) => {
                  const q = ((-135 + 27 * i) * Math.PI) / 180;
                  const major = i === 0 || i === 10 || i === 5;
                  const c = tickW / 2;
                  const r0 = k.size / 2 + 4;
                  const r1 = r0 + 6 + (major ? 1.5 : 0);
                  return (
                    <line
                      x1={c + r0 * Math.sin(q)}
                      y1={c - r0 * Math.cos(q)}
                      x2={c + r1 * Math.sin(q)}
                      y2={c - r1 * Math.cos(q)}
                      stroke={major ? '#8f96a0' : '#555c65'}
                      stroke-width="1.4"
                      stroke-linecap="round"
                    />
                  );
                }}
              </For>
            </svg>
          }
        >
          <div class="ott__ring" style={{ width: `${ringW}px`, height: `${ringW}px` }} aria-hidden="true">
            <For each={Array.from({ length: RING }, (_, i) => i)}>
              {(i) => {
                const t = ((-135 + (270 * i) / (RING - 1)) * Math.PI) / 180;
                return (
                  <i
                    classList={{ 'is-on': i / (RING - 1) <= value() + 1e-6 }}
                    style={{ left: `${(ringW / 2 + ringR * Math.sin(t)).toFixed(2)}px`, top: `${(ringW / 2 - ringR * Math.cos(t)).toFixed(2)}px` }}
                  />
                );
              }}
            </For>
          </div>
        </Show>
        <div class="ott__body" />
        <div class="ott__cap" />
        <div class="ott__ptr" />
      </div>
      <span class="ott__lgd" aria-hidden="true">
        <Show when={k.tri}>
          <i class={`ott__tri ott__tri--${k.tri}`} />
        </Show>
        {k.label}
      </span>
      <span class="ott__val" classList={{ 'is-adjusting': held() !== null }} aria-hidden="true">
        {pct()}
        <span class="ott__u">%</span>
      </span>
    </div>
  );
}
