// scripts/native-engine.mjs — the native-engine Stage 4 rig run (docs/plans/native-engine.md § Stage 4):
// builds the debug app with ASIO and runs one `--probe-engine` process: open the device, load the amp-sim
// and Pro-Q into the engine's two slots, record a loop, soak, then backend/buffer switches and plugin
// swaps while the loop plays. Relays the probe's lines and exits with its code (0 = every check passed).
//
//   pnpm native:engine [--backend=asio] [--buffer=128] [--seconds=600] [--switches=20] [--swaps=4]
//                      [--in=0] [--device=Focusrite] [--amp=<vst3>] [--proq=<vst3>]
//
// Needs no running `app` and no cable; `--amp=` or `--proq=` (empty) leaves that slot empty. The take
// records input `--in` (0-based) through the amp while it records: keep that input off a loopback cable.
// The full log lands in logs/native-engine.log. Windows node only.

import { execFileSync, spawn } from 'node:child_process';
import { createWriteStream, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { appRunning, assertWindows } from './native-kill.mjs';

assertWindows('native:engine');
const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const vst3 = join(process.env.COMMONPROGRAMFILES ?? 'C:\\Program Files\\Common Files', 'VST3');
const opt = {
  backend: 'asio',
  buffer: '128',
  seconds: '600',
  switches: '20',
  swaps: '4',
  in: '0',
  device: 'Focusrite',
  amp: join(vst3, 'Neural DSP', 'Archetype Petrucci X.vst3'),
  proq: join(vst3, 'FabFilter', 'FabFilter Pro-Q 3.vst3'),
};
for (const arg of process.argv.slice(2)) {
  const m = arg.match(/^--([\w-]+)=(.*)$/);
  if (!m || !(m[1] in opt)) {
    console.error(`unknown argument ${arg}`);
    process.exit(1);
  }
  opt[m[1]] = m[2];
}

if (appRunning()) {
  console.error('native:engine: an app is running. Close it or run pnpm native:kill first.');
  process.exit(1);
}

const logPath = join(root, 'logs', 'native-engine.log');
mkdirSync(dirname(logPath), { recursive: true });
const log = createWriteStream(logPath);

console.log('native:engine: cargo build --features asio (debug)');
execFileSync('cargo', ['build', '--features', 'asio'], { cwd: join(root, 'src-tauri'), stdio: 'inherit' });
const exe = join(root, 'src-tauri', 'target', 'debug', 'app.exe');

const plugins = [opt.amp, opt.proq].flatMap((path, slot) => (path ? ['--plugin', `${slot}=${path}`] : []));
const args = [
  '--probe-engine', opt.backend, opt.buffer, ...plugins,
  '--seconds', opt.seconds, '--switches', opt.switches, '--swaps', opt.swaps, '--in', opt.in,
  ...(opt.device ? ['--device', opt.device] : []),
];
// The soak, ~15 s a switch, ~30 s a swap, and room for loads and the teardown.
const timeoutS = Number(opt.seconds) + 15 * Number(opt.switches) + 30 * Number(opt.swaps) + 300;
log.write(`app.exe ${args.join(' ')}\n`);
const child = spawn(exe, args, { stdio: ['ignore', 'pipe', 'pipe'] });
const timer = setTimeout(() => {
  console.log(`native:engine: TIMEOUT after ${timeoutS} s`);
  child.kill();
}, timeoutS * 1000);
for (const stream of [child.stdout, child.stderr]) {
  stream.on('data', (chunk) => {
    process.stdout.write(chunk);
    log.write(chunk);
  });
}
child.on('exit', (code) => {
  clearTimeout(timer);
  console.log(`=== native:engine: ${code === 0 ? 'all checks pass' : `exit ${code}`} — log ${join('logs', 'native-engine.log')} ===`);
  log.end();
  process.exitCode = code ?? 1;
});
