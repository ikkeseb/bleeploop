import { createRoot, createSignal, onCleanup } from 'solid-js';

/**
 * How the slot row fits its window, measured, not guessed: each slot's whole signal line and slot A's four
 * input-effect plates while they fit; else the four fold into one IN FX plate whose popover holds them;
 * else one slot's line shows whole and the other folds to one plate at the row's end, a press on it
 * swapping the two (`shownSlot`, the row's own state: it never picks the MIDI slot); else both; else, where
 * the row draws each slot's chain (`state/chain.ts`), its plates go compact (`chainCompact`: no marks,
 * shorter names); else a chain still too long scrolls along its own line (`chainScroll`). What fits
 * depends on what the slots hold (a long plugin name, a refusal beside a plate), so `fitRow` tries the
 * arrangements widest first on every resize of the row and every change of what it holds, and keeps the
 * first whose lines fit. Each try is a synchronous signal write and a layout read, so no try is ever
 * painted.
 */
const fit = createRoot(() => {
  const [fx, setFx] = createSignal(false);
  const [slots, setSlots] = createSignal(false);
  const [shown, setShown] = createSignal<0 | 1>(0);
  const [compact, setCompact] = createSignal(false);
  const [scroll, setScroll] = createSignal(false);
  return { fx, setFx, slots, setSlots, shown, setShown, compact, setCompact, scroll, setScroll };
});

/** Whether the four input effects show as one IN FX plate. */
export const fxFolded = fit.fx;
/** Whether one slot's line folds to a plate (the row is too narrow for both). */
export const slotsFolded = fit.slots;
/** The slot whose line shows whole while `slotsFolded`. */
export const shownSlot = fit.shown;
export const setShownSlot = fit.setShown;
/** Whether the chain's plates show compact (the last arrangement, the chain drawn). */
export const chainCompact = fit.compact;
/** Whether a chain too long for its slot's share scrolls along its own line (the very last arrangement). */
export const chainScroll = fit.scroll;

const ARRANGEMENTS: readonly (readonly [fx: boolean, slots: boolean, compact: boolean, scroll: boolean])[] = [
  [false, false, false, false],
  [true, false, false, false],
  [false, true, false, false],
  [true, true, false, false],
  [true, true, true, false],
  [true, true, true, true],
];

/** Fit the slot row `el` (its ref in app.tsx) for as long as it is mounted. */
export function fitRow(el: HTMLElement): void {
  if (typeof ResizeObserver === 'undefined') return;
  let fitting = false;
  const refit = (): void => {
    if (fitting) return;
    fitting = true;
    for (const [fx, slots, compact, scroll] of ARRANGEMENTS) {
      fit.setFx(fx);
      fit.setSlots(slots);
      fit.setCompact(compact);
      fit.setScroll(scroll);
      if (el.scrollWidth <= el.clientWidth + 1) break;
    }
    // the tries' own DOM changes are no change of what the row holds
    mo.takeRecords();
    fitting = false;
  };
  const ro = new ResizeObserver(refit);
  // a plate or a control coming or going (a plugin loaded, a swap of the shown line). Text nodes are not:
  // a value's text, and the empty marker a popover's portal leaves in its place as it opens (a refit then
  // would remount the plate under it and close it).
  const element = (n: Node): boolean => n.nodeType === Node.ELEMENT_NODE;
  const mo = new MutationObserver((records) => {
    if (records.some((r) => [...r.addedNodes].some(element) || [...r.removedNodes].some(element))) refit();
  });
  ro.observe(el);
  mo.observe(el, { childList: true, subtree: true });
  queueMicrotask(refit);
  onCleanup(() => {
    ro.disconnect();
    mo.disconnect();
  });
}
