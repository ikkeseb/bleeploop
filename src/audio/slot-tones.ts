/**
 * OWNS: the plugin tones a session carries (engine mode). A TONE is a plugin's saved state; the native
 * host keeps one per plugin in its tone store and restores it inside every load by itself
 * (`src-tauri/src/host/tone.rs`), so rig recall brings the player's tone back with the plugin. This
 * module moves tones across a session export and import:
 *
 * - export takes each loaded slot's tone fresh, through the slot's owner (`takeSlotTones`);
 * - import stores each tone under the plugin session.json names for it (the host refuses a tone file of
 *   another plugin). A slot that holds that plugin now is reloaded in place so the load applies it,
 *   keeping its level and GO LIVE, unless the player picked another plugin for it while the import ran;
 *   a slot that holds another plugin or none is left alone, and one toast says which plugin to load
 *   (its next load, in either slot, restores the session's tone). The player's rig is never swapped
 *   (`restoreSessionTones`).
 *
 * The web audio path keeps no tones: both are no-ops there.
 */
import { engineMode, platform, type PluginFormat, type ToneImport } from '../platform';
import { notifyError, notifyInfo } from '../notify';
import { slotPlugins } from './instrument-slots';
import { reloadPlugin } from './instrument';

/** A slot's tone as a session carries it: the plugin it belongs to and the tone file's bytes. */
export interface SlotTone {
  slot: 0 | 1;
  plugin: { format: PluginFormat; path: string; id: string; name: string };
  bytes: Uint8Array;
}

/** The slot letters the UI and session.json use. */
export const slotLetter = (slot: 0 | 1): 'A' | 'B' => (slot === 0 ? 'A' : 'B');

/**
 * Each loaded slot's tone, saved fresh through its owner (the store gets it too). A plugin that keeps no
 * state has none; a slot whose save fails is left out of the export, logged and toasted once, and the
 * export goes on without it.
 */
export async function takeSlotTones(): Promise<SlotTone[]> {
  if (!engineMode()) return [];
  const tones: SlotTone[] = [];
  for (const slot of [0, 1] as const) {
    const desc = slotPlugins()[slot];
    if (!desc) continue;
    try {
      const bytes = await platform.pluginHost.takeTone(slot);
      if (!bytes) continue;
      const { format, path, id, name } = desc;
      tones.push({ slot, plugin: { format, path, id, name }, bytes });
    } catch (e) {
      console.error(`[slot-tones] slot ${slotLetter(slot)}: ${desc.name}'s tone could not be saved for the export`, e);
      notifyError(`The export has no saved settings for ${desc.name}`, e);
    }
  }
  return tones;
}

/**
 * Store a session's tones (import, after its loops loaded). For each: the native host stores it under
 * the plugin session.json names; a slot holding that plugin reloads to apply it (still holding it once
 * the host answered: `reloadPlugin`), any other slot is left alone with a toast naming the plugin to
 * load. A tone the host refuses (corrupt, not a tone, another plugin's) is logged and toasted; the
 * loops stay imported.
 */
export async function restoreSessionTones(tones: readonly SlotTone[]): Promise<void> {
  if (!engineMode()) return;
  for (const tone of tones) {
    const letter = slotLetter(tone.slot);
    let stored: ToneImport;
    try {
      stored = await platform.pluginHost.importTone(tone.slot, tone.bytes, tone.plugin);
    } catch (e) {
      console.error(`[slot-tones] slot ${letter}: the session's tone for ${tone.plugin.name} could not be kept`, e);
      notifyError(`${tone.plugin.name}: the session's saved settings could not be kept`, e);
      continue;
    }
    if (!stored.held) {
      notifyInfo(`This session used ${stored.name} in slot ${letter} — load it to hear the session's tone`);
      continue;
    }
    if ((await reloadPlugin(tone.slot, tone.plugin)) === 'failed') {
      console.error(`[slot-tones] slot ${letter}: ${stored.name} did not come back after taking the session's tone`);
    }
  }
}
