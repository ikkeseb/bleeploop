// verify/run-probes.mjs — run browser probes from verify/probes/ against a Vite server of their own.
//
//   pnpm probe <name> [<name> …] [probe args]   one or more probes; a single probe streams its output
//   pnpm probe --all                            every probe
//   pnpm probe --ci                             every probe without an `@no-ci <reason>` header line
//   pnpm probe --list                           names, and the reason a probe stays out of CI
//
// Starts Vite on a free port (no HMR, no watcher) and stops it afterwards; `--url=<server>` reuses a
// running one instead. Each probe runs as its own Node process with `--url`; it passes when it exits 0
// and printed a `=== RESULT: … passed ===` line. Output lands in logs/probes/<name>.log. Coverage and
// categories: verify/README.md.

import { spawn } from 'node:child_process';
import { mkdirSync, readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { createServer as createNetServer } from 'node:net';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const probesDir = join(here, 'probes');
const logDir = join(here, '..', 'logs', 'probes');
const TIMEOUT_MS = 10 * 60_000;

const all = readdirSync(probesDir).filter((f) => f.endsWith('.mjs')).map((f) => f.slice(0, -4)).sort();
const noCi = new Map();
for (const name of all) {
  const reason = readFileSync(join(probesDir, `${name}.mjs`), 'utf8').match(/@no-ci\s+(.+)/)?.[1];
  if (reason) noCi.set(name, reason.replace(/\s*\*\/\s*$/, '').trim());
}

const args = process.argv.slice(2);
const opts = new Set(args.filter((a) => a.startsWith('--') && !a.includes('=')));
const reuseUrl = args.find((a) => a.startsWith('--url='))?.slice(6);
const names = args.filter((a) => !a.startsWith('--'));
// Everything else (`--case=…`, `--headed`) passes through to the probes.
const passThrough = args.filter((a) => a.startsWith('--') && !a.startsWith('--url=') && !['--all', '--ci', '--list'].includes(a));

if (opts.has('--list')) {
  const pad = Math.max(...all.map((n) => n.length));
  for (const name of all) console.log(`  ${name.padEnd(pad)}  ${noCi.has(name) ? `no CI: ${noCi.get(name)}` : 'CI'}`);
  process.exit(0);
}

const unknown = names.filter((n) => !all.includes(n.replace(/\.mjs$/, '')));
if (unknown.length) {
  console.error(`unknown probe(s): ${unknown.join(', ')} — pnpm probe --list`);
  process.exit(1);
}
const selected = opts.has('--all') ? all : opts.has('--ci') ? all.filter((n) => !noCi.has(n)) : names.map((n) => n.replace(/\.mjs$/, ''));
if (selected.length === 0) {
  console.error('usage: pnpm probe <name…> | --all | --ci | --list   [--url=<running server>]');
  process.exit(1);
}
if (opts.has('--ci')) for (const [name, reason] of noCi) console.log(`  SKIP  ${name}  (${reason})`);

function freePort() {
  return new Promise((resolve, reject) => {
    const srv = createNetServer();
    srv.once('error', reject);
    srv.listen(0, '127.0.0.1', () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
  });
}

let vite;
let baseUrl = reuseUrl;
if (!baseUrl) {
  const { createServer } = await import('vite');
  const port = await freePort();
  vite = await createServer({
    logLevel: 'warn',
    server: { host: '127.0.0.1', port, strictPort: true, hmr: false, watch: null },
  });
  await vite.listen();
  baseUrl = `http://127.0.0.1:${port}`;
  console.log(`probe: Vite on ${baseUrl}`);
}

const stream = selected.length === 1;
mkdirSync(logDir, { recursive: true });

function run(name) {
  return new Promise((resolve) => {
    const started = Date.now();
    const child = spawn(process.execPath, [join(probesDir, `${name}.mjs`), `--url=${baseUrl}`, ...passThrough], {
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    let out = '';
    const collect = (sink) => (chunk) => {
      out += chunk;
      if (stream) sink.write(chunk);
    };
    child.stdout.on('data', collect(process.stdout));
    child.stderr.on('data', collect(process.stderr));
    const timer = setTimeout(() => {
      out += `\n[run-probes] timed out after ${TIMEOUT_MS / 1000} s\n`;
      child.kill();
    }, TIMEOUT_MS);
    child.on('close', (code) => {
      clearTimeout(timer);
      writeFileSync(join(logDir, `${name}.log`), out);
      const result = out.match(/=== RESULT: .*===/g)?.at(-1) ?? '';
      const ok = code === 0 && /passed/.test(result) && !/FAILED/.test(result);
      resolve({ name, ok, code, seconds: (Date.now() - started) / 1000, out });
    });
  });
}

const rows = [];
try {
  for (const name of selected) {
    if (!stream) process.stdout.write(`  …     ${name}\r`);
    const row = await run(name);
    rows.push(row);
    console.log(`  ${row.ok ? 'PASS' : 'FAIL'}  ${name}  ${row.seconds.toFixed(1)} s${row.ok ? '' : `  (exit ${row.code})`}`);
    if (!row.ok && !stream) process.stderr.write(`\n----- FAIL: ${name} -----\n${row.out.trim()}\n\n`);
  }
} finally {
  await vite?.close();
}

const failed = rows.filter((r) => !r.ok);
const total = rows.reduce((s, r) => s + r.seconds, 0);
console.log(`\n=== ${rows.length - failed.length}/${rows.length} probes passed in ${total.toFixed(0)} s${failed.length ? `; FAILED: ${failed.map((r) => r.name).join(', ')}` : ''} ===`);
process.exit(failed.length === 0 ? 0 : 1);
