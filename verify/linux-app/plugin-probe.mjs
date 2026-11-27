// Run from the repo root: node verify/linux-app/plugin-probe.mjs APP PLUGINS_DIR SCREENSHOT.png [SURGE_DIR]
// APP: a Tauri debug app built with build.devUrl http://127.0.0.1:14937 (docs/VERIFY.md § Linux plugin
// hosting); PLUGINS_DIR: verify/linux-plugins/build.sh's output. Everything is private: its own PipeWire
// server with two null sinks (nothing reaches the speakers), its own X display, a temporary HOME, and the
// test plugins as the only plugin folders. The page picks the CLAP test plugin in slot A and the VST3
// one in slot B through the slots' own pickers, then Off in slot A unloads its plugin from the running
// engine; the runner keeps a screenshot of the window.
// SURGE_DIR (optional): an unpacked Surge XT Linux release (`lib/clap`, `lib/vst3`, `share/surge-xt`),
// added to the plugin folders. The page then picks Surge XT (CLAP) in slot A, whose editor opens with
// the pick; presses the slot's editor control to close it and again to reopen it; then picks Surge XT
// (VST3) there. At each open the runner finds the editor window on the display (`xprop`, by the
// plugin's name and the host's `WM_CLASS`), checks the window the app logged is alive, and screenshots
// the whole display beside SCREENSHOT.png; after each close (by the control, by the unload) that window
// must be gone (`xprop -id` fails with BadWindow; any other failure proves nothing and fails).
import assert from 'node:assert/strict';
import { spawn, execFileSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { mkdir, mkdtemp, symlink } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { createInterface } from 'node:readline';
import { createServer } from 'vite';

const [binary, plugins, shot, surge] = process.argv.slice(2);
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
      { tag: 'script', injectTo: 'head-prepend', children: `localStorage.setItem('lf.masterVolume', '0'); localStorage.setItem('lf.clickVolume', '0'); window.__lfEditorProbe = ${!!surge};` },
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
if (surge) {
  appEnv.CLAP_PATH += ':' + join(resolve(surge), 'lib', 'clap');
  appEnv.VST3_PATH += ':' + join(resolve(surge), 'lib', 'vst3');
  // Surge reads its factory data from ~/.local/share/surge-xt.
  await mkdir(join(home, '.local', 'share'), { recursive: true });
  await symlink(join(resolve(surge), 'share', 'surge-xt'), join(home, '.local', 'share', 'surge-xt'));
}
const shots = [];
const closes = [];
/** The editor window the app logged last (`editor window 0x… open`). */
let lastWindow = null;
/** Whether window `id` exists on the private display: `alive`, `gone` (only the server's BadWindow
 * says so), or `unknown: …` (xprop failed otherwise, e.g. no connection: proves nothing). */
const windowState = (id) => {
  try {
    execFileSync('xprop', ['-display', display, '-id', id, 'WM_CLASS'], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });
    return 'alive';
  } catch (e) {
    const said = String(e.stderr ?? e.message);
    return /BadWindow/.test(said) ? 'gone' : `unknown: ${said.trim().split('\n')[0]}`;
  }
};
/** The editor window on the private display, by the plugin's name: its WM_CLASS, or null. */
const editorWindow = (name) => {
  try {
    return execFileSync('xprop', ['-display', display, '-name', name, 'WM_CLASS'], { encoding: 'utf8' }).trim();
  } catch {
    return null;
  }
};
const app = start(resolve(binary), [], appEnv);
const outcome = await new Promise((done) => {
  const timer = setTimeout(() => done({ kind: 'failed', error: 'no verdict within 90 s' }), 90_000);
  for (const stream of [app.stdout, app.stderr]) {
    createInterface({ input: stream }).on('line', (line) => {
      if (/\[scan\]|plugin_host|error/i.test(line)) console.log(line);
      const opened = line.match(/editor window (0x[0-9a-f]+) open/);
      if (opened) lastWindow = opened[1];
      const at = line.indexOf('[linux-plugin-probe]');
      if (at === -1) return;
      const event = JSON.parse(line.slice(at + 20).trim().replace(/^"|"$/g, '').replaceAll('\\"', '"'));
      console.log('probe:', JSON.stringify(event));
      if (event.kind === 'editor') {
        // The page holds still for 5 s after an open: find the window, then shoot the whole display.
        sleep(2500).then(() => {
          const found = editorWindow(event.name);
          const file = shot.replace(/\.png$/, '') + `-editor-${shots.length + 1}-${event.format}.png`;
          execFileSync('import', ['-display', display, '-window', 'root', file]);
          const id = lastWindow;
          shots.push({ ...event, window: found, id, alive: id !== null && windowState(id) === 'alive', file });
          console.log('editor window:', found ?? 'NOT FOUND', id, '→', file);
        });
      }
      if (event.kind === 'editor-closed') {
        // The page holds still for 3 s after a close: the window the app made must be gone.
        const id = lastWindow;
        sleep(1500).then(() => {
          const state = id === null ? 'no window logged' : windowState(id);
          closes.push({ ...event, id, state, gone: state === 'gone' });
          console.log('editor closed by', event.by, id, state);
        });
      }
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
console.log('PASS: both test plugins picked, loaded and listing their Gain in the app; slot A unloaded by Off');
if (surge) {
  assert.equal(shots.length, 3, `three editor opens shot, got ${shots.length}`);
  for (const s of shots) {
    assert.match(s.window ?? '', /"bleeploop-plugin", "BleepLoop"/, `${s.format} editor window on the display`);
    assert(s.alive, `the ${s.format} editor's window ${s.id} is alive`);
  }
  assert.equal(closes.length, 2, `two editor closes checked, got ${closes.length}`);
  for (const c of closes) assert(c.gone, `the editor's window ${c.id} is gone after its close by ${c.by} (xprop: ${c.state})`);
  console.log('PASS: Surge XT\'s CLAP and VST3 editors opened from slot A (pick, then its editor control) on the private display');
}
process.exit(0);
