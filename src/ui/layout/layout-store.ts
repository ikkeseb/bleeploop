import { createSignal } from 'solid-js';

/**
 * Layout state for the resizable / movable stage. Three things persist across reloads (localStorage,
 * mirroring audio-settings.ts — best-effort, swallows quota/private-mode throws). A legacy stored
 * `slotSizes` key (a deleted instrument-slot width split) still parses fine — unknown keys are ignored:
 *
 *  - `stageSizes`   — fr weights for the vertical stage stack, keyed by region id. Only the
 *                     'keyboard' / 'looper' regions are weighted; the instrument source row hugs its
 *                     content (SplitStack `autoSize`) and takes no weight. A missing id defaults to
 *                     weight 1; a legacy 'instrument' key still parses but is ignored.
 *  - `keyboardPlacement` — where the on-screen keyboard sits: 'top' (between the slots and the looper),
 *                     'bottom' (below the looper — the DEFAULT), or 'hidden' (removed — restore from
 *                     the command bar's keyboard tool).
 *  - `lastVisiblePlacement` — where a hidden keyboard returns, including across reloads.
 *
 * The weights are relative (fr), so a record like { keyboard: 0.55, looper: 1.4 } is fine — SplitStack
 * normalises against the sum of the *present* weighted regions, which is why hiding the keyboard doesn't
 * disturb the looper's share.
 */

export type KeyboardPlacement = 'top' | 'bottom' | 'hidden';

const STORAGE_KEY = 'lf.layout';

interface LayoutState {
  stageSizes: Record<string, number>;
  keyboardPlacement: KeyboardPlacement;
  lastVisiblePlacement: 'top' | 'bottom';
}

/**
 * First-run / reset baseline for the vertical stage stack. Only the keyboard + looper regions carry stage
 * weight; the instrument source row hugs its content (SplitStack `autoSize`) and is NOT in this map. The
 * looper IS the instrument, so it gets the bulk of the weighted area; the on-screen keyboard is a fallback,
 * so it's a slim strip. SplitStack normalises against the *present* weighted regions, so these hold whether
 * the keyboard sits top/bottom or is hidden (keyboard hidden → looper is the sole weighted region). Seeded
 * into `stageSizes` ONLY when no layout is persisted yet (or a stored one is corrupt) — a user's saved
 * layout is never overwritten. The same ratio also flows to each stage panel's `defaultWeight`, so a
 * double-click / Home / Enter on the keyboard↔looper divider resets that pair toward this layout. Tunable:
 * bump `looper` for an even more looper-dominant stage.
 */
export const DEFAULT_STAGE_WEIGHTS: Record<string, number> = {
  // The instrument source row hugs its content (autoSize) and holds NO stage weight — that kills
  // the dead band a fixed fr weight left between the source row and lane 1. Only the keyboard/looper pair is
  // weighted: looper = 2.0 / (0.55 + 2.0) ≈ 78% of the weighted area (the stage below the compact source
  // row); keyboard hidden → looper is the sole weighted region (100%). A user's persisted layout isn't
  // overwritten — reset the keyboard↔looper divider to pick this up. Legacy layouts that still carry an
  // `instrument` key parse fine (it's simply ignored now that the panel is autoSize).
  keyboard: 0.55,
  looper: 2.0,
};

function isWeightRecord(v: unknown): v is Record<string, number> {
  return (
    !!v &&
    typeof v === 'object' &&
    Object.values(v as Record<string, unknown>).every((x) => typeof x === 'number' && Number.isFinite(x) && x > 0)
  );
}

function read(): LayoutState {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    // No persisted layout → seed the looper-hero baseline + keyboard at the bottom (fresh install /
    // cleared storage). 'bottom' is the default placement (the keyboard is the fallback play path, so it
    // sits under the looper rather than between the source row and the lanes).
    if (!raw)
      return {
        stageSizes: { ...DEFAULT_STAGE_WEIGHTS },
        keyboardPlacement: 'bottom',
        lastVisiblePlacement: 'bottom',
      };
    const p = JSON.parse(raw) as Partial<LayoutState>;
    // An explicitly-stored 'top'/'bottom'/'hidden' is honored; anything else falls back to the 'bottom'
    // default (persisted layouts are untouched — this only governs a missing/invalid stored value).
    const placement: KeyboardPlacement =
      p.keyboardPlacement === 'bottom' || p.keyboardPlacement === 'hidden' || p.keyboardPlacement === 'top'
        ? p.keyboardPlacement
        : 'bottom';
    // Backward compatibility: old records have no lastVisiblePlacement. A visible stored placement is
    // authoritative; an old hidden record has no recoverable position and falls back to bottom.
    const lastVisiblePlacement =
      p.lastVisiblePlacement === 'top' || p.lastVisiblePlacement === 'bottom'
        ? p.lastVisiblePlacement
        : placement === 'top' || placement === 'bottom'
          ? placement
          : 'bottom';
    return {
      // A VALID stored stageSizes is honored verbatim (never overwrite a user's saved layout); a corrupt
      // one degrades to the seeded baseline rather than to flat-equal.
      stageSizes: isWeightRecord(p.stageSizes) ? p.stageSizes : { ...DEFAULT_STAGE_WEIGHTS },
      keyboardPlacement: placement,
      lastVisiblePlacement,
    };
  } catch {
    return {
      stageSizes: { ...DEFAULT_STAGE_WEIGHTS },
      keyboardPlacement: 'bottom',
      lastVisiblePlacement: 'bottom',
    };
  }
}

const initial = read();
const [stageSizes, setStageSizesSig] = createSignal<Record<string, number>>(initial.stageSizes);
const [keyboardPlacement, setKeyboardPlacementSig] = createSignal<KeyboardPlacement>(initial.keyboardPlacement);

// Where the keyboard goes back to when un-hidden (so the command-bar toggle restores its last visible spot).
let lastVisiblePlacement: 'top' | 'bottom' = initial.lastVisiblePlacement;

function persist(): void {
  try {
    localStorage.setItem(
      STORAGE_KEY,
      JSON.stringify({
        stageSizes: stageSizes(),
        keyboardPlacement: keyboardPlacement(),
        lastVisiblePlacement,
      } satisfies LayoutState),
    );
  } catch {
    /* persistence is best-effort */
  }
}

export { stageSizes, keyboardPlacement };

export function setStageSizes(next: Record<string, number>): void {
  setStageSizesSig(next);
  persist();
}

export function setKeyboardPlacement(p: KeyboardPlacement): void {
  if (p !== 'hidden') lastVisiblePlacement = p;
  setKeyboardPlacementSig(p);
  persist();
}

/** True when the keyboard occupies a stage region (top or bottom). */
export const keyboardVisible = (): boolean => keyboardPlacement() !== 'hidden';

/** Command-bar toggle: hide the keyboard, or restore it to its last visible placement. */
export function toggleKeyboardHidden(): void {
  setKeyboardPlacement(keyboardPlacement() === 'hidden' ? lastVisiblePlacement : 'hidden');
}

/** Keyboard bar control: swap between sitting above the looper and below it (no-op while hidden). */
export function moveKeyboard(): void {
  const p = keyboardPlacement();
  if (p === 'top') setKeyboardPlacement('bottom');
  else if (p === 'bottom') setKeyboardPlacement('top');
}
