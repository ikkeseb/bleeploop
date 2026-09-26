import { createSignal } from 'solid-js';

/**
 * OWNS: whether the stage view (`StageView.tsx`) is open. Session-only: every launch starts in the
 * normal UI, since a performance view that reopened by itself would hide the source slots and settings
 * a first screen needs. The command-bar cap, the `stageView` action (`src/app/actions.ts`: its key and
 * any learned pedal) and Escape all go through here.
 */
const [stageOpen, setStageOpen] = createSignal(false);
export { stageOpen, setStageOpen };

/** Open the stage view, or close it when open. */
export function toggleStage(): void {
  setStageOpen((v) => !v);
}
