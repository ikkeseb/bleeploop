/**
 * Where a unit's face stands and how large it is (`UnitFace.tsx`), as plain arithmetic. A face opens at
 * its home, hanging from its plate under the slot row (the plate's centre over `TIE_AT` on the face); the
 * player may move it anywhere in the window and scale it from its corner. Two layouts, one family: the
 * full face (900x540) and its narrow reflow (900x382, a shorter glass, smaller knobs, the same legends).
 * The scale never takes the type under its base size (the chrome's size step `ui`): where the full face
 * would go under it, the narrow one takes over, and the narrow one at `ui` is the floor. Only past the
 * app's minimum window (no room for the narrow face at `ui`) does a face scale under it. The face never
 * leaves the window: every placement clamps it in.
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
/** A move released this close to home lands home (its strap back). */
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
  return { narrow: true, scale: r >= ui ? clamp(want.scale, ui, r) : Math.max(0.3, r) };
}

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
    const { narrow, scale } = fit(want, i.ui, room);
    const home = homeOf(i, narrow ? NARROW : WIDE, scale);
    return { ...home, scale, narrow, home: true };
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
 * The size a corner drag asks for: the face's corner follows the pointer at (`cx`, `cy`). The scale is the
 * best fit of the corner to the pointer (the anchor stays: the face's top, and its left, or at home the
 * plate's centre over `TIE_AT`). A box much wider than tall asks for the narrow face, and so does a full
 * face pushed under its base size; the narrow face never goes under it.
 */
export function sizeFromCorner(start: FaceBox, i: FaceInput, cx: number, cy: number): FaceSize {
  const ax = start.home ? TIE_AT : 0;
  const anchorX = start.home ? i.plate.left + i.plate.width / 2 : start.left;
  const bw = cx - anchorX;
  const bh = cy - start.top;
  // least squares: the scale whose corner, (w - ax, h) times it, lands nearest the pointer
  const fitOf = (l: FaceLayout) => (bw * (l.w - ax) + bh * l.h) / ((l.w - ax) ** 2 + l.h ** 2);
  const wide = fitOf(WIDE);
  const aspect = (bw * WIDE.w) / (WIDE.w - ax) / Math.max(1, bh);
  if (aspect > 2 || wide < i.ui * 0.97) return { narrow: true, scale: Math.max(i.ui, fitOf(NARROW)) };
  return { narrow: false, scale: Math.max(i.ui, wide) };
}

/** One keyboard step of the face's size: up or down by a tenth; down from the full face's base size the
 * narrow face takes over. */
export function stepSize(cur: FaceSize, ui: number, up: boolean): FaceSize {
  if (up) return { narrow: cur.narrow, scale: cur.scale * 1.1 };
  const s = cur.scale / 1.1;
  if (!cur.narrow && s < ui - 1e-9) return { narrow: true, scale: ui };
  return { narrow: cur.narrow, scale: Math.max(ui, s) };
}
