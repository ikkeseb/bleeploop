// scripts/native-probe.mjs — run one DEV native probe in a real `tauri dev` app and print its verdict.
//
//   pnpm native:smoke   editor smoke    (src/debug/editor-smoke.ts)
//   pnpm native:survey  restart survey  (src/debug/restart-survey.ts)
//   pnpm native:swap    swap stress     (src/debug/swap-stress.ts)
//   pnpm native:recall  recall restart  (src/debug/recall-restart.ts): one launch per phase
//   pnpm native:engine-smoke  the UI on the native engine (src/debug/engine-smoke.ts)
//   pnpm native:tuner  the tuner on the native engine (src/debug/tuner-smoke.ts): one launch, on ASIO
//   pnpm native:engine-recovery  export, import and recovery on the engine (src/debug/engine-recovery.ts)
//   pnpm native:engine-loopback  where takes land against the click on the engine
//     (src/debug/engine-loopback.ts): needs an output cabled into input 2 (`--channel=` is 0-based)
//   pnpm native:tone-recall  a plugin's tone across a restart, a session export and import, and a kill
//     (src/debug/tone-recall.ts): one launch per phase, on WASAPI
//   pnpm native:export-master  the export's wet master rendered by the engine, beside its stem
//     (src/debug/export-master.ts): one launch, on WASAPI
//   pnpm native:midi  the UI's note sources through native MIDI's router, a WebView reload included
//     (src/debug/midi-switch.ts): one launch, on WASAPI, the master muted
//   pnpm native:storage-kill  whether a localStorage write survives app.exe dying 0-2000 ms after it
//     (src/debug/storage-kill.ts): ten launches, on WASAPI; measures, passes on any outcome
//   node scripts/native-probe.mjs bench-load --asio  the MIDI benchmark's jam load, held while
//     `LF_MIDI_BENCH` sends (src/debug/bench-load.ts): one launch, on ASIO 128, the master at 0
//
// Options: `--asio` launches `pnpm dev:asio` instead of `pnpm dev:wasapi`; `--<knob>=<value>` becomes
// `VITE_LF_PROBE_<KNOB>` (`--filter=Pro-Q,Saturn`, `--hold=`, `--settle=`, `--params=`, `--plugins=`:
// each probe's header lists its knobs); `--stall-min=<n>` (default 3) fails the run when the probe
// prints nothing for that long. Refuses to start while an app or the port-1420 server runs
// (`pnpm native:kill`). Launches, waits for the probe's verdict line, stops the whole run and exits 0
// only on a clean verdict. A phased probe launches once per phase with `VITE_LF_PROBE_PHASE` set; a
// phase ends the way its probe says: the app closes itself (`close`), the runner sends its window the
// OS close on the verdict line, as the close button does (`os-close`: Rust hands it to the app's close
// guard), the runner kills app.exe alone on that line, as a crashing plugin would (`crash`), or the
// page has app.exe abort itself after that line and the runner waits for it to go (`abort`). The
// full log lands in logs/native-<probe>.log. Windows node only: the app windows open on the PC desktop.

