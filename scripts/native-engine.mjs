// scripts/native-engine.mjs — the native engine's rig run (the bar: src-tauri/src/engine_io/probe.rs):
// builds the app with ASIO (debug, or `--profile=rig`) and runs one `--probe-engine` process: open the device, load the amp-sim
// and Pro-Q into the engine's two slots, record a loop, soak, then backend/buffer switches and plugin
// swaps while the loop plays. Relays the probe's lines and exits with its code (0 = every check passed).
//
//   pnpm native:engine [--backend=asio] [--buffer=128] [--seconds=600] [--switches=20] [--swaps=4]
//                      [--in=0] [--device=Focusrite] [--amp=<vst3>] [--proq=<vst3>] [--mute=1]
//                      [--cycle=wasapi] [--hold=2] [--pause=<ms>] [--share=<endpoint id>] [--log=native-engine]
//                      [--scene=heavy] [--profile=rig] [--tone=1 --mute=1 [--out=1] [--tone-from=S] [--tone-level=X]] [--load=test [--load-affinity=<hex>]]
//   pnpm native:engine --lag=1 --in=1 [--out=1] [--seconds=10] [--preopen=0]
//
// Needs no running `app`, and no cable but for `--tone=` and `--lag=1`; `--amp=` or `--proq=` (empty)
// leaves that slot empty. The take
// records input `--in` (0-based) through the amp while it records: keep that input off a loopback cable.
// `--mute=1` plays silence. `--tone=N` (ASIO, with `--mute=1`, not with `--lag=1`) needs the loopback cable
// from output `--out` (0 or 1) into input N (0-based; not `--in`), which slot 1 reads for the run: a steady sine plays through the soak
// and its return is watched for loopback discontinuities, each tied to the nearest callback that ran long
// or entered late (the probe's header says how the `tone` check ends). `--lag=1` runs only the lag phase instead: a chirp out `--out` through the
// loopback cable into `--in`, judged against the driver's reported latency (the Stage 1 A2 bar on the
// engine's open path; audible); `--preopen=0` opens ASIO without its preopen, for a before-and-after.
// `--cycle=` names the switches' round (`asio64`, `asio128`, `asio256`, `wasapi`, comma-separated;
// `--cycle=wasapi` with `--backend=wasapi --buffer=default` closes and reopens WASAPI at every switch, no ASIO).
// `--hold=` is the seconds each switch plays; `--pause=` closes the device and waits that many ms before
// every switch to WASAPI. `--share=` turns Share output on, mirroring the master to that WASAPI render endpoint
// (the id the release log names) while ASIO plays, so the soak counts the mirror too. `--scene=heavy` soaks
// the probe's heavy scene (its header says what plays). `--profile=rig` builds and runs the optimized
// probe build (`[profile.rig]` in src-tauri/Cargo.toml, target/rig/app.exe) instead of the debug one.
// `--tone-from=` starts the tone that many seconds into the stream (default 10) and `--tone-level=` plays it at
// that amplitude (default 0.25): the probe's header.
// `--load=test` runs a build's load beside the soak: the workspace's `cargo test` in cargo's own order
// (`scripts/cargo-test.mjs --jobs 1`, about 7 minutes a pass; its test binaries, and rustc only when the
// tests are stale) at normal priority, pass after pass from the tone's calibration (at once without
// `--tone=`) until the probe exits; `--load-affinity=` (hex, `start /affinity`'s) keeps it to those
// processors. Each pass's exit, length and what it ran, the load's priority a minute in, and the seconds
// loaded are printed beside the result; a load that never started, failed or ran no test exits 3 when the
// probe's checks passed, so a run without its load is not read as one beside a build.
// The full log lands in logs/native-engine.log (`--log=<name>`: logs/<name>.log). Windows node only.

