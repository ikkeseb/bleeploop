import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { defineConfig } from 'vite';
import solid from 'vite-plugin-solid';

// Tauri sets this when running `tauri dev` over the network; harmless in a plain browser.
const tauriDevHost = process.env.TAURI_DEV_HOST;

// Help's "About this build" and its copied diagnostics name the version and the commit a tester runs.
const appVersion: string = JSON.parse(readFileSync(new URL('./package.json', import.meta.url), 'utf8')).version;

/** The short commit the bundle is built from: CI's GITHUB_SHA, else git, else "unknown". A missing git
 * never fails a build. */
function buildCommit(): string {
  const sha = process.env.GITHUB_SHA;
  if (sha) return sha.slice(0, 7);
  try {
    return execFileSync('git', ['rev-parse', '--short', 'HEAD'], { stdio: ['ignore', 'pipe', 'ignore'] }).toString().trim() || 'unknown';
  } catch {
    return 'unknown';
  }
}

// COOP/COEP make `self.crossOriginIsolated === true`, which unlocks SharedArrayBuffer +
// Atomics — required by the looper's lock-free capture ring buffer (ringbuf.js). Set here
// from day one (P0) and mirrored on the Tauri asset protocol later (P7). All assets are
// local/offline, so COEP:require-corp has no downside here.
const crossOriginIsolation = {
  'Cross-Origin-Opener-Policy': 'same-origin',
  'Cross-Origin-Embedder-Policy': 'require-corp',
} as const;

export default defineConfig({
  plugins: [solid()],
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
    headers: { ...crossOriginIsolation },
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
    headers: { ...crossOriginIsolation },
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
