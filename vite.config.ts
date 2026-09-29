import { readFileSync } from 'node:fs';
import { defineConfig, type Plugin } from 'vite';
import solid from 'vite-plugin-solid';
import { buildCommit } from './scripts/build-commit.mjs';

// Tauri sets this when running `tauri dev` over the network; harmless in a plain browser.
const tauriDevHost = process.env.TAURI_DEV_HOST;

// Help's "About this build" and its copied diagnostics name the version and the commit a tester runs.
const appVersion: string = JSON.parse(readFileSync(new URL('./package.json', import.meta.url), 'utf8')).version;

/**
 * The dev document always answers 200, never 304. Until Stage 6 the dev server sent COOP/COEP, and a
 * browser profile that loaded the app then keeps them on its cached document through every 304 (a 304
 * updates the headers it carries and removes none): the page stays cross-origin isolated and refuses
 * the recovery worker, whose script carries no COEP, so autosave fails. One full answer replaces the
 * cached entry. The release build's assets come fresh through Tauri's protocol and need none of this.
 */
const freshDocument: Plugin = {
  name: 'fresh-document',
  configureServer(server) {
    server.middlewares.use((req, _res, next) => {
      if (req.headers.accept?.includes('text/html')) delete req.headers['if-none-match'];
      next();
    });
  },
};

export default defineConfig({
  plugins: [solid(), freshDocument],
  // Declared in src/env.d.ts.
  define: {
    __APP_VERSION__: JSON.stringify(appVersion),
    __APP_COMMIT__: JSON.stringify(buildCommit()),
  },
  // Tauri expects a fixed port and its own console; don't let Vite clear it.
  clearScreen: false,
  server: {
    // Tauri's conventional dev port (avoids the default 5173 used by other local projects).
    port: 1420,
    strictPort: true,
    host: tauriDevHost || false,
    hmr: tauriDevHost ? { protocol: 'ws', host: tauriDevHost, port: 1421 } : undefined,
    // src-tauri/ = Rust output; .claude/ holds agent worktrees (full repo copies — a file
    // change there must never reload the live app); logs/ = runtime-gate logs.
    watch: { ignored: ['**/src-tauri/**', '**/.claude/**', '**/logs/**'] },
  },
  preview: {
    port: 1420,
    // Mirror the dev server: fail loudly if 1420 is taken rather than silently rebind to 1421+.
    // A probe run directly (without `pnpm probe`) targets localhost:1420, so a moved preview would
    // measure a stale build with no error.
    strictPort: true,
  },
  // Vite matches env prefixes with startsWith — there is NO globbing, so 'TAURI_ENV_*' would never
  // match. Keep the intent (expose Tauri env vars to the frontend) with the correct literal prefix.
  envPrefix: ['VITE_', 'TAURI_ENV_'],
  build: {
    // WebView2 (v149 here) is far newer than chrome105; conservative floor for the Tauri build.
    target: process.env.TAURI_ENV_PLATFORM === 'windows' ? 'chrome105' : 'esnext',
    // Vite 8 bundles rolldown + oxc; esbuild is no longer included, so `true` (= oxc) is used.
    minify: !process.env.TAURI_ENV_DEBUG,
    sourcemap: !!process.env.TAURI_ENV_DEBUG,
  },
});