import { execFileSync, spawn } from 'node:child_process';
import { createWriteStream, mkdirSync } from 'node:fs';
import { setPriority, constants } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
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
  tone: '',
  lag: '',
  out: '1',
  preopen: '',
  cycle: '',
  hold: '',
  pause: '',
  share: '',
  scene: '',
  profile: '',
  'tone-from': '',
  'tone-level': '',
  load: '',
  'load-affinity': '',
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

if (opt.load && opt.load !== 'test') {
  console.error(`--load=${opt.load}: expected test`);
  process.exit(1);
}

if (opt.lag && opt.tone) {
  console.error('--tone runs in the soak: not with --lag=1 (the lag phase runs alone)');
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
      ...(opt.tone ? ['--tone', opt.tone, '--out', opt.out] : []),
      ...(opt.tone && opt['tone-from'] ? ['--tone-from', opt['tone-from']] : []),
      ...(opt.tone && opt['tone-level'] ? ['--tone-level', opt['tone-level']] : []),
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
const say = (line) => {
  console.log(line);
  if (!log.writableEnded) log.write(`${line}\n`);
};

// The load (`--load=test`): passes until the probe exits, then the running one is killed. A pass is a
// node of its own that sets its class to normal (and its affinity) before it starts any child, so cargo
// and the tests are born at that class even from a BelowNormal shell; its output lands in
// logs/<log>-load.log.
const LOAD_PASS = `
import { execFileSync } from 'node:child_process';
import { constants, setPriority } from 'node:os';
setPriority(constants.priority.PRIORITY_NORMAL);
const mask = process.env.LF_LOAD_AFFINITY;
if (mask) execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', \`(Get-Process -Id \${process.pid}).ProcessorAffinity = 0x\${mask}\`]);
const { cargoTest } = await import(process.env.LF_LOAD_SCRIPT);
const r = await cargoTest(['--workspace', '--no-default-features'], { jobs: 1, log: (t) => process.stdout.write(t) });
console.log(\`load-pass: \${r.binaries} binaries, \${r.passed} tests passed\`);
process.exit(r.code === 0 ? 0 : 1);
`;
const load = { started: 0, passes: [], child: null, tally: null, stopping: false, failed: false, killed: null, out: null };
function loadPass() {
  const at = Date.now();
  const env = { ...process.env, LF_LOAD_SCRIPT: pathToFileURL(join(root, 'scripts', 'cargo-test.mjs')).href, LF_LOAD_AFFINITY: opt['load-affinity'] };
  const pass = spawn(process.execPath, ['--input-type=module', '-e', LOAD_PASS], { cwd: root, env, stdio: ['ignore', 'pipe', 'pipe'] });
  // What the pass ran so far: crates compiled, test binaries finished (cargo-test.mjs's "Running" line
  // after each), whether it waited on cargo's build lock.
  const tally = { compiled: 0, ran: 0, blocked: false };
  load.tally = tally;
  for (const stream of [pass.stdout, pass.stderr]) {
    stream.on('data', (chunk) => {
      if (!load.out.writableEnded) load.out.write(chunk);
      const text = String(chunk);
      tally.compiled += (text.match(/^\s*Compiling /gm) ?? []).length;
      tally.ran += (text.match(/^\s*Running /gm) ?? []).length;
      tally.blocked ||= text.includes('Blocking waiting for file lock');
    });
  }
  load.child = pass;
  pass.on('close', (code) => {
    const seconds = (Date.now() - at) / 1000;
    load.child = null;
    if (load.stopping) {
      load.killed = { seconds, tally };
      load.onStopped?.();
      return;
    }
    load.passes.push({ code, seconds, tally });
    say(`native:engine: load pass ${load.passes.length} exit ${code} after ${seconds.toFixed(0)} s (${loadWhat(tally)})`);
    if (code !== 0 || tally.ran === 0) {
      load.failed = true;
      say('native:engine: LOAD FAILED: the rest of the soak ran without it');
      return;
    }
    loadPass();
  });
}
function loadWhat(tally) {
  return `${tally.ran} test binaries ran, ${tally.compiled} crates compiled${tally.blocked ? ', waited on the build lock' : ''}`;
}
function startLoad() {
  if (!opt.load || load.started) return;
  load.started = Date.now();
  load.out = createWriteStream(join(root, 'logs', `${opt.log}-load.log`));
  say(`native:engine: load starts (cargo test, cargo order, normal priority${opt['load-affinity'] ? `, affinity ${opt['load-affinity']}` : ''})`);
  loadPass();
  setTimeout(() => {
    if (load.stopping) return;
    try {
      const classes = execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command',
        "Get-Process | Where-Object { $_.Name -in 'app','cargo','rustc' -or $_.Path -like '*src-tauri\\target\\debug\\deps\\*' } | ForEach-Object { \"$($_.Name)=$($_.PriorityClass)/$($_.ProcessorAffinity)\" }"], { encoding: 'utf8' });
      say(`native:engine: load priority/affinity a minute in: ${classes.trim().split(/\r?\n/).join(', ') || 'no process found'}`);
    } catch (e) {
      say(`native:engine: load priority unread: ${e instanceof Error ? e.message : String(e)}`);
    }
  }, 60_000).unref();
}
if (!opt.tone) startLoad();

