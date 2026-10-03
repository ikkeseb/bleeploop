import { createSignal } from 'solid-js';
import { readStoredNumber, writeStoredNumber } from '../state/persist';
import { STAGE_VIEWS } from './views';
import type { StageViewDef } from './visual';

/**
 * OWNS: whether the stage view (`StageView.tsx`) is open, and which of its looks (`views.ts`) shows.
 *
 * Open is session-only: every launch starts in the normal UI, since a performance view that reopened
 * by itself would hide the source slots and settings a first screen needs. The command-bar cap, the
 * `stageView` action (`src/app/actions.ts`: its key and any learned pedal) and Escape all go through
 * here.
 *
 * The look is a per-device preference, kept across launches (`persist.ts`, as the master and click
 * levels are) as its place in `STAGE_VIEWS`; a stored place the list no longer has falls back to the
 * first look.
 */
const [stageOpen, setStageOpen] = createSignal(false);
export { stageOpen, setStageOpen };

/** Open the stage view, or close it when open. */
export function toggleStage(): void {
  setStageOpen((v) => !v);
}

const VIEW_KEY = 'lf.stageView';
const [viewIndex, setViewIndex] = createSignal(Math.floor(readStoredNumber(VIEW_KEY, 0, 0, STAGE_VIEWS.length - 1)));

/** The look the stage view shows. */
export const currentStageView = (): StageViewDef => STAGE_VIEWS[viewIndex()];

/** Step to the next look, wrapping (the view switch, the V key, a learned pedal). Does nothing while
 * the stage view is closed. */
export function nextStageView(): void {
  if (!stageOpen()) return;
  const next = (viewIndex() + 1) % STAGE_VIEWS.length;
  setViewIndex(next);
  writeStoredNumber(VIEW_KEY, next);
}
