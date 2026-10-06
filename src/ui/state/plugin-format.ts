/**
 * OWNS: the plugin formats the app knows (`PluginFormat`, `src/platform/host.ts`), as one runtime
 * check for the two places that read a format from storage: a saved rig (`rig-recall.ts`) and a
 * session's `plugins` list (`session/session-schema.ts`). PURE: Node-importable, type-only imports.
 * The record is typed on the union, so a format added there fails to compile until it is listed here.
 */
import type { PluginFormat } from '../../platform';

const KNOWN: Record<PluginFormat, true> = { clap: true, vst3: true, vst2: true };

/** True for a format the app can host; anything else (an older or newer app's, a typo) is not. */
export function isPluginFormat(v: unknown): v is PluginFormat {
  return typeof v === 'string' && Object.hasOwn(KNOWN, v);
}
