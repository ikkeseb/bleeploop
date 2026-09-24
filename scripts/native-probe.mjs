// scripts/native-probe.mjs — run one DEV native probe in a real `tauri dev` app and print its verdict.
//
//   pnpm native:smoke   editor smoke    (src/debug/editor-smoke.ts)
//   pnpm native:survey  restart survey  (src/debug/restart-survey.ts)
//   pnpm native:swap    swap stress     (src/debug/swap-stress.ts)
//   pnpm native:recall  recall restart  (src/debug/recall-restart.ts): one launch per phase
//   pnpm native:loopback  loopback sync (src/debug/loopback-sync.ts): needs an output cabled into input 1
//
// Options: `--asio` launches `pnpm dev:asio` instead of `pnpm dev:wasapi`; `--<knob>=<value>` becomes
// `VITE_LF_PROBE_<KNOB>` (`--filter=Pro-Q,Saturn`, `--hold=`, `--settle=`, `--params=`, `--plugins=`:
// each probe's header lists its knobs); `--stall-min=<n>` (default 3) fails the run when the probe
// prints nothing for that long. Refuses to start while an app or the port-1420 server runs
// (`pnpm native:kill`). Launches, waits for the probe's verdict line, stops the whole run and exits 0
// only on a clean verdict. A phased probe launches once per phase with `VITE_LF_PROBE_PHASE` set; a
// phase ends the way its probe says: the app closes itself (`close`), the runner sends its window the
// OS close on the verdict line, as the close button does (`os-close`: Rust hands it to the app's close
// guard), or the runner kills app.exe alone on that line, as a crashing plugin would (`crash`). The
// full log lands in logs/native-<probe>.log. Windows node only: the app windows open on the PC desktop.

