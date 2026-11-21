/**
 * Where a unit's face stands and how large it is (`UnitFace.tsx`), as plain arithmetic. A face opens at
 * its home, just under the slot row below its plate (the plate's centre over `TIE_AT` on the face); the
 * player may move it anywhere in the window and scale it from its corner. Two layouts, one family: the
 * full face (900x540) and its narrow reflow (900x382, a shorter glass, smaller knobs, the same legends).
 * The scale never takes the type under its base size (the chrome's size step `ui`): where the full face
 * would go under it, the narrow one takes over, and the narrow one at `ui` is the floor. Where home has no
 * room for the narrow face at `ui` (a short window) or the scrolled plate puts home above the viewport
 * margin, the face detaches and fits the window from home's place; only a window that cannot hold the narrow
 * face at `ui` at all scales it under. The face never leaves the window: every placement clamps it in, its
 * every control inside.
 */

interface FaceLayout {
  w: number;
  h: number;
}
export const WIDE: FaceLayout = { w: 900, h: 540 };
export const NARROW: FaceLayout = { w: 900, h: 382 };
/** Where the plate's centre falls on the face at home (the face's x, at scale 1). */
const TIE_AT = 333;
/** The gap from the plate's foot to a face at home, and the window's margin round a face. */
const DROP = 19;
const MARGIN = 8;
/** A move released this close to home lands home. */
const SNAP = 12;

/** The size the player picked: the layout and its scale. */
interface FaceSize {
  narrow: boolean;
  scale: number;
}
/** Where the player put the face: its top-left in the window (CSS pixels). */
export interface FaceAt {
  x: number;
  y: number;
}
/** A face as drawn: its top-left, scale and layout, and whether it hangs at home. */
export interface FaceBox {
  left: number;
  top: number;
  scale: number;
  narrow: boolean;
  home: boolean;
}
/** What a placement reads: the plate's box, the window, the size step, the player's size and place. */
export interface FaceInput {
  plate: { left: number; width: number; bottom: number };
  win: { w: number; h: number };
  ui: number;
  size: FaceSize | null;
  at: FaceAt | null;
}

const clamp = (x: number, lo: number, hi: number): number => Math.min(hi, Math.max(lo, x));

/** The layout and scale for `want` where a layout fits at most `room(l)`. */
function fit(want: FaceSize, ui: number, room: (l: FaceLayout) => number): { narrow: boolean; scale: number } {
  if (!want.narrow) {
    const s = Math.min(want.scale, room(WIDE));
    if (s >= ui - 1e-9) return { narrow: false, scale: s };
  }
  const r = room(NARROW);
  return { narrow: true, scale: r >= ui ? clamp(want.scale, ui, r) : Math.max(FLOOR, r) };
}
/** The smallest scale a window too small for the narrow face at `ui` may take (never zero). */
const FLOOR = 0.05;

/** Home's top-left for a face of `scale` and layout `l`. */
function homeOf(i: FaceInput, l: FaceLayout, scale: number): { left: number; top: number } {
  const centre = i.plate.left + i.plate.width / 2;
  return { left: Math.max(MARGIN, Math.min(centre - TIE_AT * scale, i.win.w - MARGIN - l.w * scale)), top: i.plate.bottom + DROP };
}

/** Place a face: at home under its plate, or where the player put it, pulled into the window. */
export function placeFace(i: FaceInput): FaceBox {
  const want = i.size ?? { narrow: false, scale: i.ui };
  if (!i.at) {
    const top = i.plate.bottom + DROP;
    const room = (l: FaceLayout) => Math.min((i.win.w - 2 * MARGIN) / l.w, (i.win.h - top - MARGIN) / l.h);
    if (top >= MARGIN && room(NARROW) >= i.ui - 1e-9) {
      const { narrow, scale } = fit(want, i.ui, room);
      const home = homeOf(i, narrow ? NARROW : WIDE, scale);
      return { ...home, scale, narrow, home: true };
    }
    // no room under the plate, or plate scrolled above the margin: detach and fit to the window
    return placeFace({ ...i, at: { x: i.plate.left + i.plate.width / 2 - TIE_AT * i.ui, y: top } });
  }
  const room = (l: FaceLayout) => Math.min((i.win.w - 2 * MARGIN) / l.w, (i.win.h - 2 * MARGIN) / l.h);
  const { narrow, scale } = fit(want, i.ui, room);
  const l = narrow ? NARROW : WIDE;
  const left = Math.max(MARGIN, Math.min(i.at.x, i.win.w - MARGIN - l.w * scale));
  const top = Math.max(MARGIN, Math.min(i.at.y, i.win.h - MARGIN - l.h * scale));
  return { left, top, scale, narrow, home: false };
}

