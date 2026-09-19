/**
 * DEV probe: does every installed plugin's editor open into the host window and close again
 * without a hang? Loads each scanned plugin into slot 0, opens its editor (the UI's `embedded`
 * request; CLAP plugins that offer a floating window take that path), holds it, closes it, and
 * unloads — timing each step so a wedge shows up as a missing `closed` line, not a silent stall.
 * `[smoke]` lines go through `console.error` (→ the same log as the Rust host), so they sit next to
 * the host's `editor embedded into host window (WxH)` / `editor closed` lines.
 *
 * Trigger: `VITE_LF_PROBE=editor-smoke` at Vite start (DEV only). Knobs:
 *   `VITE_LF_PROBE_FILTER`  comma-separated name substrings; only matching plugins are probed
 *   `VITE_LF_PROBE_HOLD`    ms the editor stays open (default 1500)
 */
import { engine } from '../audio/engine';
import { availablePlugins, clearPlugin, selectPlugin } from '../audio/instrument';
import { nativeHostReady, slotPlugins } from '../audio/instrument-slots';
import { platform } from '../platform';

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
const log = (...args: unknown[]) => console.error('[smoke]', ...args);

export async function runEditorSmoke(): Promise<void> {
  const filter = (import.meta.env.VITE_LF_PROBE_FILTER as string | undefined)
    ?.split(',')
    .map((s) => s.trim().toLowerCase())
    .filter(Boolean);
  const hold = Number(import.meta.env.VITE_LF_PROBE_HOLD ?? 1500) || 1500;
  for (let i = 0; i < 600 && !(nativeHostReady() && availablePlugins().length); i++) await sleep(100);
  const list = availablePlugins().filter(
    (d) => !filter?.length || filter.some((f) => d.name.toLowerCase().includes(f)),
  );
  if (!list.length) {
    log('no plugins to probe (scan empty or filter matched nothing)');
    return;
  }
  await engine.start();
  log(`start: ${list.length} plugin(s), hold=${hold} ms`);
  let opened = 0;
  let failed = 0;
  for (const desc of list) {
    const tag = `${desc.name} [${desc.format}]`;
    log(`load ${tag}`);
    await selectPlugin(0, desc);
    if (slotPlugins()[0]?.id !== desc.id) {
      log(`LOAD FAILED ${tag}`);
      failed++;
      continue;
    }
    await sleep(1500);
    const t0 = performance.now();
    try {
      await platform.pluginHost.openEditor(0, 'embedded');
      log(`opened ${tag} in ${Math.round(performance.now() - t0)} ms`);
      opened++;
    } catch (e) {
      log(`OPEN FAILED ${tag}: ${String(e)}`);
      failed++;
    }
    await sleep(hold);
    const t1 = performance.now();
    try {
      await platform.pluginHost.closeEditor(0);
      log(`closed ${tag} in ${Math.round(performance.now() - t1)} ms`);
    } catch (e) {
      log(`CLOSE FAILED ${tag}: ${String(e)}`);
      failed++;
    }
    await sleep(500);
    await clearPlugin(0);
    await sleep(500);
    log(`unloaded ${tag}`);
  }
  log(`complete: ${opened} opened, ${failed} failed, of ${list.length}`);
}
