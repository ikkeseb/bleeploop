// scripts/native-engine.mjs — the native engine's rig run (the bar: src-tauri/src/engine_io/probe.rs):
// builds the app with ASIO (debug, or `--profile=rig`) and runs one `--probe-engine` process: open the device, load the amp-sim
// and Pro-Q into the engine's two slots, record a loop, soak, then backend/buffer switches and plugin
// swaps while the loop plays. Relays the probe's lines and exits with its code (0 = every check passed).
//
//   pnpm native:engine [--backend=asio] [--buffer=128] [--seconds=600] [--switches=20] [--swaps=4]
//                      [--in=0] [--device=Focusrite] [--amp=<vst3>] [--proq=<vst3>] [--mute=1]
//                      [--cycle=wasapi] [--hold=2] [--pause=<ms>] [--share=<endpoint id>] [--log=native-engine]
//                      [--scene=heavy] [--profile=rig]
//   pnpm native:engine --lag=1 --in=1 [--out=1] [--seconds=10] [--preopen=0]
//
// Needs no running `app` and no cable; `--amp=` or `--proq=` (empty) leaves that slot empty. The take
// records input `--in` (0-based) through the amp while it records: keep that input off a loopback cable.
// `--mute=1` plays silence. `--lag=1` runs only the lag phase instead: a chirp out `--out` through the
// loopback cable into `--in`, judged against the driver's reported latency (the Stage 1 A2 bar on the
// engine's open path; audible); `--preopen=0` opens ASIO without its preopen, for a before-and-after.
// `--cycle=` names the switches' round (`asio64`, `asio128`, `asio256`, `wasapi`, comma-separated;
// `--cycle=wasapi` with `--backend=wasapi --buffer=default` closes and reopens WASAPI at every switch, no ASIO).
// `--hold=` is the seconds each switch plays; `--pause=` closes the device and waits that many ms before
// every switch to WASAPI. `--share=` turns Share output on, mirroring the master to that WASAPI render endpoint
// (the id the release log names) while ASIO plays, so the soak counts the mirror too. `--scene=heavy` soaks
// the probe's heavy scene (its header says what plays). `--profile=rig` builds and runs the optimized
// probe build (`[profile.rig]` in src-tauri/Cargo.toml, target/rig/app.exe) instead of the debug one.
// The full log lands in logs/native-engine.log (`--log=<name>`: logs/<name>.log). Windows node only.

import { execFileSync, spawn } from 'node:child_process';
import { createWriteStream, mkdirSync } from 'node:fs';
import { setPriority, constants } from 'node:os';
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
  mute: '',
  lag: '',
  out: '1',
  preopen: '',
  cycle: '',
  hold: '',
  pause: '',
  share: '',
  scene: '',
  profile: '',
  log: 'native-engine',
};
for (const arg of process.argv.slice(2)) {
  const m = arg.match(/^--([\w-]+)=(.*)$/);
  if (!m || !(m[1] in opt)) {
    console.error(`unknown argument ${arg}`);
    process.exit(1);
  }
  opt[m[1]] = m[2];
}

if (opt.profile && opt.profile !== 'rig') {
  console.error(`--profile=${opt.profile}: expected rig (or none: the debug build)`);
  process.exit(1);
}

if (appRunning()) {
  console.error('native:engine: an app is running. Close it or run pnpm native:kill first.');
  process.exit(1);
}

const logPath = join(root, 'logs', `${opt.log}.log`);
mkdirSync(dirname(logPath), { recursive: true });
const log = createWriteStream(logPath);

const profile = opt.profile ? ['--profile', opt.profile] : [];
console.log(`native:engine: cargo build ${[...profile, '--features', 'asio'].join(' ')} (${opt.profile || 'debug'})`);
execFileSync('cargo', ['build', ...profile, '--features', 'asio'], { cwd: join(root, 'src-tauri'), stdio: 'inherit' });
const exe = join(root, 'src-tauri', 'target', opt.profile || 'debug', 'app.exe');

const plugins = [opt.amp, opt.proq].flatMap((path, slot) => (path ? ['--plugin', `${slot}=${path}`] : []));
const args = opt.lag
  ? ['--probe-engine', opt.backend, opt.buffer, '--lag', '--seconds', opt.seconds, '--in', opt.in, '--out', opt.out,
      ...(opt.preopen === '0' ? ['--no-preopen'] : [])]
  : [
      '--probe-engine', opt.backend, opt.buffer, ...plugins,
      '--seconds', opt.seconds, '--switches', opt.switches, '--swaps', opt.swaps, '--in', opt.in,
      ...(opt.cycle ? ['--cycle', opt.cycle] : []),
      ...(opt.hold ? ['--hold', opt.hold] : []),
      ...(opt.pause ? ['--pause', opt.pause] : []),
      ...(opt.device ? ['--device', opt.device] : []),
      ...(opt.mute ? ['--mute'] : []),
      ...(opt.share ? ['--share', opt.share] : []),
      ...(opt.scene ? ['--scene', opt.scene] : []),
    ];
// The soak, ~15 s a switch, ~30 s a swap, and room for loads and the teardown.
const timeoutS = Number(opt.seconds) + (15 + Number(opt.hold || 0) + Number(opt.pause || 0) / 1000) * Number(opt.switches) + 30 * Number(opt.swaps) + 300;
log.write(`${opt.profile || 'debug'} app.exe ${args.join(' ')}\n`);
const child = spawn(exe, args, { stdio: ['ignore', 'pipe', 'pipe'] });
// Launch at Normal like the owner's app; the rig's BelowNormal tmux shell passes its class on.
try {
  setPriority(child.pid, constants.priority.PRIORITY_NORMAL);
} catch (e) {
  const line = `native:engine: launch kept the shell's priority: ${e instanceof Error ? e.message : String(e)}`;
  console.log(line);
  log.write(`${line}\n`);
}
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
  console.log(`=== native:engine: ${code === 0 ? 'all checks pass' : `exit ${code}`} — log ${join('logs', `${opt.log}.log`)} ===`);
  log.end();
  process.exitCode = code ?? 1;
});
