import { For, Show, createMemo, createSignal, onCleanup, onMount, type JSX } from 'solid-js';
import { Portal } from 'solid-js/web';
import type { UnitParamId, UnitState } from '../../platform';
import { setUnitParam, toggleUnit } from '../state/chain';
import { EscapeCloses } from '../looper/shared';
import type { RowPopover } from '../instrument/RowPopover';
import { NARROW, WIDE, nearHome, placeFace, sizeFromCorner, stepSize, type FaceAt, type FaceBox, type FaceInput } from './face-place';
import './unit-face.css';

/**
 * A built-in unit's face, the frame every face shares (the OTT's, the ECHO's): one piece of gear opened
 * from its plate (Parameters, or a press on its name). It opens at its home, hanging under the slot row
 * from its plate by a strap of the plate's own panel; the player may move it anywhere in the window (a
 * press and drag on the face's panel, its glass included) and scale it from its corner grip, and it keeps
 * that place and size per unit kind, across launches (`lf.unitFaces`). Moved off home the strap goes (it
 * means "hangs from this plate"); a move released near home lands home. Its type never goes under the
 * base size: shrunk past it, the narrow face takes over (`face-place.ts`). It never leaves the window: a
 * window resize pulls it back in. Non-modal, as the row's popovers: Escape, its close or a press outside
 * closes it, a keyboard close hands focus back to the plate's name. With the face itself focused, the
 * arrow keys move it (8 px, Shift 40 px), + and - scale it, Home sends it home at its first size.
 * The frame draws the name, the power switch (the unit's bypass), the maker's mark and the close; the
 * face draws its glass and its controls (`children`), on the narrow face or the full one.
 */
