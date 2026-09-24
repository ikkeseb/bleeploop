// scripts/native-spike.mjs — the native-engine Stage 1 premise spike (docs/plans/native-engine.md):
// builds the debug app with ASIO, runs the `--probe-engine-spike` / `--probe-share` matrix one
// process at a time and prints one PASS/FAIL table.
//
//   pnpm native:spike [--only=asio,echo,c1,wasapi,share] [--blocks=64,128,256] [--launches=4]
//                     [--long-min=10] [--in=1] [--out=1] [--device=Focusrite] [--proq=<vst3>] [--amp=<vst3>]
//
// Needs the loopback cable (default: line out R → input 2, `--in=1 --out=1`, 0-based) and no running
// `app`. The chirps are audible on the interface's outputs: turn the monitors down. Runs at the
// device's current rate and never changes it. The full log lands in logs/native-spike.log. Windows
// node only.

import { execFileSync, spawn } from 'node:child_process';
import { createWriteStream, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { appRunning, assertWindows } from './native-kill.mjs';

assertWindows('native:spike');
const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const vst3 = join(process.env.COMMONPROGRAMFILES ?? 'C:\\Program Files\\Common Files', 'VST3');
const opt = {
  only: 'asio,echo,c1,wasapi,share',
  blocks: '64,128,256',
  launches: '4',
  'long-min': '10',
  in: '1',
  out: '1',
  device: 'Focusrite',
  proq: join(vst3, 'FabFilter', 'FabFilter Pro-Q 3.vst3'),
  amp: join(vst3, 'Neural DSP', 'Archetype Petrucci X.vst3'),
};
for (const arg of process.argv.slice(2)) {
  const m = arg.match(/^--([\w-]+)=(.*)$/);
  if (!m || !(m[1] in opt)) {
    console.error(`unknown argument ${arg}`);
    process.exit(1);
  }
  opt[m[1]] = m[2];
}
const only = new Set(opt.only.split(','));
const blocks = opt.blocks.split(',');
const chans = ['--in', opt.in, '--out', opt.out];

if (appRunning()) {
  console.error('native:spike: an app is running. Close it or run pnpm native:kill first.');
  process.exit(1);
}

const logPath = join(root, 'logs', 'native-spike.log');
mkdirSync(dirname(logPath), { recursive: true });
const log = createWriteStream(logPath);
const say = (line) => {
  console.log(line);
  log.write(`${line}\n`);
};

say('native:spike: cargo build --features asio (debug)');
execFileSync('cargo', ['build', '--features', 'asio'], { cwd: join(root, 'src-tauri'), stdio: 'inherit' });
const exe = join(root, 'src-tauri', 'target', 'debug', 'app.exe');

/** One probe process: resolves with its parsed JSON phases and verdict lines. */
function probe(label, args, timeoutS) {
  return new Promise((resolve) => {
    say(`--- ${label}: app.exe ${args.join(' ')}`);
    const child = spawn(exe, args, { stdio: ['ignore', 'pipe', 'pipe'] });
    const result = { label, json: [], verdicts: [], invalid: null, exit: null };
    const timer = setTimeout(() => {
      say(`${label}: TIMEOUT after ${timeoutS} s`);
      child.kill();
    }, timeoutS * 1000);
    let buf = '';
    const onData = (chunk) => {
      buf += chunk;
      let nl;
      while ((nl = buf.indexOf('\n')) >= 0) {
        const line = buf.slice(0, nl).trimEnd();
        buf = buf.slice(nl + 1);
        log.write(`${line}\n`);
        const m = line.match(/^\[(engine-spike|share-probe)\] (.*)$/);
        if (!m) continue;
        const body = m[2];
        if (body.startsWith('{')) result.json.push(JSON.parse(body));
        else if (/^(PASS|FAIL) /.test(body)) {
          const [, pass, id, rest] = body.match(/^(PASS|FAIL) (\S+) (.*)$/);
          result.verdicts.push({ pass: pass === 'PASS', id, rest });
          say(`  ${body}`);
        } else if (body.startsWith('INVALID')) {
          result.invalid = body;
          say(`  ${body}`);
        } else if (!body.startsWith('started') && !body.startsWith('plugin unloaded')) say(`  ${body}`);
      }
    };
    child.stdout.on('data', onData);
    child.stderr.on('data', onData);
    child.on('exit', (code) => {
      clearTimeout(timer);
      result.exit = code;
      resolve(result);
    });
  });
}

const rows = [];
const row = (id, where, pass, value, bar) => rows.push({ id, where, verdict: pass === null ? 'INFO' : pass ? 'PASS' : 'FAIL', value, bar });
const fromRun = (r, where) => {
  if (r.invalid) row('INVALID', where, false, r.invalid.replace(/^INVALID /, ''), 'xcorr >= 0.8 (setup fault: fix and rerun)');
  else if (r.exit !== 0 && !r.verdicts.length) row('ERROR', where, false, `exit ${r.exit}`, 'the probe runs');
  for (const v of r.verdicts) row(v.id, where, v.pass, v.rest, '');
};

const longS = Number(opt['long-min']) * 60;
if (only.has('asio')) {
  for (const b of blocks) {
    const medians = [];
    const long = await probe(`asio ${b} long`, ['--probe-engine-spike', 'asio', b, ...chans, '--seconds', String(longS)], longS + 120);
    fromRun(long, `asio ${b} ${opt['long-min']} min`);
    if (long.json[0]?.lagMedianFrames != null) medians.push(long.json[0].lagMedianFrames);
    for (let i = 0; i < Number(opt.launches); i++) {
      const r = await probe(`asio ${b} launch ${i + 1}`, ['--probe-engine-spike', 'asio', b, ...chans, '--seconds', '60'], 180);
      fromRun(r, `asio ${b} launch ${i + 1}`);
      if (r.json[0]?.lagMedianFrames != null) medians.push(r.json[0].lagMedianFrames);
    }
    if (medians.length >= 2) {
      const move = Math.max(...medians) - Math.min(...medians);
      row('A3.launches', `asio ${b}`, move <= 2, `median moves ${move.toFixed(2)}f over ${medians.length} launches (${medians.map((m) => m.toFixed(1)).join(', ')})`, '<= 2 frames across launches');
    }
  }
}
if (only.has('echo')) {
  for (const b of blocks) {
    const r = await probe(`asio ${b} echo Pro-Q`, ['--probe-engine-spike', 'asio', b, ...chans, '--seconds', '60', '--echo', '--plugin', opt.proq], 180);
    fromRun(r, `asio ${b} echo`);
  }
}
if (only.has('c1')) {
  for (const b of ['128', '256']) {
    const r = await probe(`asio ${b} amp`, ['--probe-engine-spike', 'asio', b, ...chans, '--seconds', '125', '--quiet', '--plugin', opt.amp], 300);
    fromRun(r, `asio ${b} amp`);
  }
}
if (only.has('wasapi')) {
  const dev = ['--device', opt.device];
  const w1 = await probe('wasapi', ['--probe-engine-spike', 'wasapi', 'default', ...chans, ...dev, '--seconds', '60'], 180);
  fromRun(w1, 'wasapi');
  const w2 = await probe('wasapi echo', ['--probe-engine-spike', 'wasapi', 'default', ...chans, ...dev, '--seconds', '60', '--echo'], 180);
  fromRun(w2, 'wasapi echo');
  const j = w2.json[0];
  if (j?.rtMs != null) {
    const p = j.rtParts;
    const rem = j.rtMs - p.sumMs;
    row('W2', 'wasapi echo', null, `RT ${j.rtMs.toFixed(1)}ms = input age ${p.inputAgeMs.toFixed(1)} + ring ${p.ringMs.toFixed(1)} + output ${p.outputLatencyMs.toFixed(1)}, remainder ${Math.abs(rem) > 5 ? 'unknown ' : ''}${rem.toFixed(1)}ms`, 'printed with its parts (no bar)');
  }
}
if (only.has('share')) {
  const r = await probe('share', ['--probe-share', 'all'], 180);
  fromRun(r, 'share');
}

say('\n=== native:spike ===');
say('| verdict | id | where | value | bar |');
say('|---|---|---|---|---|');
for (const r of rows) say(`| ${r.verdict} | ${r.id} | ${r.where} | ${r.value} | ${r.bar} |`);
const failed = rows.filter((r) => r.verdict === 'FAIL').length;
say(`=== ${failed ? `${failed} FAIL` : 'all PASS'} — log ${join('logs', 'native-spike.log')} ===`);
log.end();
process.exitCode = failed ? 1 : 0;
