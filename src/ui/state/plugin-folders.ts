/**
 * OWNS: the plugin scan's folder list as Audio Settings shows it: the host's built-in folders
 * (read-only) and the player's own, which the native host keeps (`src-tauri/src/host/folders.rs`).
 * A change is the host's to make and persist; this module mirrors its answer and asks for a scan
 * (`scanForPlugins`, which queues behind a running one). Empty in the browser build.
 */
import { createSignal } from 'solid-js';
import { platform, type PluginFolders } from '../../platform';
import { notifyError } from '../../notify';
import { scanForPlugins } from './instrument';

const [pluginFolders, setPluginFolders] = createSignal<PluginFolders>({ builtin: [], user: [], unsupported: [] });
// An add (its dialog open) or a remove in flight: the section's buttons wait for it.
const [pluginFoldersBusy, setPluginFoldersBusy] = createSignal(false);

// Counts the answers asked for and published: a read that a later read or a change overtook is
// dropped, so an old list never lands on a newer one.
let latest = 0;

/** Read the folders from the host (Audio Settings, each time it opens). */
export async function refreshPluginFolders(): Promise<void> {
  if (!platform.pluginHost.available) return;
  const mine = ++latest;
  try {
    const folders = await platform.pluginHost.pluginFolders();
    if (mine === latest) setPluginFolders(folders);
  } catch (e) {
    console.error('[plugin-folders] reading the plugin folders failed', e);
    notifyError('Could not read the plugin folders', e);
  }
}

/** One change at a time: the host's answer becomes the list, then a normal (not forced) scan walks
 * it. `null` (the dialog cancelled) changes nothing and scans nothing. */
async function change(request: () => Promise<PluginFolders | null>, failure: string): Promise<void> {
  if (!platform.pluginHost.available || pluginFoldersBusy()) return;
  setPluginFoldersBusy(true);
  let folders: PluginFolders | null;
  try {
    folders = await request();
  } catch (e) {
    console.error(`[plugin-folders] ${failure}`, e);
    notifyError(failure, e);
    return;
  } finally {
    setPluginFoldersBusy(false);
  }
  if (!folders) return;
  latest += 1;
  setPluginFolders(folders);
  await scanForPlugins();
}

/** Add a folder: the native host opens its folder dialog and takes the path from it. */
export function addPluginFolder(): Promise<void> {
  return change(() => platform.pluginHost.addPluginFolder(), 'Could not add the plugin folder');
}

/** Remove one of the player's folders, spelled as the list shows it. */
export function removePluginFolder(path: string): Promise<void> {
  return change(() => platform.pluginHost.removePluginFolder(path), 'Could not remove the plugin folder');
}

/** Read-only reactive accessors: the folders, and whether a change is in flight. */
export { pluginFolders, pluginFoldersBusy };
