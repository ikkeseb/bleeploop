/**
 * Module hooks that let plain Node load the real `src/` modules the way Vite does in the app.
 *
 * - `tone` resolves to `fake-tone.ts`; `*?worker&url` to a data module whose default export is the
 *   worklet's file URL (the fake `audioWorklet.addModule` imports it); `*?worker` to an empty class.
 * - Extensionless relative imports under `src/` get `.ts` (or `/index.ts`), as Vite resolves them.
 * - `solid-js` resolves with the `browser` condition: the server build never runs effects.
 * - `import.meta.env` in `src/` reads as `{ DEV: false }`, the production build, unless a guard sets
 *   `globalThis.__importMetaEnv` (read when a fresh generation evaluates).
 * - A `?g=N` query on an imported `src/` URL propagates to every `src/` module it imports, so each
 *   generation is a fresh module graph with fresh module-level state. Resolution and stripped
 *   source are cached, which keeps a fresh graph at a few milliseconds.
 */
import { registerHooks, stripTypeScriptTypes } from 'node:module';

const srcRoot = new URL('../../src/', import.meta.url).href;
const fakeTone = new URL('./fake-tone.ts', import.meta.url).href;
const GEN = /\?g=\d+$/;

interface Resolved {
  url: string;
  format?: string | null;
  shortCircuit: true;
}
type NextResolve = (specifier: string, context?: { parentURL?: string; conditions?: string[] }) => {
  url: string;
  format?: string | null;
};

const resolved = new Map<string, Resolved>();
const stripped = new Map<string, string>();

function dataModule(code: string): Resolved {
  return { url: `data:text/javascript,${encodeURIComponent(code)}`, shortCircuit: true };
}

function resolveUncached(
  specifier: string,
  context: { parentURL?: string; conditions?: string[] },
  next: NextResolve,
): Resolved {
  if (specifier === 'tone') return { url: fakeTone, shortCircuit: true };
  const parent = context.parentURL?.replace(GEN, '');
  if (specifier.endsWith('?worker&url')) {
    const file = new URL(specifier.slice(0, -'?worker&url'.length), parent).href;
    return dataModule(`export default ${JSON.stringify(file)};`);
  }
  if (specifier.endsWith('?worker')) return dataModule('export default class {}');
  if (specifier === 'solid-js' || specifier.startsWith('solid-js/')) {
    return { ...next(specifier, { ...context, conditions: ['browser', 'import', 'default'] }), shortCircuit: true };
  }
  const relative = specifier.startsWith('./') || specifier.startsWith('../');
  if (relative && parent?.startsWith(srcRoot) && !/\.[cm]?[jt]sx?$/.test(specifier)) {
    try {
      return { ...next(`${specifier}.ts`, context), shortCircuit: true };
    } catch {
      return { ...next(`${specifier}/index.ts`, context), shortCircuit: true };
    }
  }
  return { ...next(specifier, context), shortCircuit: true };
}

registerHooks({
  resolve(specifier, context, next) {
    const gen = context.parentURL?.match(GEN)?.[0] ?? '';
    const key = `${context.parentURL?.replace(GEN, '')}|${specifier}`;
    let hit = resolved.get(key);
    if (!hit) {
      hit = resolveUncached(specifier, context, next as NextResolve);
      resolved.set(key, hit);
    }
    if (gen && hit.url.startsWith(srcRoot) && !hit.url.includes('?')) return { ...hit, url: hit.url + gen };
    return hit;
  },
  load(url, context, next) {
    const base = url.replace(GEN, '');
    if (!base.startsWith(srcRoot) || !base.endsWith('.ts')) return next(url, context);
    let source = stripped.get(base);
    if (source === undefined) {
      const raw = String(next(base, context).source).replaceAll('import.meta.env', '(globalThis.__importMetaEnv ?? { DEV: false })');
      source = stripTypeScriptTypes(raw);
      stripped.set(base, source);
    }
    return { format: 'module', source, shortCircuit: true };
  },
});