/** Whether a face released at `box` is close enough to its home to land there. */
export function nearHome(i: FaceInput, box: FaceBox): boolean {
  const home = placeFace({ ...i, at: null, size: { narrow: box.narrow, scale: box.scale } });
  return home.narrow === box.narrow && Math.abs(home.left - box.left) <= SNAP && Math.abs(home.top - box.top) <= SNAP;
}

/**
 * The size a corner drag asks for: the face's corner, as the face is placed at that size (its top, and its
 * left: a moved face's own, at home the plate's centre over `TIE_AT`, either clamped into the window),
 * lands nearest the pointer at (`cx`, `cy`), the pointer's x held inside the window and its y as the hand
 * made it (a corner pulled past the window's foot is deliberate growth, and the placement after fits the
 * result to the window). Its first pixels of drag move it by pixels, from wherever the face is drawn. A
 * full face pushed under its base size asks for the narrow face (its shape alone never does: a corner held
 * at the window's edge would ask for it); the narrow face never goes under its base size.
 */
export function sizeFromCorner(start: FaceBox, i: FaceInput, cx: number, cy: number): FaceSize {
  // The pointer's travel is measured as the hand made it: a corner pulled past the window's foot is
  // deliberate growth, and placement fits the result to the window after (it slides the face up).
  const px = Math.min(cx, i.win.w - MARGIN);
  const py = cy;
  const centre = i.plate.left + i.plate.width / 2;
  const corner = (l: FaceLayout, s: number): [number, number] => {
    const want = start.home ? centre - TIE_AT * s : start.left;
    const left = Math.max(MARGIN, Math.min(want, i.win.w - MARGIN - l.w * s));
    return [left + l.w * s, start.top + l.h * s];
  };
  const wide = nearest((s) => corner(WIDE, s), px, py);
  const narrow = nearest((s) => corner(NARROW, s), px, py);
  if (start.narrow) {
    // start in the displayed narrow layout; switch to full only after deliberate downward growth
    const startBottom = start.top + NARROW.h * start.scale;
    const tallEnough = py - start.top >= WIDE.h * Math.max(i.ui, start.scale * 0.95);
    if (wide.scale >= i.ui && py - startBottom >= 48 * i.ui && (tallEnough || wide.dist < narrow.dist)) {
      return { narrow: false, scale: Math.max(i.ui, wide.scale) };
    }
    return { narrow: true, scale: Math.max(i.ui, narrow.scale) };
  }
  if (wide.scale < i.ui * 0.97) return { narrow: true, scale: Math.max(i.ui, narrow.scale) };
  return { narrow: false, scale: Math.max(i.ui, wide.scale) };
}

/** The scale whose point `at(s)` lands nearest (`px`, `py`), and its squared distance: `at` is linear but
 * for a few clamps, so each of many short spans is solved as a line, and the nearest of them wins. */
function nearest(at: (s: number) => [number, number], px: number, py: number): { scale: number; dist: number } {
  const LO = 0.1;
  const HI = 6;
  const N = 120;
  let best = LO;
  let bestD = Infinity;
  for (let k = 0; k < N; k++) {
    const s0 = LO + ((HI - LO) * k) / N;
    const s1 = LO + ((HI - LO) * (k + 1)) / N;
    const [x0, y0] = at(s0);
    const [x1, y1] = at(s1);
    const bx = (x1 - x0) / (s1 - s0);
    const by = (y1 - y0) / (s1 - s0);
    const t = clamp(s0 + (bx * (px - x0) + by * (py - y0)) / (bx * bx + by * by || 1), s0, s1);
    const [x, y] = at(t);
    const d = (x - px) ** 2 + (y - py) ** 2;
    if (d < bestD) [best, bestD] = [t, d];
  }
  return { scale: best, dist: bestD };
}

/** One keyboard step of the face's size: up or down by a tenth; down from the full face's base size the
 * narrow face takes over, and up from the narrow face's the full face is back. */
export function stepSize(cur: FaceSize, ui: number, up: boolean): FaceSize {
  // up from the narrow face at its base size: the full face again
  if (up) return cur.narrow && cur.scale <= ui + 1e-9 ? { narrow: false, scale: ui } : { narrow: cur.narrow, scale: cur.scale * 1.1 };
  const s = cur.scale / 1.1;
  if (!cur.narrow && s < ui - 1e-9) return { narrow: true, scale: ui };
  return { narrow: cur.narrow, scale: Math.max(ui, s) };
}