let pending = '';
for (const stream of [child.stdout, child.stderr]) {
  stream.on('data', (chunk) => {
    process.stdout.write(chunk);
    log.write(chunk);
    pending = (pending + chunk).slice(-4096);
    if (opt.tone && pending.includes('tone: calibrated')) startLoad();
  });
}

// Stops the load and waits for its pass to close: the verdict on whether the soak ran beside a load.
async function stopLoad() {
  load.stopping = true;
  let confirmed = true;
  if (load.child) {
    const closed = new Promise((resolve) => {
      load.onStopped = () => resolve(true);
      setTimeout(() => resolve(false), 15_000).unref();
    });
    try {
      execFileSync('taskkill', ['/F', '/T', '/PID', String(load.child.pid)], { stdio: 'pipe' });
    } catch (e) {
      say(`native:engine: load kill failed: ${e instanceof Error ? e.message.split('\n')[0] : String(e)}`);
    }
    confirmed = await closed;
    if (!confirmed) say('native:engine: the load did not close within 15 s: its processes may outlive the run');
  }
  load.out?.end();
  if (!load.started) {
    say('native:engine: LOAD INVALID: it never started');
    return false;
  }
  const whole = load.passes.filter((p) => p.code === 0);
  const killed = load.killed ?? (load.child ? { seconds: 0, tally: load.tally } : null);
  const ran = load.passes.reduce((sum, p) => sum + p.tally.ran, 0) + (killed?.tally.ran ?? 0);
  const loaded = load.passes.reduce((sum, p) => sum + p.seconds, 0) + (killed?.seconds ?? 0);
  say(`native:engine: load: ${whole.length} whole pass(es)${killed ? `, the last stopped after ${killed.seconds.toFixed(0)} s (${loadWhat(killed.tally)})` : ''}: ${loaded.toFixed(0)} s, ${ran} test binaries ran${load.failed ? '; a pass FAILED' : ''}`);
  const invalid = load.failed ? 'a pass failed or ran no test' : ran === 0 ? 'no test binary ran' : !confirmed ? 'its stop was not confirmed' : '';
  if (invalid) say(`native:engine: LOAD INVALID: ${invalid}`);
  return !invalid;
}

child.on('close', async (code) => {
  clearTimeout(timer);
  const loadOk = opt.load ? await stopLoad() : true;
  // 3: the probe's checks passed, but its soak did not run beside the load it was asked for.
  const exit = code !== 0 ? (code ?? 1) : loadOk ? 0 : 3;
  console.log(`=== native:engine: ${exit === 0 ? 'all checks pass' : `exit ${exit}`} — log ${join('logs', `${opt.log}.log`)} ===`);
  log.end();
  process.exitCode = exit;
});
