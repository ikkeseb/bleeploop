/**
 * DEV probe: does switching plugins complete after the loaded one was tweaked? Every ordered pair of
 * the probed plugins is swapped IN PLACE in slot 0 (a fresh `selectPlugin` over the loaded plugin, as
 * the slot picker does — the other probes unload in between and never take this path), once with the
 * editor closed and once with it left open across the swap. Before each swap the first params are
 * swept min → max → default. Every step is timed and bounded, so a wedge shows up as a `TIMEOUT` line
 * naming the step instead of a silent stall; after a timeout the probe stops (the host is no longer
 * in a known state). `[swap]` lines go through `console.error` (→ the same log as the Rust host).
 *
 * Trigger: `VITE_LF_PROBE=swap-stress` at Vite start (DEV only). Knobs:
 *   `VITE_LF_PROBE_FILTER`  comma-separated name substrings; only matching plugins are probed
 *   `VITE_LF_PROBE_PARAMS`  how many params to sweep before a swap (default 8)
 */
import { engine } from '../audio/engine';
import { availablePlugins, clearPlugin, selectPlugin } from '../audio/instrument';
import { nativeHostReady, slotPlugins } from '../audio/instrument-slots';
import { platform, type PluginDescriptor } from '../platform';

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
const log = (...args: unknown[]) => console.error('[swap]', ...args);
const STEP_TIMEOUT_MS = 30000;

class StepTimeout extends Error {}

/** Run one step under a deadline; logs its duration, throws StepTimeout when it never returns. */
async function step<T>(name: string, run: () => Promise<T>): Promise<T> {
  const t0 = performance.now();
  let timer: ReturnType<typeof setTimeout> | undefined;
  const deadline = new Promise<never>((_, reject) => {
    timer = setTimeout(() => reject(new StepTimeout(name)), STEP_TIMEOUT_MS);
  });
  try {
    const result = await Promise.race([run(), deadline]);
    log(`${name}: ${Math.round(performance.now() - t0)} ms`);
    return result;
  } finally {
    clearTimeout(timer);
  }
}

async function tweak(tag: string, count: number): Promise<void> {
  const params = (await step(`listParams ${tag}`, () => platform.pluginHost.listParams(0))).slice(0, count);
  for (const p of params) {
    for (const v of [p.minValue, p.maxValue, p.value]) {
      await platform.pluginHost.setParameter(0, p.id, v);
      await sleep(30);
    }
  }
  log(`tweaked ${params.length} param(s) on ${tag}`);
}

export async function runSwapStress(): Promise<void> {
  const filter = (import.meta.env.VITE_LF_PROBE_FILTER as string | undefined)
    ?.split(',')
    .map((s) => s.trim().toLowerCase())
    .filter(Boolean);
  const paramCount = Number(import.meta.env.VITE_LF_PROBE_PARAMS ?? 8) || 8;
  for (let i = 0; i < 600 && !(nativeHostReady() && availablePlugins().length); i++) await sleep(100);
  const list = availablePlugins().filter(
    (d) => !filter?.length || filter.some((f) => d.name.toLowerCase().includes(f)),
  );
  if (list.length < 2) {
    log(`need at least 2 plugins, got ${list.length} (scan empty or filter too narrow)`);
    return;
  }
  await engine.start();
  const tagOf = (d: PluginDescriptor) => `${d.name} [${d.format}]`;
  log(`start: ${list.length} plugin(s), ${list.length * (list.length - 1) * 2} swap(s)`);
  let swaps = 0;
  let failed = 0;
  try {
    for (const editorOpen of [false, true]) {
      for (const from of list) {
        for (const to of list) {
          if (from === to) continue;
          const label = `${tagOf(from)} -> ${tagOf(to)} (editor ${editorOpen ? 'open' : 'closed'})`;
          log(`case ${label}`);
          await step(`load ${tagOf(from)}`, () => selectPlugin(0, from));
          if (slotPlugins()[0]?.id !== from.id) {
            log(`LOAD FAILED ${tagOf(from)}`);
            failed++;
            continue;
          }
          await sleep(1000);
          if (editorOpen) {
            try {
              await step(`openEditor ${tagOf(from)}`, () => platform.pluginHost.openEditor(0, 'embedded'));
            } catch (e) {
              if (e instanceof StepTimeout) throw e;
              log(`open failed (continuing without editor) ${tagOf(from)}: ${String(e)}`);
            }
            await sleep(800);
          }
          await tweak(tagOf(from), paramCount);
          await step(`swap ${label}`, () => selectPlugin(0, to));
          if (slotPlugins()[0]?.id !== to.id) {
            log(`SWAP FAILED ${label}`);
            failed++;
          } else swaps++;
          await sleep(800);
          await step(`unload ${tagOf(to)}`, () => clearPlugin(0));
          await sleep(500);
        }
      }
    }
    log(`complete: ${swaps} swapped, ${failed} failed`);
  } catch (e) {
    if (e instanceof StepTimeout) log(`TIMEOUT after ${STEP_TIMEOUT_MS} ms in: ${e.message} — probe stopped, ${swaps} swapped before it`);
    else log(`ABORTED: ${String(e)}`);
  }
}