export function UnitFace(props: {
  pop: RowPopover;
  unit: () => UnitState;
  /** The plate's element: the face hangs from it at home. */
  plate: () => HTMLElement | undefined;
  slotLetter: string;
  /** The unit's kind: the face's class and the key its place is kept under. */
  kind: 'ott' | 'echo';
  /** The name printed on the face. */
  name: string;
  children: (narrow: () => boolean) => JSX.Element;
}) {
  let frame!: HTMLDivElement;
  const [plateBox, setPlateBox] = createSignal<FaceInput['plate']>({ left: 0, width: 0, bottom: 0 });
  const [win, setWin] = createSignal({ w: window.innerWidth, h: window.innerHeight });
  const [ui, setUi] = createSignal(1);
  const measure = () => {
    const r = props.plate()?.getBoundingClientRect();
    if (r) setPlateBox({ left: r.left, width: r.width, bottom: r.bottom });
    setWin({ w: window.innerWidth, h: window.innerHeight });
    // the chrome's size step (`--ui`, a step up on the wide display)
    setUi(Number(getComputedStyle(document.documentElement).getPropertyValue('--ui')) || 1);
  };
  measure();
  onMount(() => {
    measure();
    window.addEventListener('resize', measure);
    onCleanup(() => window.removeEventListener('resize', measure));
  });

  // what the player set (kept), and a drag's own while it runs
  const [live, setLive] = createSignal<Kept | null>(null);
  const kept = () => live() ?? faces()[props.kind] ?? EMPTY;
  const input = (k: Kept = kept()): FaceInput => ({
    plate: plateBox(),
    win: win(),
    ui: ui(),
    size: k.size ? { narrow: k.size.narrow, scale: k.size.k * ui() } : null,
    at: k.at,
  });
  const box = createMemo(() => placeFace(input()));
  /** The size as kept: its scale a multiple of the size step, so a place kept on one display fits another. */
  const keepSize = (b: FaceBox): Kept['size'] => ({ narrow: b.narrow, k: b.scale / ui() });
  const commit = (k: Kept) => {
    setLive(null);
    keep(props.kind, k);
  };

  // ── the move and the resize: one press per pointer; a release eats its own click (never a close) ──
  let press: { id: number; x: number; y: number; start: FaceBox; resize: boolean; moved: boolean } | null = null;
  let swallow = false;
  const [moving, setMoving] = createSignal(false);
  const begin = (e: PointerEvent, resize: boolean) => {
    if (press || e.button !== 0) return;
    e.preventDefault();
    e.stopPropagation();
    frame.focus({ preventScroll: true });
    frame.setPointerCapture(e.pointerId);
    press = { id: e.pointerId, x: e.clientX, y: e.clientY, start: box(), resize, moved: false };
  };
  const onDown = (e: PointerEvent) => {
    // the controls are their own presses; the rest of the panel, the glass included, carries the face
    if ((e.target as HTMLElement).closest('button, [role="slider"], .uface__grip')) return;
    begin(e, false);
  };
  const onMove = (e: PointerEvent) => {
    if (!press || e.pointerId !== press.id) return;
    const dx = e.clientX - press.x;
    const dy = e.clientY - press.y;
    if (!press.moved && Math.hypot(dx, dy) < 3) return;
    press.moved = true;
    setMoving(true);
    const s = press.start;
    const base = faces()[props.kind] ?? EMPTY;
    if (press.resize) {
      const corner = { x: s.left + WIDE.w * s.scale + dx, y: s.top + (s.narrow ? NARROW : WIDE).h * s.scale + dy };
      const size = sizeFromCorner(s, input(base), corner.x, corner.y);
      // a face at home grows from its strap; a moved one from its top-left
      setLive({ at: s.home ? null : { x: s.left, y: s.top }, size: { narrow: size.narrow, k: size.scale / ui() } });
    } else {
      setLive({ at: { x: s.left + dx, y: s.top + dy }, size: base.size ?? keepSize(s) });
    }
  };
  const onUp = (e: PointerEvent) => {
    if (!press || e.pointerId !== press.id) return;
    const p = press;
    press = null;
    setMoving(false);
    if (frame.hasPointerCapture(e.pointerId)) frame.releasePointerCapture(e.pointerId);
    if (!p.moved) return;
    swallow = true;
    setTimeout(() => (swallow = false));
    const b = box();
    const base = faces()[props.kind] ?? EMPTY;
    if (p.resize) commit({ at: b.home ? null : { x: b.left, y: b.top }, size: keepSize(b) });
    else if (nearHome(input(), b)) commit({ at: null, size: base.size });
    else commit({ at: { x: b.left, y: b.top }, size: base.size ?? keepSize(p.start) });
  };
  const onCancel = (e: PointerEvent) => {
    if (!press || e.pointerId !== press.id) return;
    press = null;
    setMoving(false);
    setLive(null);
  };
  onCleanup(() => {
    press = null;
    setLive(null);
  });

  const onKey = (e: KeyboardEvent) => {
    // the face itself: a control's keys are its own
    if (e.target !== frame || e.altKey || e.ctrlKey || e.metaKey) return;
    const b = box();
    const base = faces()[props.kind] ?? EMPTY;
    const step = e.shiftKey ? 40 : 8;
    const nudge = { ArrowLeft: [-step, 0], ArrowRight: [step, 0], ArrowUp: [0, -step], ArrowDown: [0, step] }[e.key];
    if (nudge) commit({ at: { x: b.left + nudge[0], y: b.top + nudge[1] }, size: base.size ?? keepSize(b) });
    else if (e.key === '+' || e.key === '=' || e.key === '-') {
      const size = stepSize({ narrow: b.narrow, scale: b.scale }, ui(), e.key !== '-');
      commit({ at: base.at, size: { narrow: size.narrow, k: size.scale / ui() } });
    } else if (e.key === 'Home') commit(EMPTY);
    else return;
    e.preventDefault();
    e.stopPropagation();
  };

  const on = () => !props.unit().bypass;
  const id = () => props.unit().id;
  const tie = () => {
    const p = plateBox();
    return { left: `${p.left + p.width / 2 - 16}px`, top: `${p.bottom}px` };
  };
  return (
    <Portal>
      <EscapeCloses close={() => props.pop.close(true)} />
      <div
        class="slot-pop__backdrop uface-backdrop"
        onClick={() => {
          if (!swallow) props.pop.close(false);
        }}
      >
        <Show when={box().home}>
          <i class="uface-tie" aria-hidden="true" style={tie()} />
        </Show>
        <div
          ref={(el) => {
            frame = el;
            queueMicrotask(() => el.focus());
          }}
          class={`uface ${props.kind}`}
          classList={{ 'uface--byp': !on(), 'uface--narrow': box().narrow, 'uface--moving': moving(), 'uface--moved': !box().home }}
          role="dialog"
          aria-label={`${props.name}, slot ${props.slotLetter}`}
          aria-keyshortcuts="ArrowLeft ArrowRight ArrowUp ArrowDown + - Home"
          id={`${props.kind}-face-${id()}`}
          tabindex={-1}
          style={{ left: `${box().left}px`, top: `${box().top}px`, scale: String(box().scale) }}
          onClick={(e) => e.stopPropagation()}
          onPointerDown={onDown}
          onPointerMove={onMove}
          onPointerUp={onUp}
          onPointerCancel={onCancel}
          on:lostpointercapture={onCancel}
          onKeyDown={onKey}
        >
          {props.children(() => box().narrow)}
          <span class="uface__wm" aria-hidden="true">
            {props.name}
          </span>
          <button
            type="button"
            class="uface__pwr"
            classList={{ 'is-on': on() }}
            aria-pressed={on()}
            aria-label={`${props.name} in slot ${props.slotLetter} on`}
            title={on() ? 'On: press to bypass' : 'Bypassed: press to switch on'}
            disabled={props.unit().state !== 'seated'}
            onClick={() => toggleUnit(id())}
          />
          <span class="uface__maker" aria-hidden="true">
            BLEEPLOOP
          </span>
          <button type="button" class="uface__close" aria-label={`Close the ${props.name}`} onClick={(e) => props.pop.close(e.detail === 0)}>
            <svg width="12" height="12" viewBox="0 0 12 12" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" aria-hidden="true">
              <path d="M2 2l8 8M10 2L2 10" />
            </svg>
          </button>
          <i class="uface__grip" aria-hidden="true" title="Drag to resize" onPointerDown={(e) => begin(e, true)} />
        </div>
      </div>
    </Portal>
  );
}

