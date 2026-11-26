import { createSignal, untrack } from 'solid-js';
import type { UnitId } from '../../platform';

/**
 * Which unit faces are open (`FaceHost.tsx` draws them, `UnitFace.tsx` is each one's frame), and the plates
 * they hang from. A face is free-standing: it is no popover of the row, so it outlives its plate (a slot
 * folded under it keeps it open) and stays open beside the looper and beside another face. One face per
 * kind (its place is kept per kind): opening a second unit's face of a kind closes the first. The last face
 * touched is on top (`stack`, the drawing order; the open list itself never reorders, so no face's DOM
 * moves under the focus or a pointer's capture).
 */

export type FaceKind = 'ott' | 'echo' | 'ring';
export interface OpenFace {
  unit: UnitId;
  kind: FaceKind;
}

/** A plate as its face needs it: its element (the face hangs from it at home) and the hand-back of the
 * focus to its name or its menu (the app's `returnFocus`, as the row's popovers hand it back). */
interface PlateRef {
  el: HTMLElement;
  /** Focus the plate's name (a keyboard close) or its menu (a close the unit forced). */
  focus: (to: 'name' | 'menu') => void;
}

const [open, setOpen] = createSignal<readonly OpenFace[]>([]);
const [stack, setStack] = createSignal<readonly UnitId[]>([]);
const [plates, setPlates] = createSignal<ReadonlyMap<UnitId, PlateRef>>(new Map());

/** The open faces, in the order they opened. */
export const openFaces = open;
/** Whether `unit`'s face is open. */
export const faceOpen = (unit: UnitId): boolean => open().some((f) => f.unit === unit);
/** `unit`'s face's place in the drawing order: the last touched highest. */
export const faceLayer = (unit: UnitId): number => stack().indexOf(unit);

// The changes below read the store untracked: an effect that opens, closes or registers (a plate's) never
// subscribes to what it writes.

/** Open `unit`'s face (closing another of its kind), or close it when it is open. */
export function toggleFace(unit: UnitId, kind: FaceKind): void {
  untrack(() => {
    if (faceOpen(unit)) return closeFace(unit);
    const others = open().filter((f) => f.kind !== kind);
    setOpen([...others, { unit, kind }]);
    setStack([...stack().filter((u) => others.some((f) => f.unit === u)), unit]);
  });
}

export function closeFace(unit: UnitId): void {
  untrack(() => {
    if (!faceOpen(unit)) return;
    setOpen(open().filter((f) => f.unit !== unit));
    setStack(stack().filter((u) => u !== unit));
  });
}

/** Bring `unit`'s face to the top (a press or the focus in it). */
export function raiseFace(unit: UnitId): void {
  const s = untrack(stack);
  if (s.at(-1) === unit || !s.includes(unit)) return;
  setStack([...s.filter((u) => u !== unit), unit]);
}

/** Register a plate while it is mounted; the face of its unit hangs from it. */
export function registerPlate(unit: UnitId, ref: PlateRef): () => void {
  setPlates(new Map(untrack(plates)).set(unit, ref));
  return () => {
    const now = untrack(plates);
    if (now.get(unit) !== ref) return;
    const next = new Map(now);
    next.delete(unit);
    setPlates(next);
  };
}
/** `unit`'s plate while it is mounted (a folded slot's plates are not). */
export const plateOf = (unit: UnitId): PlateRef | undefined => plates().get(unit);
