import { createSignal } from 'solid-js';
import { notifyError, notifyInfo } from '../notify';
import { platform, type AppUpdate } from '../platform';
import { rigRecallOnClose } from '../ui/state/rig-recall';
import { jamInProgress, saveForQuit } from './close-guard';

/**
 * The updater's UI half (the native half: `src-tauri/src/update.rs`). At launch a release build asks
 * for a newer version, quietly when offline; an offer shows as a dot on the Help cap, one toast and an
 * "Update ready" section at the top of Help. Nothing installs until the player presses it there: the
 * installer quits the app, so the jam is saved for recovery first, as a close saves it.
 */
const [offered, setOffered] = createSignal<AppUpdate | null>(null);
const [installing, setInstalling] = createSignal(false);

export { offered as updateOffered, installing as updateInstalling };

/** Ask once, at launch. */
export async function checkForUpdate(): Promise<void> {
  if (!platform.updates.available) return;
  let update: AppUpdate | null;
  try {
    update = await platform.updates.check();
  } catch {
    return; // offline or GitHub unreachable: the native side logs why, and the next launch asks again
  }
  if (!update) return;
  setOffered(update);
  notifyInfo(`BleepLoop v${update.version} is ready: open Help (?) to update`);
}

/** The release notes as Help lists them: one line per bullet, the markdown emphasis dropped. */
export function updateNoteLines(notes: string): string[] {
  return notes
    .split('\n')
    .map((line) => line.trim().replace(/^- /, '').replaceAll('**', ''))
    .filter((line) => line !== '');
}

/** Help's UPDATE AND RESTART. */
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
    setInstalling(false);
  }
}