// ── the place and size each kind of face keeps, across launches ───────────────────────────────────────

/** A kind's kept place (null: home) and size (null: the default; `k` the scale over the size step). */
interface Kept {
  at: FaceAt | null;
  size: { narrow: boolean; k: number } | null;
}
const EMPTY: Kept = { at: null, size: null };
const STORE = 'lf.unitFaces';
const finite = (x: unknown): x is number => typeof x === 'number' && Number.isFinite(x);
function readKept(): Record<string, Kept> {
  try {
    const raw: unknown = JSON.parse(localStorage.getItem(STORE) ?? '{}');
    const out: Record<string, Kept> = {};
    if (typeof raw !== 'object' || raw === null) return out;
    for (const [kind, v] of Object.entries(raw as Record<string, Partial<Kept>>)) {
      const at = v?.at && finite(v.at.x) && finite(v.at.y) ? { x: v.at.x, y: v.at.y } : null;
      const size = v?.size && typeof v.size.narrow === 'boolean' && finite(v.size.k) && v.size.k > 0 ? { narrow: v.size.narrow, k: v.size.k } : null;
      out[kind] = { at, size };
    }
    return out;
  } catch {
    return {};
  }
}
const [faces, setFaces] = createSignal<Record<string, Kept>>(typeof localStorage === 'undefined' ? {} : readKept());
function keep(kind: string, k: Kept): void {
  const next = { ...faces(), [kind]: k };
  setFaces(next);
  try {
    localStorage.setItem(STORE, JSON.stringify(next));
  } catch {
    /* storage full or off: the place holds for the run */
  }
}

// ── a programmed knob, the family's ─────────────────────────────────────────────────────────────────

/** A knob on a face: the param it sends, its legend and accessible name, its default and its top (the
 * engine's ceiling: FEEDBACK's 0.95), its centre and size on the face, the hero's LED ring or a small
 * one's ticks, and a band triangle beside its legend (the OTT's UP and DOWN). It reads in percent. */
export interface KnobDef {
  param: UnitParamId;
  label: string;
  /** Its accessible name after the unit's name ("depth": "OTT depth"). */
  name: string;
  def: number;
  max?: number;
  x: number;
  y: number;
  size: number;
  hero?: boolean;
  tri?: 'up' | 'dn';
}

/** A built-in's param as the feed reports it, or its default. */
export const paramOf = (u: UnitState, param: UnitParamId, def: number): number => {
  const params = 'params' in u ? u.params : undefined;
  return params?.[param] ?? def;
};

/** Pixels of vertical drag for a knob's whole travel; Shift drags four times finer. */
const DRAG_PX = 220;

/** A programmed knob: its body, knurl, cap and pointer, the hero's LED ring or a small one's printed ticks,
 * its legend and value on the face's one row. A slider to assistive tech and the keyboard. Sends
 * `SetUnitParam` and shows the feed's value, its own only while a drag holds it. */
