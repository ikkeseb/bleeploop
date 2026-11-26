// Run from the repo root. Build a Tauri debug app with build.devUrl set to
// http://127.0.0.1:14937. Needs Xvfb + xvfb-run, JACK tools, parec and pactl.
// pw-jack node verify/linux-app/run.mjs APP JACK_INPUT_GROUP JACK_OUTPUT_GROUP PULSE_SINK.monitor
// Only the two process-owned windows and links are driven. No desktop clicks or server reconfiguration.
import assert from 'node:assert/strict';
import { spawn, execFileSync } from 'node:child_process';
import { once } from 'node:events';
import { createWriteStream } from 'node:fs';
import { mkdir, mkdtemp, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { resolve, join } from 'node:path';
import { createInterface } from 'node:readline';
import { createServer } from 'vite';
import { measure } from './waveform.mjs';

const [binary, input, output, monitor] = process.argv.slice(2);
assert(binary && input && output && monitor && process.argv.length === 6, 'Arguments: APP JACK_INPUT_GROUP JACK_OUTPUT_GROUP PULSE_SINK.monitor');
assert(monitor.endsWith('.monitor') && !monitor.startsWith('@'), 'Specify the exact sink monitor name');
const sinks = JSON.parse(execFileSync('pactl', ['-f', 'json', 'list', 'sinks'], { encoding: 'utf8' }));
assert(sinks.some((sink) => sink.monitor_source === monitor), `Unknown sink monitor ${monitor}`);
const dir = await mkdtemp(join(tmpdir(), 'bleeploop-app-probe-'));
console.log('Private profile and measurements:', dir);
let phase = 'choose';
let picks;
let expectedBlock;
const result = { monitor };
const children = new Set();
function terminate(child) {
  if (child.pid) {
    try { process.kill(-child.pid, 'SIGTERM'); } catch (err) { if (err.code !== 'ESRCH') throw err; }
  }
}
function stopAll() { for (const child of children) terminate(child); }
process.on('exit', stopAll);
for (const signal of ['SIGINT', 'SIGTERM']) process.once(signal, () => { stopAll(); process.exit(1); });

const server = await createServer({
  cacheDir: join(dir, 'vite'),
  server: { host: '127.0.0.1', port: 14937, strictPort: true, hmr: false, watch: null },
  plugins: [{
    name: 'private-linux-app-probe',
    transformIndexHtml() {
      // Synchronous head script, before app modules read the profile or boot native audio.
      return [
        { tag: 'script', injectTo: 'head-prepend', children: `
          window.__linuxAppProbe = ${JSON.stringify({ phase, input, output, picks, expectedBlock }).replaceAll('<', '\\u003c')};
          localStorage.setItem('lf.masterVolume', '0.02');
          localStorage.setItem('lf.clickVolume', '0');
        ` },
        { tag: 'script', injectTo: 'body', attrs: { type: 'module', src: '/verify/linux-app/controls.mjs' } },
      ];
    },
  }],
});
const childEnv = { ...process.env, GDK_BACKEND: 'x11', LIBGL_ALWAYS_SOFTWARE: '1', LP_NUM_THREADS: '1', WEBKIT_DISABLE_DMABUF_RENDERER: '1',
  XDG_DATA_HOME: join(dir, 'data'), XDG_CONFIG_HOME: join(dir, 'config'), XDG_CACHE_HOME: join(dir, 'cache'), XDG_STATE_HOME: join(dir, 'state') };
for (const key of ['XDG_DATA_HOME', 'XDG_CONFIG_HOME', 'XDG_CACHE_HOME', 'XDG_STATE_HOME']) await mkdir(childEnv[key]);

async function launch() {
  const child = spawn('xvfb-run', ['-a', '-s', '-screen 0 1440x1000x24 -nolisten tcp', resolve(binary)],
    { detached: true, env: childEnv, stdio: ['ignore', 'pipe', 'pipe'] });
  children.add(child);
  const rows = {};
  let failure;
  const log = createWriteStream(join(dir, phase + '.log'));
  const record = (line) => {
    log.write(line + '\n');
    const found = line.match(/\[linux-app-probe\]\s+(\{.*\})/);
    if (!found) return;
    try {
      const message = JSON.parse(found[1]);
      console.log(message.kind, JSON.stringify(message));
      rows[message.kind] = message;
      result[message.kind] = message;
      if (message.kind === 'fail') throw new Error(message.error);
    } catch (err) {
      failure = err;
      terminate(child);
    }
  };
  for (const stream of [child.stdout, child.stderr]) createInterface({ input: stream }).on('line', record);
  const timer = setTimeout(() => { failure = new Error(`${phase} timed out. See ${dir}/${phase}.log`); terminate(child); }, 45000);
  try {
    const [code] = await once(child, 'close');
    if (failure) throw failure;
    assert.equal(code, 0, `App exited ${code}. See ${dir}/${phase}.log`);
    assert(rows[phase === 'choose' ? 'chosen' : 'take'], `Missing ${phase} result. See ${dir}/${phase}.log`);
    return rows;
  } finally {
    clearTimeout(timer);
    terminate(child);
    children.delete(child);
    log.end();
  }
}

function startMonitor(rate) {
  const chunks = [];
  const child = spawn('parec', ['--raw', '--format=float32le', '--channels=2', '--no-remix', '--latency-msec=60',
    '--client-name=BleepLoop monitor probe', '--device=' + monitor, '--rate=' + rate],
    { detached: true, stdio: ['ignore', 'pipe', 'inherit'] });
  children.add(child);
  const closed = once(child, 'close');
  const ready = once(child.stdout, 'data');
  child.stdout.on('data', (part) => chunks.push(part));
  return {
    ready: Promise.race([ready, closed.then(() => { throw new Error('Monitor recorder exited before capture'); })]),
    async finish() {
      assert(child.exitCode === null && !child.signalCode, 'Monitor recorder exited early');
      terminate(child);
      await closed;
      children.delete(child);
      const raw = Buffer.concat(chunks);
      assert.equal(raw.length % 8, 0, 'Partial stereo float frame');
      return new Float32Array(raw.buffer, raw.byteOffset, raw.length / 4);
    },
  };
}

function wav(samples, rate) {
  const bytes = Buffer.alloc(44 + 2 * samples.length);
  bytes.write('RIFF', 0); bytes.writeUInt32LE(bytes.length - 8, 4); bytes.write('WAVEfmt ', 8);
  bytes.writeUInt32LE(16, 16); bytes.writeUInt16LE(1, 20); bytes.writeUInt16LE(2, 22);
  bytes.writeUInt32LE(rate, 24); bytes.writeUInt32LE(4 * rate, 28);
  bytes.writeUInt16LE(4, 32); bytes.writeUInt16LE(16, 34); bytes.write('data', 36);
  bytes.writeUInt32LE(bytes.length - 44, 40);
  for (let i = 0; i < samples.length; i++) bytes.writeInt16LE(Math.round(Math.max(-1, Math.min(1, samples[i])) * 32767), 44 + i * 2);
  return bytes;
}

try {
  await server.listen();
  const { chosen } = await launch();
  picks = chosen.picks;
  expectedBlock = chosen.device.block;
  phase = 'take';
  const recorder = startMonitor(chosen.device.sampleRate);
  let timeout;
  try {
    await Promise.race([recorder.ready, new Promise((_, reject) => { timeout = setTimeout(() => reject(new Error('Monitor capture timed out')), 6000); })]);
  } finally { clearTimeout(timeout); }
  let samples;
  try {
    await launch();
  } finally {
    samples = await recorder.finish();
    await writeFile(join(dir, 'monitor.wav'), wav(samples, chosen.device.sampleRate));
  }
  const { reopened, take, timing } = result;
  result.waveform = measure(samples, reopened.device, take);
  result.audio = 'pass';
  assert(timing, 'No timing report');
  for (const key of ['xruns', 'missingFrames']) {
    assert(Number.isSafeInteger(timing.before[key]) && Number.isSafeInteger(timing.after[key]), `No native ${key} counter`);
    assert.equal(timing.after[key], timing.before[key], `Audio passed; timing failed: ${key} ${timing.before[key]} to ${timing.after[key]}`);
  }
  assert.equal(timing.feed.resetsAfter, timing.feed.resetsBefore, 'Feed reset during measurement');
  assert.equal(timing.feed.captureGapEvents.length, 0, 'Capture rejected for a gap');
  result.verdict = 'pass';
  console.log('PASS', JSON.stringify(result.waveform));
} catch (err) {
  result.verdict = 'fail';
  result.error = String(err);
  throw err;
} finally {
  await writeFile(join(dir, 'result.json'), JSON.stringify(result, null, 2));
  stopAll();
  await server.close();
}
