// verify/run-guards.mjs — run every guard in verify/guards/ and summarize.
//
// Runs deterministic audio-logic, format and repository-contract checks without a browser or hardware.
// Browser probes and Rust tests run separately. Coverage and usage: verify/README.md.
//
// Run: pnpm verify   (or: node verify/run-guards.mjs)
// Exit code is 0 only if every verifier passed; non-zero (and a FAIL list) otherwise.

import { readdirSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const here = join(dirname(fileURLToPath(import.meta.url)), 'guards');

const files = readdirSync(here)
  .filter((f) => f.endsWith('.mjs'))
  .sort();

if (files.length === 0) {
  console.error('no guards found in', here);
  process.exit(1);
}

let totalChecks = 0;
let failedFiles = 0;
const rows = [];

for (const f of files) {
  const res = spawnSync(process.execPath, [join(here, f)], { encoding: 'utf8' });
  const out = `${res.stdout || ''}${res.stderr || ''}`;
  const m = out.match(/(\d+)\/(\d+) checks passed/);
  const checks = m ? Number(m[2]) : 0;
  const failedInFile = m ? Number(m[2]) - Number(m[1]) : NaN;
  // checks > 0: a verifier reporting "0/0 checks passed" (a fully-rotted guard that asserts nothing)
  // must FAIL, not silently pad the green summary.
  const ok = res.status === 0 && m && failedInFile === 0 && checks > 0;
  totalChecks += checks;
  if (!ok) failedFiles += 1;
  rows.push({
    f,
    ok,
    summary: m ? (checks > 0 ? `${m[1]}/${m[2]}` : `${m[1]}/${m[2]} (0 checks ran)`) : '(no RESULT line)',
    status: res.status,
  });
  if (!ok) {
    // surface the failing detail so a regression is immediately actionable
    process.stderr.write(`\n----- FAIL: ${f} (exit ${res.status}) -----\n${out.trim()}\n`);
  }
}

const pad = Math.max(...rows.map((r) => r.f.length));
console.log('\nBleepLoop guards');
for (const r of rows) {
  console.log(`  ${r.ok ? 'PASS' : 'FAIL'}  ${r.f.padEnd(pad)}  ${r.summary}`);
}
console.log(
  `\n=== ${files.length - failedFiles}/${files.length} guards passed, ${totalChecks} total checks, ${failedFiles} file(s) failed ===`,
);

process.exit(failedFiles === 0 ? 0 : 1);