import { execFileSync, spawn } from 'node:child_process';
import { createWriteStream, mkdirSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { appRunning, assertWindows, closeAppWindow, killNative, nativeRunning } from './native-kill.mjs';

const PROBES = {
  'editor-smoke': { tag: 'smoke', end: /^(complete: .*|no plugins.*)$/, pass: /^complete: \d+ opened, 0 failed/ },
  'restart-survey': { tag: 'survey', end: /^(complete|no plugins.*)$/, pass: /^complete$/ },
  'swap-stress': { tag: 'swap', end: /^(complete: .*|TIMEOUT .*|ABORTED.*|need at least .*)$/, pass: /^complete: \d+ swapped, 0 failed/ },
  // `config`: a Tauri config overlay; its own identifier gives the run its own WebView2 profile, so the
  // owner's jam, recovery and settings are never read or written.
  'loopback-sync': { tag: 'loopback', end: /^(result: .*|FAIL.*)$/, pass: /^result: /, config: 'scripts/loopback-probe.tauri.json' },
  // `recallLines`: how many `[rig-recall]` log lines the phase must print.
  'recall-restart': {
    tag: 'recall',
    phases: [
      { name: 'save', end: /^(saved: .*|FAIL.*)$/, pass: /^saved: /, exit: 'close', recallLines: 0 },
      { name: 'check', end: /^(restored: .*|FAIL.*)$/, pass: /^restored: /, exit: 'os-close', recallLines: 0 },
      { name: 'crash', end: /^(in flight: .*|FAIL.*)$/, pass: /^in flight: /, exit: 'crash' },
      { name: 'skip', end: /^(skipped: .*|FAIL.*)$/, pass: /^skipped: /, exit: 'close', recallLines: 1 },
      { name: 'after', end: /^(clean: .*|FAIL.*)$/, pass: /^clean: /, exit: 'close', recallLines: 0 },
    ],
  },
};
const BOOT_MIN = 20; // the first `tauri dev` after a Cargo change compiles for minutes
const EXIT_WAIT_MS = 60_000; // how long a `close` phase's app may take to quit

const [name, ...args] = process.argv.slice(2);
const spec = PROBES[name];
if (!spec) {
  console.error(`usage: node scripts/native-probe.mjs <${Object.keys(PROBES).join('|')}> [--asio] [--<knob>=<value>]`);
  process.exit(1);
}
assertWindows(`native probe ${name}`);

const env = { ...process.env, VITE_LF_PROBE: name };
let stallMin = 3;
for (const arg of args) {
  const m = arg.match(/^--([\w-]+)=(.*)$/);
  if (m?.[1] === 'stall-min') stallMin = Number(m[2]) || stallMin;
  else if (m) env[`VITE_LF_PROBE_${m[1].toUpperCase().replace(/-/g, '_')}`] = m[2];
  else if (arg !== '--asio') {
    console.error(`unknown argument ${arg}`);
    process.exit(1);
  }
}
const devScript = args.includes('--asio') ? 'dev:asio' : 'dev:wasapi';

const busy = nativeRunning();
if (busy.length) {
  console.error(`native probe: already running (${busy.join(', ')}). Close the app or run pnpm native:kill first.`);
  process.exit(1);
}

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const logPath = join(root, 'logs', `native-${name}.log`);
mkdirSync(dirname(logPath), { recursive: true });
const log = createWriteStream(logPath);
console.log(`native probe ${name}: pnpm ${devScript}, log ${join('logs', `native-${name}.log`)}`);
const tagged = new RegExp(`\\[${spec.tag}\\] (.*)$`);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Stop the dev run and whatever it left; returns the "name pid" lines `killNative` stopped. */
function stopRun(child) {
  try {
    execFileSync('taskkill', ['/T', '/F', '/PID', String(child.pid)], { stdio: 'ignore' });
  } catch {
    // already gone
  }
  return killNative();
}

/**
 * One launch: resolves `{ line, reason, recallLines }` — `line` is the verdict line (null when there
 * was none), `reason` says why not, `recallLines` counts the launch's `[rig-recall]` lines.
 */
function launch(phase, phaseEnv) {
  return new Promise((resolve) => {
    if (phase.name) log.write(`\n===== phase ${phase.name} =====\n`);
    const child = spawn(`pnpm ${devScript}${spec.config ? ` --config ${spec.config}` : ''}`, { cwd: root, env: phaseEnv, shell: true, stdio: ['ignore', 'pipe', 'pipe'] });
    let lastLine = Date.now();
    let seen = false;
    let partial = '';
    let recallLines = 0;
    let verdict;
    let settled = false;

    const done = (line, reason) => {
      if (settled) return;
      settled = true;
      clearInterval(watchdog);
      resolve({ line, reason, recallLines, stopped: stopRun(child) });
    };
    // After the verdict line: `close` waits for the app to quit by itself, `os-close` closes its window
    // first, `crash` kills app.exe alone at once (no tree kill: the WebView2 processes are left to
    // notice, as after a real crash).
    const afterVerdict = async (line) => {
      verdict = line;
      if (phase.exit === 'os-close') {
        const t0 = Date.now();
        if (!closeAppWindow()) return done(null, `no app window to close after: ${line}`);
        console.log(`  sent the app window its close ${Date.now() - t0} ms after the verdict line`);
      }
      if (phase.exit === 'crash') {
        const t0 = Date.now();
        try {
          execFileSync('taskkill', ['/F', '/IM', 'app.exe'], { stdio: 'ignore' });
        } catch {
          return done(null, 'app.exe was already gone at the crash line');
        }
        console.log(`  killed app.exe ${Date.now() - t0} ms after the in-flight line`);
      }
      if (phase.exit !== 'kill') {
        const deadline = Date.now() + EXIT_WAIT_MS;
        while (appRunning()) {
          if (Date.now() > deadline) return done(null, `app.exe did not quit within ${EXIT_WAIT_MS / 1000} s after: ${line}`);
          await sleep(500);
        }
        await sleep(1000); // let the dev run print its last lines
      }
      done(line);
    };

    function onChunk(chunk) {
      log.write(chunk);
      const lines = (partial + chunk).split(/\r?\n/);
      partial = lines.pop();
      for (const raw of lines) {
        // eslint-disable-next-line no-control-regex
        const clean = raw.replace(/\x1b\[[0-9;]*m/g, '');
        if (clean.includes('[rig-recall]')) recallLines++;
        const msg = clean.match(tagged)?.[1]?.trimEnd();
        if (msg === undefined) continue;
        seen = true;
        lastLine = Date.now();
        if (!/^\s/.test(msg)) console.log(`  [${spec.tag}]${phase.name ? ` ${phase.name}:` : ''} ${msg}`);
        if (verdict === undefined && phase.end.test(msg)) void afterVerdict(msg);
      }
    }
    child.stdout.on('data', onChunk);
    child.stderr.on('data', onChunk);
    child.on('close', () => {
      if (verdict === undefined) done(null, 'the dev run exited before a verdict line');
    });

    const watchdog = setInterval(() => {
      if (verdict !== undefined) return;
      const idle = (Date.now() - lastLine) / 60_000;
      if (!seen && idle > BOOT_MIN) done(null, `no [${spec.tag}] line within ${BOOT_MIN} min of launch`);
      else if (seen && idle > stallMin) done(null, `stalled: no [${spec.tag}] line for ${stallMin} min`);
    }, 5000);
    process.once('SIGINT', () => done(null, 'interrupted'));
  });
}

const phases = spec.phases ?? [{ ...spec, name: null, exit: 'kill' }];
let handover = null;
let result = { line: null, reason: 'no phase ran' };
let ok = true;
for (const phase of phases) {
  const phaseEnv = { ...env, ...(phase.name ? { VITE_LF_PROBE_PHASE: phase.name } : {}) };
  if (handover) phaseEnv.VITE_LF_PROBE_EXPECT = handover;
  result = await launch(phase, phaseEnv);
  if (result.stopped.length) console.log(`  stopped ${result.stopped.join(', ')}`);
  ok = result.line !== null && phase.pass.test(result.line);
  if (ok && phase.recallLines !== undefined && result.recallLines !== phase.recallLines) {
    ok = false;
    result.reason = `${phase.name}: ${result.recallLines} [rig-recall] line(s), expected ${phase.recallLines}`;
    result.line = null;
  }
  if (ok && result.line.startsWith('saved: ')) handover = result.line.slice('saved: '.length);
  if (!ok) {
    if (phase.name && result.line) result.line = `${phase.name}: ${result.line}`;
    break;
  }
}
await new Promise((r) => log.end(r));
const extra = name === 'restart-survey' && result.line ? `; ${surveySummary()}` : '';
const shown = ok && spec.phases ? `${spec.phases.length} phases, last: ${result.line}` : (result.line ?? result.reason);
console.log(`\n=== ${name}: ${ok ? 'PASS' : 'FAIL'}: ${shown}${extra} ===`);
process.exit(ok ? 0 : 1);

// What the survey learned, as the counts its baseline in docs/VERIFY.md uses.
function surveySummary() {
  const text = readFileSync(logPath, 'utf8');
  const flags = new Map();
  for (const m of text.matchAll(/VST3 restartComponent\(([^)]*)\)/g)) flags.set(m[1], (flags.get(m[1]) ?? 0) + 1);
  const vst3 = [...flags.values()].reduce((a, b) => a + b, 0);
  const clap = (text.match(/request_restart/g) ?? []).length;
  const failed = (text.match(/\[survey\] LOAD FAILED/g) ?? []).length;
  const byFlag = [...flags].map(([f, n]) => `${f} ${n}`).join(', ');
  return `${vst3} VST3 restartComponent${byFlag ? ` (${byFlag})` : ''}, ${clap} CLAP request_restart line(s), ${failed} load failure(s)`;
}
