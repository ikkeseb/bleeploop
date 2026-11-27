// Run from the repo root: node verify/linux-app/plugin-probe.mjs APP PLUGINS_DIR SCREENSHOT.png
// APP: a Tauri debug app built with build.devUrl http://127.0.0.1:14937 (docs/VERIFY.md § Linux plugin
// hosting); PLUGINS_DIR: verify/linux-plugins/build.sh's output. Everything is private: its own PipeWire
// server with two null sinks (nothing reaches the speakers), its own X display, a temporary HOME, and the
// test plugins as the only plugin folders. The page picks the CLAP test plugin in slot A and the VST3
// one in slot B through the slots' own pickers; the runner keeps a screenshot of the window.
import assert from 'node:assert/strict';
import { spawn, execFileSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { mkdir, mkdtemp } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { createInterface } from 'node:readline';
import { createServer } from 'vite';

const [binary, plugins, shot] = process.argv.slice(2);
assert(binary && plugins && shot, 'Arguments: APP PLUGINS_DIR SCREENSHOT.png');
const dir = await mkdtemp(join(tmpdir(), 'bleeploop-plugin-probe-'));
const runtime = join(dir, 'runtime');
await mkdir(runtime, { mode: 0o700 });
const children = [];
const stopAll = () => {
  for (const child of children.reverse()) {
    try { process.kill(-child.pid, 'SIGTERM'); } catch {}
  }
};
process.on('exit', stopAll);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const start = (cmd, args, env) => {
  const child = spawn(cmd, args, { env, detached: true, stdio: ['ignore', 'pipe', 'pipe'] });
  children.push(child);
  return child;
};

// A private PipeWire graph: the app's JACK client meets only these two null sinks.
const graph = { ...process.env, PIPEWIRE_RUNTIME_DIR: runtime, PIPEWIRE_REMOTE: 'pipewire-0', XDG_RUNTIME_DIR: runtime,
  XDG_CONFIG_HOME: runtime, PIPEWIRE_CONFIG_DIR: '/usr/share/pipewire', DISABLE_RTKIT: '1' };
for (const key of ['PIPEWIRE_QUANTUM', 'PIPEWIRE_LATENCY', 'PIPEWIRE_RATE', 'PIPEWIRE_PROPS', 'PIPEWIRE_CONFIG_NAME']) delete graph[key];
start('pipewire', [], graph);
for (let i = 0; !existsSync(join(runtime, 'pipewire-0')); i++) {
  assert(i < 200, 'the private PipeWire server did not open its socket');
  await sleep(25);
}
for (const node of ['fixture_in', 'fixture_out']) {
  execFileSync('pw-cli', ['create-node', 'adapter', `{ factory.name=support.null-audio-sink node.name=${node} node.description=${node} media.class=Audio/Sink object.linger=true audio.position=[ FL FR ] adapter.auto-port-config={ mode=dsp monitor=true position=preserve } }`], { env: graph, timeout: 5000 });
}

const server = await createServer({
  cacheDir: join(dir, 'vite'),
  server: { host: '127.0.0.1', port: 14937, strictPort: true, hmr: false, watch: null },
  plugins: [{
    name: 'private-linux-plugin-probe',
    transformIndexHtml: () => [
      { tag: 'script', injectTo: 'head-prepend', children: "localStorage.setItem('lf.masterVolume', '0'); localStorage.setItem('lf.clickVolume', '0');" },
      { tag: 'script', injectTo: 'body', attrs: { type: 'module', src: '/verify/linux-app/plugin-controls.mjs' } },
    ],
  }],
});
await server.listen();

const display = ':' + (90 + Math.floor(Math.random() * 9));
start(process.env.XVFB ?? 'Xvfb', [display, '-screen', '0', '1600x1000x24', '-nolisten', 'tcp'], process.env);
await sleep(500);
const home = join(dir, 'home');
const appEnv = { ...graph, DISPLAY: display, HOME: home, GDK_BACKEND: 'x11', LIBGL_ALWAYS_SOFTWARE: '1', WEBKIT_DISABLE_DMABUF_RENDERER: '1',
  XDG_DATA_HOME: join(home, 'data'), XDG_CONFIG_HOME: join(home, 'config'), XDG_CACHE_HOME: join(home, 'cache'), XDG_STATE_HOME: join(home, 'state'),
  CLAP_PATH: join(resolve(plugins), 'clap'), VST3_PATH: join(resolve(plugins), 'vst3') };
delete appEnv.WAYLAND_DISPLAY;
for (const key of ['XDG_DATA_HOME', 'XDG_CONFIG_HOME', 'XDG_CACHE_HOME', 'XDG_STATE_HOME']) await mkdir(appEnv[key], { recursive: true });
const app = start(resolve(binary), [], appEnv);
const outcome = await new Promise((done) => {
  const timer = setTimeout(() => done({ kind: 'failed', error: 'no verdict within 90 s' }), 90_000);
  for (const stream of [app.stdout, app.stderr]) {
    createInterface({ input: stream }).on('line', (line) => {
      if (/\[scan\]|plugin_host|error/i.test(line)) console.log(line);
      const at = line.indexOf('[linux-plugin-probe]');
      if (at === -1) return;
      const event = JSON.parse(line.slice(at + 20).trim().replace(/^"|"$/g, '').replaceAll('\\"', '"'));
      console.log('probe:', JSON.stringify(event));
      if (event.kind === 'done' || event.kind === 'failed') {
        clearTimeout(timer);
        done(event);
      }
    });
  }
  app.on('exit', (code) => done({ kind: 'failed', error: `the app exited (${code})` }));
});
if (outcome.kind === 'done') {
  await sleep(1500);
  execFileSync('import', ['-display', display, '-window', 'root', shot]);
  console.log('screenshot:', shot);
}
await server.close();
stopAll();
console.log('temporary folder:', dir);
assert.equal(outcome.kind, 'done', outcome.error);
console.log('PASS: both test plugins picked, loaded and listing their Gain in the app');
process.exit(0);
