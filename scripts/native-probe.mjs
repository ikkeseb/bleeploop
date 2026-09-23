// scripts/native-probe.mjs — run one DEV native probe in a real `tauri dev` app and print its verdict.
//
//   pnpm native:smoke   editor smoke    (src/debug/editor-smoke.ts)
//   pnpm native:survey  restart survey  (src/debug/restart-survey.ts)
//   pnpm native:swap    swap stress     (src/debug/swap-stress.ts)
//
// Options: `--asio` launches `pnpm dev:asio` instead of `pnpm dev:wasapi`; `--<knob>=<value>` becomes
// `VITE_LF_PROBE_<KNOB>` (`--filter=Pro-Q,Saturn`, `--hold=`, `--settle=`, `--params=`: each probe's
// header lists its knobs); `--stall-min=<n>` (default 3) fails the run when the probe prints nothing
// for that long. Refuses to start while an app or the port-1420 server runs (`pnpm native:kill`).
// Launches, waits for the probe's verdict line, stops the whole run and exits 0 only on a clean
// verdict. The full log lands in logs/native-<probe>.log. Windows node only: the app windows open on
// the PC desktop.

import { execFileSync, spawn } from 'node:child_process';
import { createWriteStream, mkdirSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { assertWindows, killNative, nativeRunning } from './native-kill.mjs';

const PROBES = {
  'editor-smoke': { tag: 'smoke', end: /^(complete: .*|no plugins.*)$/, pass: /^complete: \d+ opened, 0 failed/ },
  'restart-survey': { tag: 'survey', end: /^(complete|no plugins.*)$/, pass: /^complete$/ },
  'swap-stress': { tag: 'swap', end: /^(complete: .*|TIMEOUT .*|ABORTED.*|need at least .*)$/, pass: /^complete: \d+ swapped, 0 failed/ },
};
const BOOT_MIN = 20; // the first `tauri dev` after a Cargo change compiles for minutes

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

const child = spawn(`pnpm ${devScript}`, { cwd: root, env, shell: true, stdio: ['ignore', 'pipe', 'pipe'] });
const tagged = new RegExp(`\\[${spec.tag}\\] (.*)$`);
let lastLine = Date.now();
let seen = false;
let partial = '';

function onChunk(chunk) {
  log.write(chunk);
  const lines = (partial + chunk).split(/\r?\n/);
  partial = lines.pop();
  for (const raw of lines) {
    // eslint-disable-next-line no-control-regex
    const msg = raw.replace(/\x1b\[[0-9;]*m/g, '').match(tagged)?.[1]?.trimEnd();
    if (msg === undefined) continue;
    seen = true;
    lastLine = Date.now();
    if (!/^\s/.test(msg)) console.log(`  [${spec.tag}] ${msg}`);
    if (spec.end.test(msg)) finish(msg);
  }
}
child.stdout.on('data', onChunk);
child.stderr.on('data', onChunk);
child.on('close', () => finish(null, 'the dev run exited before a verdict line'));

const watchdog = setInterval(() => {
  const idle = (Date.now() - lastLine) / 60_000;
  if (!seen && idle > BOOT_MIN) finish(null, `no [${spec.tag}] line within ${BOOT_MIN} min of launch`);
  else if (seen && idle > stallMin) finish(null, `stalled: no [${spec.tag}] line for ${stallMin} min`);
}, 5000);
process.on('SIGINT', () => finish(null, 'interrupted'));

let finished = false;
function finish(line, reason) {
  if (finished) return;
  finished = true;
  clearInterval(watchdog);
  try {
    execFileSync('taskkill', ['/T', '/F', '/PID', String(child.pid)], { stdio: 'ignore' });
  } catch {
    // already gone
  }
  const stopped = killNative();
  log.end(() => {
    if (stopped.length) console.log(`  stopped ${stopped.join(', ')}`);
    const ok = line !== null && spec.pass.test(line);
    const extra = name === 'restart-survey' && line ? `; ${surveySummary()}` : '';
    console.log(`\n=== ${name}: ${ok ? 'PASS' : 'FAIL'}: ${line ?? reason}${extra} ===`);
    process.exit(ok ? 0 : 1);
  });
}

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