export function FaceKnob(props: { def: KnobDef; unit: () => UnitState; unitName: string }) {
  const k = props.def;
  const max = k.max ?? 1;
  const [held, setHeld] = createSignal<number | null>(null);
  const value = () => held() ?? paramOf(props.unit(), k.param, k.def);
  const pct = () => Math.round(value() * 100);
  /** The knob's travel, 0 to 1, over the param's range. */
  const travel = () => Math.min(1, Math.max(0, value() / max));
  // only a seated unit takes its params (the batch drops one for any other: a waiting, parked or leaving unit)
  const enabled = () => props.unit().state === 'seated';
  const send = (v: number) => {
    const x = Math.round(Math.min(max, Math.max(0, v)) * 100) / 100;
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
    const v = drag.from + ((drag.y - e.clientY) / DRAG_PX) * max * (e.shiftKey ? 0.25 : 1);
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
    else if (e.key === 'End') send(max);
    else return;
    e.preventDefault();
    e.stopPropagation();
  };
  const wheel = (e: WheelEvent) => {
    if (!enabled()) return;
    e.preventDefault();
    send(value() + (e.deltaY < 0 ? 0.01 : -0.01));
  };
  const angle = () => `${-135 + 270 * travel()}deg`;
  // the hero's ring of LED dots, lit up to its value; a small knob's eleven printed ticks
  const RING = 31;
  const ringR = k.size / 2 + 13;
  const ringW = k.size + 40;
  const tickW = k.size + 36;
  return (
    <div class="uface__ctl" classList={{ 'uface__ctl--hero': k.hero }} style={{ '--x': `${k.x}px`, '--y': `${k.y}px`, '--s': `${k.size}px` }}>
      <div
        class="uface__knob"
        role="slider"
        tabindex={0}
        aria-label={`${props.unitName} ${k.name}`}
        aria-valuemin={0}
        aria-valuemax={Math.round(max * 100)}
        aria-valuenow={pct()}
        aria-valuetext={`${pct()} %`}
        aria-disabled={!enabled()}
        data-param={k.param}
        style={knobStyle(angle(), k.size, k.hero)}
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
        <Show when={k.hero} fallback={<Ticks size={k.size} w={tickW} />}>
          <div class="uface__ring" style={{ width: `${ringW}px`, height: `${ringW}px` }} aria-hidden="true">
            <For each={Array.from({ length: RING }, (_, i) => i)}>
              {(i) => {
                const t = ((-135 + (270 * i) / (RING - 1)) * Math.PI) / 180;
                return (
                  <i
                    classList={{ 'is-on': i / (RING - 1) <= travel() + 1e-6 }}
                    style={{ left: `${(ringW / 2 + ringR * Math.sin(t)).toFixed(2)}px`, top: `${(ringW / 2 - ringR * Math.cos(t)).toFixed(2)}px` }}
                  />
                );
              }}
            </For>
          </div>
        </Show>
        <KnobBody />
      </div>
      <span class="uface__lgd" aria-hidden="true">
        <Show when={k.tri}>
          <i class={`uface__tri uface__tri--${k.tri}`} />
        </Show>
        {k.label}
      </span>
      <span class="uface__val" classList={{ 'is-adjusting': held() !== null }} aria-hidden="true">
        {pct()}
        <span class="uface__u">%</span>
      </span>
    </div>
  );
}

/** A knob's turn and its knurl's pitch, as its CSS reads them. */
export const knobStyle = (angle: string, size: number, hero?: boolean): JSX.CSSProperties => ({
  '--a': angle,
  '--kd': `${((360 * 1.35) / (Math.PI * size)).toFixed(3)}deg`,
  '--pw': hero ? '3px' : '2.5px',
});

/** The knob itself: knurled body, cap, pointer. */
export function KnobBody() {
  return (
    <>
      <div class="uface__body" />
      <div class="uface__cap" />
      <div class="uface__ptr" />
    </>
  );
}

/** A small knob's eleven printed ticks, the middle and both ends longer. */
function Ticks(props: { size: number; w: number }) {
  return (
    <svg class="uface__ticks" width={props.w} height={props.w} aria-hidden="true">
      <For each={Array.from({ length: 11 }, (_, i) => i)}>
        {(i) => {
          const q = ((-135 + 27 * i) * Math.PI) / 180;
          const major = i === 0 || i === 10 || i === 5;
          const c = props.w / 2;
          const r0 = props.size / 2 + 4;
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
  );
}