import { execFileSync, spawn } from 'node:child_process';
import { createWriteStream, mkdirSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { appRunning, assertWindows, closeAppWindow, killNative, nativeRunning } from './native-kill.mjs';

// Every probe runs in a profile of its own: `config` is a Tauri config overlay whose identifier gives the
// run its own WebView2 profile and app-local data folder, so the owner's jam, recovery, settings and
// plugin tones are never read or written.
const CLASSIC = 'scripts/classic-probe.tauri.json';
const PROBES = {
  'editor-premise': { tag: 'premise', end: /^(complete: .*|FAIL.*)$/, pass: /^complete: /, config: CLASSIC },
  'editor-smoke': { tag: 'smoke', end: /^(complete: .*|no plugins.*)$/, pass: /^complete: \d+ opened, 0 failed/, config: CLASSIC },
  'restart-survey': { tag: 'survey', end: /^(complete|no plugins.*)$/, pass: /^complete$/, config: CLASSIC },
  // `failLine`: a probe line that fails the run whatever the verdict says. The summary counts a
  // failed editor open as a failure and the swaps done with an editor open (at least one, or the
  // editor-open round proved nothing); verify/guards/native-swap-verdict.mjs holds both patterns.
  'swap-stress': {
    tag: 'swap',
    end: /^(complete: .*|TIMEOUT .*|ABORTED.*|need at least .*)$/,
    pass: /^complete: \d+ swapped, 0 failed, [1-9]\d* with an editor open$/,
    failLine: /^open failed /,
    config: CLASSIC,
  },
  // One launch, closed through its window as the close button does; any other frontend `console.error`
  // line fails it.
  'engine-smoke': {
    tag: 'engine-smoke',
    config: 'scripts/engine-probe.tauri.json',
    cleanLog: true,
    phases: [{ name: 'smoke', end: /^(complete: .*|FAIL.*)$/, pass: /^complete: /, exit: 'os-close' }],
  },
  // engine-smoke's profile; one launch, closed through its window.
  'tuner-smoke': {
    tag: 'tuner-smoke',
    config: 'scripts/engine-probe.tauri.json',
    cleanLog: true,
    phases: [{ name: 'tuner', end: /^(complete: .*|FAIL.*)$/, pass: /^complete: /, exit: 'os-close' }],
  },
  // The same profile; the save phase ends with app.exe killed while its loops play, the restore phase
  // with a window close.
  'engine-recovery': {
    tag: 'engine-recovery',
    config: 'scripts/engine-probe.tauri.json',
    cleanLog: true,
    phases: [
      { name: 'save', end: /^(saved: .*|FAIL.*)$/, pass: /^saved: /, exit: 'crash' },
      { name: 'restore', end: /^(restored: .*|FAIL.*)$/, pass: /^restored: /, exit: 'os-close' },
    ],
  },
  // engine-smoke's profile; one launch runs every buffer size, closed through its window.
  'engine-loopback': {
    tag: 'engine-loopback',
    config: 'scripts/engine-probe.tauri.json',
    cleanLog: true,
    phases: [{ name: 'loopback', end: /^(complete: .*|FAIL.*)$/, pass: /^complete: /, exit: 'os-close' }],
  },
  // A profile of its own (its own tone store); save closes itself, check is killed once its tones are
  // saved by the debounce alone, after closes itself. `set: ` hands over like `saved: `.
  'tone-recall': {
    tag: 'tone',
    config: 'scripts/tone-probe.tauri.json',
    cleanLog: true,
    phases: [
      { name: 'save', end: /^(saved: .*|FAIL.*)$/, pass: /^saved: /, exit: 'close' },
      { name: 'check', end: /^(set: .*|FAIL.*)$/, pass: /^set: /, exit: 'crash' },
      { name: 'after', end: /^(restored: .*|FAIL.*)$/, pass: /^restored: /, exit: 'close' },
    ],
  },
  // A profile of its own (its output moves to the virtual cable); one launch, closed through its window.
  'export-master': {
    tag: 'export-master',
    config: 'scripts/export-probe.tauri.json',
    cleanLog: true,
    phases: [{ name: 'export', end: /^(complete: .*|FAIL.*)$/, pass: /^complete: /, exit: 'os-close' }],
  },
  // A profile of its own; one launch (the probe reloads the WebView inside it), closed through its
  // window. `rustErrors`: a Rust ERROR line fails it too.
  'midi-switch': {
    tag: 'midi-switch',
    config: 'scripts/midi-probe.tauri.json',
    cleanLog: true,
    rustErrors: true,
    phases: [{ name: 'switch', end: /^(complete: .*|FAIL.*)$/, pass: /^complete: /, exit: 'os-close' }],
  },
  // The MIDI benchmark's jam load (run with `LF_MIDI_BENCH` set); engine-smoke's profile, one launch,
  // closed through its window. `requireLine`: the log must carry it, the benchmark's report.
  'bench-load': {
    tag: 'bench-load',
    config: 'scripts/engine-probe.tauri.json',
    cleanLog: true,
    requireLine: /\[midi-bench\] report: /,
    phases: [{ name: 'load', end: /^(complete: .*|FAIL.*)$/, pass: /^complete: /, exit: 'os-close' }],
  },
  // A profile of its own (its keys only). Each way of dying writes a stamp and is followed by a launch that
  // reads it back; `abort-<ms>` has app.exe abort itself <ms> after the write, `kill` is killed by the
  // runner. `webviewCensus`: after each phase, count the profile's WebView2 processes and wait until they
  // are gone (up to 30 s), so a launch never reuses the last one's browser process.
  'storage-kill': {
    tag: 'storage-kill',
    config: 'scripts/storage-probe.tauri.json',
    webviewCensus: true,
    phases: [0, 50, 500, 2000].flatMap((ms) => [
      { name: `abort-${ms}`, end: /^(armed: .*|FAIL.*)$/, pass: /^armed: /, exit: 'abort' },
      { name: `read-abort-${ms}`, end: /^(read: .*|FAIL.*)$/, pass: /^read: /, exit: 'close' },
    ]).concat([
      { name: 'kill', end: /^(set: .*|FAIL.*)$/, pass: /^set: /, exit: 'crash' },
      { name: 'read-kill', end: /^(read: .*|FAIL.*)$/, pass: /^read: /, exit: 'close' },
    ]),
  },
  // `recallLines`: how many `[rig-recall]` log lines the phase must print.
  'recall-restart': {
    tag: 'recall',
    config: CLASSIC,
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
const WEBVIEW_WAIT_MS = 30_000; // how long a census waits for the profile's WebView2 processes to go

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

// The probe's own profile: its identifier names the app-local data folder (%LOCALAPPDATA%\<identifier>
// on Windows). The default identifier is the owner's profile: never used.
const identifier = JSON.parse(readFileSync(join(root, spec.config), 'utf8')).identifier;
if (!identifier || identifier === 'com.bleeploop.app') {
  console.error(`native probe ${name}: ${spec.config} must name a probe profile of its own`);
  process.exit(1);
}
const logPath = join(root, 'logs', `native-${name}.log`);
mkdirSync(dirname(logPath), { recursive: true });
const log = createWriteStream(logPath);
console.log(`native probe ${name}: pnpm ${devScript}, log ${join('logs', `native-${name}.log`)}`);
const tagged = new RegExp(`\\[${spec.tag}\\] (.*)$`);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Stop the dev run and whatever it left; returns the "name pid" lines `killNative` stopped. The tree
 * kill takes the run's own cargo; the sweep leaves any other cargo alone (someone else's build). */
function stopRun(child) {
  try {
    execFileSync('taskkill', ['/T', '/F', '/PID', String(child.pid)], { stdio: 'ignore' });
  } catch {
    // already gone
  }
  return killNative({ cargo: false });
}

/** How many msedgewebview2.exe processes run on this profile's user data folder (its command line names
 * the identifier's folder); null when PowerShell could not tell. */
function webviewCount() {
  const ps = `@(Get-CimInstance Win32_Process -Filter "Name='msedgewebview2.exe'" | Where-Object { $_.CommandLine -like '*${identifier}*' }).Count`;
  try {
    return Number(execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', ps], { encoding: 'utf8' }).trim());
  } catch {
    return null;
  }
}

/** After app.exe is gone: the profile's WebView2 processes then, and how long they took to go (ms; null
 * when they outlived the wait). */
async function webviewCensus() {
  const t0 = Date.now();
  const atExit = webviewCount();
  let left = atExit;
  while (left && Date.now() - t0 < WEBVIEW_WAIT_MS) {
    await sleep(250);
    left = webviewCount();
  }
  const goneMs = left === 0 ? Date.now() - t0 : null;
  console.log(`  WebView2: ${atExit ?? '?'} process(es) of this profile when app.exe was seen gone; ${goneMs === null ? `${left ?? '?'} left after ${WEBVIEW_WAIT_MS / 1000} s` : `none after ${goneMs} ms`}`);
  return { atExit, goneMs };
}

/**
 * One launch: resolves `{ line, reason, recallLines }` — `line` is the verdict line (null when there
 * was none), `reason` says why not, `recallLines` counts the launch's `[rig-recall]` lines.
 */
function launch(phase, phaseEnv) {
  return new Promise((resolve) => {
    if (phase.name) log.write(`\n===== phase ${phase.name} =====\n`);
    const child = spawn(`pnpm ${devScript} --config ${spec.config}`, { cwd: root, env: phaseEnv, shell: true, stdio: ['ignore', 'pipe', 'pipe'] });
    let lastLine = Date.now();
    let seen = false;
    let partial = '';
    let recallLines = 0;
    let requiredSeen = false;
    let killLag = null;
    let census = null;
    const lines = [];
    const failLines = [];
    const webviewErrors = [];
    const rustErrors = [];
    let verdict;
    let settled = false;

    const done = (line, reason) => {
      if (settled) return;
      settled = true;
      clearInterval(watchdog);
      resolve({ line, reason, recallLines, requiredSeen, lines, killLag, census, failLines, webviewErrors, rustErrors, stopped: stopRun(child) });
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
        // A line that carries the page's `Date.now()` (`"at":`): the lag from the page to the kill, on the
        // one system clock both read.
        const at = Number(line.match(/"at":(\d+)/)?.[1]);
        if (at) {
          killLag = { line: t0 - at, kill: Date.now() - at };
          console.log(`  the line reached the runner ${killLag.line} ms after the page's write; app.exe killed ${killLag.kill} ms after it`);
        }
      }
      if (phase.exit !== 'kill') {
        const deadline = Date.now() + EXIT_WAIT_MS;
        while (appRunning()) {
          if (Date.now() > deadline) return done(null, `app.exe did not quit within ${EXIT_WAIT_MS / 1000} s after: ${line}`);
          await sleep(500);
        }
        if (spec.webviewCensus) census = await webviewCensus();
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
        if (spec.requireLine?.test(clean)) requiredSeen = true;
        if (spec.cleanLog && clean.includes('[webview][ERROR]') && !clean.includes(`[${spec.tag}]`)) webviewErrors.push(clean);
        if (spec.rustErrors && /\]\[ERROR\]/.test(clean) && !clean.includes('[webview]')) rustErrors.push(clean);
        const msg = clean.match(tagged)?.[1]?.trimEnd();
        if (msg === undefined) continue;
        seen = true;
        lastLine = Date.now();
        lines.push(msg);
        if (spec.failLine?.test(msg)) failLines.push(msg);
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
const ran = [];
for (const phase of phases) {
  const phaseEnv = { ...env, ...(phase.name ? { VITE_LF_PROBE_PHASE: phase.name } : {}) };
  if (handover) phaseEnv.VITE_LF_PROBE_EXPECT = handover;
  result = await launch(phase, phaseEnv);
  ran.push({ phase, result });
  if (result.stopped.length) console.log(`  stopped ${result.stopped.join(', ')}`);
  ok = result.line !== null && phase.pass.test(result.line);
  if (ok && result.failLines.length) {
    ok = false;
    for (const line of result.failLines) console.log(`  [${spec.tag}] ${line}`);
    result.reason = `${result.failLines.length} line(s) matching ${spec.failLine}`;
    result.line = null;
  }
  if (ok && spec.cleanLog && result.webviewErrors.length) {
    ok = false;
    for (const line of result.webviewErrors) console.log(`  ${line}`);
    result.reason = `${result.webviewErrors.length} frontend console.error line(s) besides the probe's own`;
    result.line = null;
  }
  if (ok && spec.rustErrors && result.rustErrors.length) {
    ok = false;
    for (const line of result.rustErrors) console.log(`  ${line}`);
    result.reason = `${result.rustErrors.length} Rust ERROR line(s)`;
    result.line = null;
  }
  if (ok && spec.rustErrors) console.log(`  [${spec.tag}] log: no Rust ERROR line`);
  if (ok && spec.requireLine && !result.requiredSeen) {
    ok = false;
    result.reason = `the log has no line matching ${spec.requireLine}`;
    result.line = null;
  }
  if (ok && phase.recallLines !== undefined && result.recallLines !== phase.recallLines) {
    ok = false;
    result.reason = `${phase.name}: ${result.recallLines} [rig-recall] line(s), expected ${phase.recallLines}`;
    result.line = null;
  }
  if (ok && /^(saved|set|armed): /.test(result.line)) handover = result.line.replace(/^\w+: /, '');
  if (!ok) {
    if (phase.name && result.line) result.line = `${phase.name}: ${result.line}`;
    break;
  }
}
await new Promise((r) => log.end(r));
const extra = name === 'restart-survey' && result.line ? `; ${surveySummary()}` : name === 'storage-kill' ? `; ${storageSummary()}` : '';
const shown = ok && spec.phases ? `${spec.phases.length} phases, last: ${result.line}` : (result.line ?? result.reason);
console.log(`\n=== ${name}: ${ok ? 'PASS' : 'FAIL'}: ${shown}${extra} ===`);
process.exit(ok ? 0 : 1);

// What the survey learned, as the counts its baseline in docs/VERIFY.md uses.
function surveySummary() {
  const text = readFileSync(logPath, 'utf8');
  const flags = new Map();
  // The engine's plugin owners' lines (`host/vst3_engine.rs`, `host/clap_engine.rs`).
  for (const m of text.matchAll(/engine slot \d+ restartComponent\(([^)]*)\)/g)) flags.set(m[1], (flags.get(m[1]) ?? 0) + 1);
  const vst3 = [...flags.values()].reduce((a, b) => a + b, 0);
  const clap = (text.match(/engine slot \d+ restart at the plugin's request/g) ?? []).length;
  const failed = (text.match(/\[survey\] LOAD FAILED/g) ?? []).length;
  const byFlag = [...flags].map(([f, n]) => `${f} ${n}`).join(', ');
  return `${vst3} VST3 restartComponent${byFlag ? ` (${byFlag})` : ''}, ${clap} CLAP request_restart line(s), ${failed} load failure(s)`;
}

// storage-kill's table: per way of dying, the lag from the write to the death, the profile's WebView2
// processes at app.exe's death and how long they lived on, and whether the read-back found the stamp.
function storageSummary() {
  const rows = [];
  for (const { phase, result } of ran) {
    if (phase.name.startsWith('read-')) continue;
    const read = ran.find((r) => r.phase.name === `read-${phase.name}`)?.result.line?.match(/survived=(yes|no)/)?.[1] ?? 'not read';
    const abortLag = result.lines.map((l) => l.match(/^abort: called (-?\d+) ms after/)?.[1]).find(Boolean);
    const lag = phase.exit === 'crash' ? (result.killLag ? `${result.killLag.kill} ms (line ${result.killLag.line} ms)` : '?') : abortLag !== undefined ? `${abortLag} ms` : '? (no abort line)';
    const wv = result.census ? `${result.census.atExit ?? '?'} WebView2 at death, ${result.census.goneMs === null ? 'outlived the wait' : `gone in ${result.census.goneMs} ms`}` : 'WebView2 not counted';
    const way = phase.exit === 'crash' ? 'runner kill' : `abort after ${phase.name.slice('abort-'.length)} ms`;
    rows.push(`${way}: write to death ${lag}; ${wv}; survived ${read}`);
  }
  console.log(`\nstorage-kill:\n${rows.map((r) => `  ${r}`).join('\n')}`);
  return rows.map((r) => `${r.split(':')[0]} ${r.match(/survived (.*)$/)[1]}`).join(', ');
}
