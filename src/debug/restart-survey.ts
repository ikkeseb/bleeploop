/**
 * DEV probe: which installed plugins raise a restart/rescan request, and when. Loads every scanned
 * plugin into slot 0 in turn, waits for load-time flags, sweeps each parameter through min → max →
 * default, then unloads. Everything it learns is written as `[survey]` lines through `console.error`
 * (→ `frontend_log` → the same stdout/log file as the Rust host), so a `tauri dev` log can be read
 * afterwards next to the host's `restartComponent(...)` / `request_restart` lines.
 *
 * Trigger: `VITE_LF_PROBE=restart-survey` at Vite start (DEV only). Optional knobs:
 *   `VITE_LF_PROBE_FILTER`  comma-separated name substrings; only matching plugins are surveyed
 *   `VITE_LF_PROBE_SETTLE`  ms to wait after each parameter's three sets (default 0 = fast pass)
 */
import { engine } from '../audio/engine';
import { availablePlugins, clearPlugin, selectPlugin } from '../audio/instrument';
import { nativeHostReady, slotPlugins } from '../audio/instrument-slots';
import { platform } from '../platform';

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
const log = (...args: unknown[]) => console.error('[survey]', ...args);

export async function runRestartSurvey(): Promise<void> {
  const filter = (import.meta.env.VITE_LF_PROBE_FILTER as string | undefined)
    ?.split(',')
    .map((s) => s.trim().toLowerCase())
    .filter(Boolean);
  const settle = Number(import.meta.env.VITE_LF_PROBE_SETTLE ?? 0) || 0;
  // Wait for the boot chain: host ready + scan published.
  for (let i = 0; i < 600 && !(nativeHostReady() && availablePlugins().length); i++) await sleep(100);
  const list = availablePlugins().filter(
    (d) => !filter?.length || filter.some((f) => d.name.toLowerCase().includes(f)),
  );
  if (!list.length) {
    log('no plugins to survey (scan empty or filter matched nothing)');
    return;
  }
  // Resume the ctx so the JS side drains the ring — the plugin then processes like it does live.
  await engine.start();
  log(`start: ${list.length} plugin(s), settle=${settle} ms`);
  for (const desc of list) {
    const tag = `${desc.name} [${desc.format}]`;
    log(`load ${tag} ${desc.path}`);
    const t0 = performance.now();
    await selectPlugin(0, desc);
    if (slotPlugins()[0]?.id !== desc.id) {
      log(`LOAD FAILED ${tag}`);
      continue;
    }
    log(`loaded ${tag} in ${Math.round(performance.now() - t0)} ms`);
    await sleep(2500); // load-time flags land within the owner loop's 2 s recv_timeout
    let params: Awaited<ReturnType<typeof platform.pluginHost.listParams>> = [];
    try {
      params = await platform.pluginHost.listParams(0);
    } catch (e) {
      log(`listParams failed ${tag}: ${String(e)}`);
    }
    log(`sweep ${tag}: ${params.length} params`);
    for (const p of params) {
      log(`  param ${p.id} "${p.name}" [${p.minValue}..${p.maxValue}] def ${p.defaultValue}`);
      for (const v of [p.minValue, p.maxValue, p.defaultValue]) {
        try {
          await platform.pluginHost.setParameter(0, p.id, v);
        } catch (e) {
          log(`  setParameter failed ${p.id}=${v}: ${String(e)}`);
        }
      }
      if (settle) await sleep(settle);
    }
    await sleep(2500);
    log(`sweep done ${tag}`);
    // The sweep ends every param at its default; the controller's live value (what listParams
    // reports and what the plugin GUI shows) must agree with what the host just set. Plugins may
    // quantise (stepped params), so count mismatches beyond a coarse tolerance.
    try {
      const after = await platform.pluginHost.listParams(0);
      const byId = new Map(params.map((p) => [p.id, p]));
      let mismatched = 0;
      let first = '';
      for (const q of after) {
        const p = byId.get(q.id);
        if (!p) continue;
        const span = Math.abs(p.maxValue - p.minValue) || 1;
        if (Math.abs(q.value - p.defaultValue) / span > 0.01) {
          mismatched++;
          if (!first) first = `${q.id} "${q.name}" got ${q.value} expected ${p.defaultValue}`;
        }
      }
      log(`value-check ${tag}: ${mismatched}/${after.length} differ from the last set value${first ? ` (first: ${first})` : ''}`);
    } catch (e) {
      log(`value-check failed ${tag}: ${String(e)}`);
    }
    await clearPlugin(0);
    await sleep(500);
    log(`unloaded ${tag}`);
  }
  log('complete');
}
