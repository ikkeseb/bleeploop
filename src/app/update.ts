import { createSignal } from 'solid-js';
import { notifyError, notifyInfo } from '../notify';
import { platform, type AppUpdate, type UpdateProgress } from '../platform';
import { rigRecallOnClose } from '../ui/state/rig-recall';
import { jamInProgress, saveForQuit } from './close-guard';

/**
 * The updater's UI half (the native half: `src-tauri/src/update.rs`). At launch a release build asks
 * for a newer version, quietly when offline; an offer raises the command bar's update pill
 * (`src/ui/UpdatePill.tsx`) and one toast. Nothing installs until the player presses it there: the
 * installer quits the app, so the jam is saved for recovery first, as a close saves it. An install is
 * three waits long (download, signature, installer), so each reports itself and the pill's panel
 * shows where it is — a press with no read-out reads as a hang.
 */
const [offered, setOffered] = createSignal<AppUpdate | null>(null);
const [installing, setInstalling] = createSignal(false);
const [progress, setProgress] = createSignal<UpdateProgress | null>(null);

export { offered as updateOffered, installing as updateInstalling, progress as updateProgress };

/** Ask once, at launch. */
export async function checkForUpdate(): Promise<void> {
  if (!platform.updates.available) return;
  // Before the check, so the listener is up long before any press can reach the native side.
  platform.updates.onProgress(setProgress);
  let update: AppUpdate | null;
  try {
    update = await platform.updates.check();
  } catch {
    return; // offline or GitHub unreachable: the native side logs why, and the next launch asks again
  }
  if (!update) return;
  setOffered(update);
  notifyInfo(`BleepLoop v${update.version} is ready: press UPDATE in the command bar`);
}

/** The release notes as the update panel lists them: one line per bullet, the markdown emphasis dropped. */
export function updateNoteLines(notes: string): string[] {
  return notes
    .split('\n')
    .map((line) => line.trim().replace(/^- /, '').replaceAll('**', ''))
    .filter((line) => line !== '');
}

/** MB to one decimal, for the download's read-out (1 MB = 1024 KiB, as the installer's size reads). */
function megabytes(bytes: number): string {
  return (bytes / (1024 * 1024)).toFixed(1);
}

/** What the panel says while an install runs: the stage, with the download's own numbers. */
export function updateProgressLabel(stage: UpdateProgress): string {
  switch (stage.stage) {
    case 'downloading':
      return stage.total === null
        ? `Downloading ${megabytes(stage.downloaded)} MB`
        : `Downloading ${megabytes(stage.downloaded)} of ${megabytes(stage.total)} MB`;
    case 'verifying':
      return 'Checking the signature';
    case 'installing':
      return 'Installing — BleepLoop closes now';
  }
}

/** The bar's fill, 0–1, or null where there is no number to show (no size, or past the download). */
export function updateProgressFraction(stage: UpdateProgress): number | null {
  if (stage.stage === 'installing') return 1;
  if (stage.stage === 'verifying') return 1;
  if (stage.total === null || stage.total <= 0) return null;
  return Math.min(1, stage.downloaded / stage.total);
}

/** The update panel's UPDATE AND RESTART. */
export async function installUpdate(): Promise<void> {
  const update = offered();
  if (!update || installing()) return;
  if (
    jamInProgress() &&
    !window.confirm(
      `Update to v${update.version} now? BleepLoop closes, installs it and opens again. The latest committed loops will be saved locally. Audio still being recorded is not included.`,
    )
  ) {
    return;
  }
  setInstalling(true);
  try {
    if (!(await saveForQuit())) return;
    // A clean quit: a rig restored this launch comes back in the new version (`rig-recall.ts`).
    rigRecallOnClose();
    await platform.updates.install();
  } catch (error) {
    console.error('[app] update failed', error);
    notifyError('The update did not install', error);
  } finally {
    // A failure leaves the app running, so the read-out must not keep a stage on screen under a
    // button that is pressable again.
    setProgress(null);
    setInstalling(false);
  }
}
