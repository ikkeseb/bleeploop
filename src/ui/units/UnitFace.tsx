import { For, Show, createEffect, createMemo, createSignal, on, onCleanup, onMount, type JSX } from 'solid-js';
import type { UnitParamId, UnitState } from '../../platform';
import { setUnitParam, slotChain, toggleUnit } from '../state/chain';
import { NARROW, WIDE, nearHome, placeFace, sizeFromCorner, stepSize, type FaceAt, type FaceBox, type FaceInput } from './face-place';
import { faceLayer, plateOf, raiseFace, type FaceKind } from './face-store';
import './unit-face.css';

/**
 * A built-in unit's face, the frame every face shares (the OTT's, the ECHO's): one piece of gear opened
 * from its plate (Parameters, or a press on its name), drawn by `FaceHost.tsx` while it is open
 * (`face-store.ts`). Free-standing: no backdrop, so the looper and the row under and beside it play as
 * ever, a press outside never closes it, a slot folded under it leaves it open where it stands, and another
 * unit's face may stand beside it, the last touched on top. It closes by its close, by Escape with the focus
 * in it, or by its plate's name again; a keyboard close hands the focus back to the plate's name.
 *
 * It opens at its home, just under the slot row below its plate (with nothing drawn between them); the
 * player may move it anywhere in the window (a press and drag on the face's panel, its glass included) and
 * scale it from its corner grip, and it keeps that place and size per unit kind, across launches
 * (`lf.unitFaces`). A move released near home lands home; a move or nudge keeps the displayed fit for the
 * current viewport without storing it as the size preference. Its type never goes under the base size:
 * shrunk past it, the narrow face takes over (`face-place.ts`). It never leaves the window: a window resize
 * pulls it back in, and a window too short for it under the plate (or a scrolled plate above the margin)
 * fits it to the window instead. With the face
 * itself focused, the arrow keys move it (8 px, Shift 40 px), + and - scale it, Home sends it home at its
 * first size. The frame draws the name, the power switch (the unit's bypass), the maker's mark and the
 * close; the face draws its glass and its controls (`children`), on the narrow face or the full one.
 */
