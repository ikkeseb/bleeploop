/**
 * DEV probe (temporary, D1's premise): with the same plugin file in BOTH slots, does an editor open
 * that the UI's affinity guard would refuse actually wedge? Bypasses the guard (`platform.pluginHost`
 * directly, as editor-smoke does). Loads the plugin into A and B, opens and closes A's editor, then:
 *   `reload` (default)  A goes Off and loads the same file again (a new owner thread), A's editor opens
 *   `other`             B's editor opens
 *   `b-first`           B's editor opens first, A's never did (is the module bound at load?)
 * Each open races a timer, so a wedge prints `complete: WEDGED ...` instead of a silent stall.
 *
 * Trigger: `VITE_LF_PROBE=editor-premise` at Vite start (DEV only). Knobs:
 *   `VITE_LF_PROBE_FILTER`  name substring of the plugin (default `Archetype`)
 *   `VITE_LF_PROBE_FORMAT`  `clap` | `vst3` | `vst2` (default: the first match)
 *   `VITE_LF_PROBE_CASE`    `reload` | `other` | `b-first`
 */
import { availablePlugins, clearPlugin, selectPlugin } from '../ui/state/instrument';
import { nativeHostReady, slotPlugins } from '../ui/state/instrument-slots';
import { platform } from '../platform';

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
const log = (...args: unknown[]) => console.error('[premise]', ...args);
const OPEN_WAIT_MS = 45_000;

async function open(slot: 0 | 1): Promise<string> {
  const t0 = performance.now();
  const timer = sleep(OPEN_WAIT_MS).then(() => 'no answer');
  const answer = platform.pluginHost.openEditor(slot).then(
    () => 'opened',
    (e: unknown) => `refused: ${String(e)}`,
  );
  const result = await Promise.race([answer, timer]);
  return `${result} after ${Math.round(performance.now() - t0)} ms`;
}

export async function runEditorPremise(): Promise<void> {
  const filter = ((import.meta.env.VITE_LF_PROBE_FILTER as string | undefined) || 'Archetype').toLowerCase();
  const format = (import.meta.env.VITE_LF_PROBE_FORMAT as string | undefined)?.toLowerCase();
  const knob = import.meta.env.VITE_LF_PROBE_CASE as string | undefined;
  const which = knob === 'other' || knob === 'b-first' ? knob : 'reload';
  for (let i = 0; i < 600 && !(nativeHostReady() && availablePlugins().length); i++) await sleep(100);
  const desc = availablePlugins().find(
    (d) => d.name.toLowerCase().includes(filter) && (!format || d.format.toLowerCase() === format),
  );
  if (!desc) return log(`FAIL no plugin matches ${filter}${format ? ` [${format}]` : ''}`);
  const tag = `${desc.name} [${desc.format}]`;
  log(`start: ${tag}, case ${which}`);
  await selectPlugin(0, desc);
  await selectPlugin(1, desc);
  if (slotPlugins()[0]?.id !== desc.id || slotPlugins()[1]?.id !== desc.id) return log('FAIL load into A and B');
  await sleep(1500);
  if (which === 'b-first') {
    const b = await open(1);
    return log(`complete: ${b.startsWith('opened') ? 'no wedge' : 'WEDGED'} (${which}, ${tag}): ${b}`);
  }
  const first = await open(0);
  log(`A open: ${first}`);
  if (!first.startsWith('opened')) return log(`FAIL A's first open ${first}`);
  await sleep(1500);
  await platform.pluginHost.closeEditor(0);
  log('A closed');
  await sleep(1000);
  let slot: 0 | 1 = 1;
  if (which === 'reload') {
    slot = 0;
    await clearPlugin(0);
    await sleep(1000);
    await selectPlugin(0, desc);
    if (slotPlugins()[0]?.id !== desc.id) return log('FAIL reload into A');
    log('A off and loaded again');
    await sleep(1500);
  }
  const second = await open(slot);
  log(`${slot === 0 ? 'A' : 'B'} open: ${second}`);
  log(`complete: ${second.startsWith('opened') ? 'no wedge' : 'WEDGED'} (${which}, ${tag}): ${second}`);
}