export function UnitFace(props: {
  unit: () => UnitState;
  slotLetter: () => string;
  /** The unit's kind: the face's class and the key its place is kept under. */
  kind: FaceKind;
  /** The name printed on the face. */
  name: string;
  close: (keyboard: boolean) => void;
  children: (narrow: () => boolean) => JSX.Element;
}) {
  let frame!: HTMLDivElement;
  const id = () => props.unit().id;
  const plateEl = () => plateOf(id())?.el;
  // the plate's box as last measured (a folded plate's last place: the face stays where it stood)
  const [plateBox, setPlateBox] = createSignal<FaceInput['plate']>({ left: 0, width: 0, bottom: 0 });
  const [win, setWin] = createSignal({ w: window.innerWidth, h: window.innerHeight });
  const [ui, setUi] = createSignal(1);
  const measure = () => {
    const el = plateOf(id())?.el;
    const r = el?.isConnected ? el.getBoundingClientRect() : null;
    if (r && r.width > 0) setPlateBox({ left: r.left, width: r.width, bottom: r.bottom });
    setWin({ w: window.innerWidth, h: window.innerHeight });
    // the chrome's size step (`--ui`, a step up on the wide display)
    setUi(Number(getComputedStyle(document.documentElement).getPropertyValue('--ui')) || 1);
  };
  measure();
  // measured again whenever the plate can have moved: the window, a scroll, the plate's own size, a chain
  // report (a reorder, a plate before it come or gone), the plate mounted again (its slot unfolded)
  let raf = 0;
  const later = () => {
    if (raf === 0) raf = requestAnimationFrame(() => ((raf = 0), measure()));
  };
  const plateSize = new ResizeObserver(later);
  createEffect(
    on(plateEl, (el) => {
      plateSize.disconnect();
      if (el) plateSize.observe(el);
      measure();
    }),
  );
  createEffect(on([() => slotChain(0), () => slotChain(1)], later, { defer: true }));
  onMount(() => {
    measure();
    window.addEventListener('resize', measure);
    document.addEventListener('scroll', later, { capture: true, passive: true });
  });
  onCleanup(() => {
    window.removeEventListener('resize', measure);
    document.removeEventListener('scroll', later, { capture: true });
    plateSize.disconnect();
    cancelAnimationFrame(raf);
  });

  // what the player set (kept), a drag's own while it runs, and the displayed fit held after a move or nudge
  // (so release never jumps, without storing it as the size preference): it holds for the viewport it was
  // made in and clears at the first change of the window or the size step, never to return with a round trip
  const [fitHold, setFitHold] = createSignal<{ narrow: boolean; k: number } | null>(null);
  const viewport = createMemo(() => `${win().w}x${win().h}@${ui()}`);
  createEffect(on(viewport, () => setFitHold(null), { defer: true }));
  const [live, setLive] = createSignal<Kept | null>(null);
  const kept = () => live() ?? faces()[props.kind] ?? EMPTY;
  const input = (k: Kept = kept(), useHold = live() === null): FaceInput => {
    const size = k.at && useHold ? (fitHold() ?? k.size) : k.size;
    return {
      plate: plateBox(),
      win: win(),
      ui: ui(),
      size: size ? { narrow: size.narrow, scale: size.k * ui() } : null,
      at: k.at,
    };
  };
  const box = createMemo(() => placeFace(input()));
  /** The size as kept: its scale a multiple of the size step, so a place kept on one display fits another. */
  const keepSize = (b: FaceBox): Kept['size'] => ({ narrow: b.narrow, k: b.scale / ui() });
  const commit = (k: Kept) => {
    setLive(null);
    keep(props.kind, k);
  };

  // ── the move and the resize: one press per pointer ──
  let press: { id: number; x: number; y: number; start: FaceBox; resize: boolean; moved: boolean } | null = null;
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
    if ((e.target as HTMLElement).closest('button, [role="slider"], [data-control]')) return;
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
      const size = sizeFromCorner(s, input(base, false), corner.x, corner.y);
      // a face at home grows under its plate; a moved one from its top-left
      setLive({ at: s.home ? null : { x: s.left, y: s.top }, size: { narrow: size.narrow, k: size.scale / ui() } });
    } else {
      // the face keeps the size it is drawn at under the hand
      setLive({ at: { x: s.left + dx, y: s.top + dy }, size: keepSize(s) });
    }
  };
  const onUp = (e: PointerEvent) => {
    if (!press || e.pointerId !== press.id) return;
    const p = press;
    press = null;
    setMoving(false);
    if (frame.hasPointerCapture(e.pointerId)) frame.releasePointerCapture(e.pointerId);
    if (!p.moved) return;
    const b = box();
    const base = faces()[props.kind] ?? EMPTY;
    if (p.resize) {
      setFitHold(null);
      commit({ at: b.home ? null : { x: b.left, y: b.top }, size: keepSize(b) });
    } else if (nearHome(input(base, false), b)) {
      setFitHold(null);
      commit({ at: null, size: base.size });
    } else {
      setFitHold(keepSize(b));
      commit({ at: { x: b.left, y: b.top }, size: base.size });
    }
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
    if (e.key === 'Escape') {
      // with the focus anywhere in the face: it closes, and the Escape is spent here
      e.preventDefault();
      e.stopPropagation();
      props.close(true);
      return;
    }
    // the face itself: a control's keys are its own
    if (e.target !== frame || e.altKey || e.ctrlKey || e.metaKey) return;
    const b = box();
    const base = faces()[props.kind] ?? EMPTY;
    const step = e.shiftKey ? 40 : 8;
    const nudge = { ArrowLeft: [-step, 0], ArrowRight: [step, 0], ArrowUp: [0, -step], ArrowDown: [0, step] }[e.key];
    if (nudge) {
      setFitHold(keepSize(b));
      commit({ at: { x: b.left + nudge[0], y: b.top + nudge[1] }, size: base.size });
    } else if (e.key === '+' || e.key === '=' || e.key === '-') {
      setFitHold(null);
      const size = stepSize({ narrow: b.narrow, scale: b.scale }, ui(), e.key !== '-');
      commit({ at: base.at, size: { narrow: size.narrow, k: size.scale / ui() } });
    } else if (e.key === 'Home') {
      setFitHold(null);
      commit(EMPTY);
    } else return;
    e.preventDefault();
    e.stopPropagation();
  };

  const powered = () => !props.unit().bypass;
  return (
    <div class="uface-place" style={{ 'z-index': faceLayer(id()) + 1 }}>
      <div
        ref={(el) => {
          frame = el;
          // a press anywhere in it, a control's included, brings it to the top
          el.addEventListener('pointerdown', () => raiseFace(id()), true);
          queueMicrotask(() => el.focus());
        }}
        class={`uface ${props.kind}`}
        classList={{ 'uface--byp': !powered(), 'uface--narrow': box().narrow, 'uface--moving': moving() }}
        role="dialog"
        aria-label={`${props.name}, slot ${props.slotLetter()}`}
        aria-keyshortcuts="ArrowLeft ArrowRight ArrowUp ArrowDown + - Home Escape"
        id={`${props.kind}-face-${id()}`}
        tabindex={-1}
        style={{ left: `${box().left}px`, top: `${box().top}px`, scale: String(box().scale) }}
        onFocusIn={() => raiseFace(id())}
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
          classList={{ 'is-on': powered() }}
          aria-pressed={powered()}
          aria-label={`${props.name} in slot ${props.slotLetter()} on`}
          title={powered() ? 'On: press to bypass' : 'Bypassed: press to switch on'}
          disabled={props.unit().state !== 'seated'}
          onClick={() => toggleUnit(id())}
        />
        <span class="uface__maker" aria-hidden="true">
          BLEEPLOOP
        </span>
        <button type="button" class="uface__close" aria-label={`Close the ${props.name}`} onClick={(e) => props.close(e.detail === 0)}>
          <svg width="12" height="12" viewBox="0 0 12 12" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" aria-hidden="true">
            <path d="M2 2l8 8M10 2L2 10" />
          </svg>
        </button>
        <i class="uface__grip" aria-hidden="true" data-control title="Drag to resize" onPointerDown={(e) => begin(e, true)} />
      </div>
    </div>
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

/** A knob's centre and size on the face. */
interface KnobGeo {
  x: number;
  y: number;
  size: number;
}
/** A knob on a face: the param it sends, its legend and accessible name, its default and its top (the
 * engine's ceiling: FEEDBACK's 0.95), its centre and size on the full face and on the narrow one, the hero's
 * LED ring or a small one's ticks, and a band triangle beside its legend (the OTT's UP and DOWN). It reads
 * in percent. One def per knob for both layouts: a reflow moves the knob, never mounts another (the focus
 * stays on it). */
export interface KnobDef {
  param: UnitParamId;
  label: string;
  /** Its accessible name after the unit's name ("depth": "OTT depth"). */
  name: string;
  def: number;
  max?: number;
  wide: KnobGeo;
  narrow: KnobGeo;
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
 * `SetUnitParam` and shows the feed's value; only while a drag holds it does it follow the hand, and the
 * drag's end (a release, a lost capture, the knob disabled under it) hands it back to the feed. */
export function FaceKnob(props: { def: KnobDef; narrow: () => boolean; unit: () => UnitState; unitName: string }) {
  const k = props.def;
  const max = k.max ?? 1;
  const geo = () => (props.narrow() ? k.narrow : k.wide);
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
  let drag: { id: number; y: number; from: number; el: HTMLElement } | null = null;
  const letGo = (returnToFrame = false) => {
    const el = drag?.el;
    drag = null;
    setHeld(null);
    if (returnToFrame && el && document.activeElement === el) {
      el.closest<HTMLElement>('.uface')?.focus({ preventScroll: true });
    }
  };
  // disabled under a drag (the unit unseated): the drag ends, the knob reads the feed
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
    drag = { id: e.pointerId, y: e.clientY, from: value(), el };
    setHeld(value());
  };
  const onMove = (e: PointerEvent) => {
    if (!drag || drag.id !== e.pointerId || !enabled()) return;
    const v = drag.from + ((drag.y - e.clientY) / DRAG_PX) * max * (e.shiftKey ? 0.25 : 1);
    setHeld(send(v));
  };
  const onUp = (e: PointerEvent) => {
    if (drag?.id === e.pointerId) letGo(true);
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
  const ringW = () => geo().size + 40;
  const dot = (i: number) => {
    const t = ((-135 + (270 * i) / (RING - 1)) * Math.PI) / 180;
    const r = geo().size / 2 + 13;
    return { left: `${(ringW() / 2 + r * Math.sin(t)).toFixed(2)}px`, top: `${(ringW() / 2 - r * Math.cos(t)).toFixed(2)}px` };
  };
  return (
    <div
      class="uface__ctl"
      classList={{ 'uface__ctl--hero': k.hero }}
      style={{ '--x': `${geo().x}px`, '--y': `${geo().y}px`, '--s': `${geo().size}px` }}
    >
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
        style={knobStyle(angle(), geo().size, k.hero)}
        onPointerDown={onDown}
        onPointerMove={onMove}
        onPointerUp={onUp}
        onPointerCancel={onUp}
        on:lostpointercapture={onUp}
        onKeyDown={onKey}
        onDblClick={() => enabled() && send(k.def)}
        ref={(el) => {
          el.addEventListener('wheel', wheel, { passive: false });
          onCleanup(() => el.removeEventListener('wheel', wheel));
        }}
      >
        <Show when={k.hero} fallback={<Ticks size={geo().size} />}>
          <div class="uface__ring" style={{ width: `${ringW()}px`, height: `${ringW()}px` }} aria-hidden="true">
            <For each={Array.from({ length: RING }, (_, i) => i)}>
              {(i) => <i classList={{ 'is-on': i / (RING - 1) <= travel() + 1e-6 }} style={dot(i)} />}
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
function Ticks(props: { size: number }) {
  const w = () => props.size + 36;
  const c = () => w() / 2;
  return (
    <svg class="uface__ticks" width={w()} height={w()} aria-hidden="true">
      <For each={Array.from({ length: 11 }, (_, i) => i)}>
        {(i) => {
          const q = ((-135 + 27 * i) * Math.PI) / 180;
          const major = i === 0 || i === 10 || i === 5;
          const r0 = () => props.size / 2 + 4;
          const r1 = () => r0() + 6 + (major ? 1.5 : 0);
          return (
            <line
              x1={c() + r0() * Math.sin(q)}
              y1={c() - r0() * Math.cos(q)}
              x2={c() + r1() * Math.sin(q)}
              y2={c() - r1() * Math.cos(q)}
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
