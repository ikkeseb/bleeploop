// scripts/engine-test.mjs — the engine's tests in the local gate: `cargo test -p lf-engine
// --no-default-features` from src-tauri/ (the pure engine crate, no device, no SDK). Part of
// `pnpm check`, so the pre-push hook runs them. Where cargo is absent (the Mac has no Rust toolchain)
// it warns and skips; CI's engine job runs them there.
//
//   pnpm test:engine

import { spawn, spawnSync } from 'node:child_process';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const cwd = join(root, 'src-tauri');
const args = ['test', '-p', 'lf-engine', '--no-default-features'];

const probe = spawnSync('cargo', ['--version'], { cwd, encoding: 'utf8' });
if (probe.error || probe.status !== 0) {
  console.warn(`  SKIP  cargo ${args.join(' ')}  (no cargo on this machine: ${probe.error?.message ?? probe.stderr.trim()})`);
  process.exit(0);
}

const started = Date.now();
const child = spawn('cargo', args, { cwd, stdio: ['ignore', 'pipe', 'pipe'] });
let out = '';
const collect = (chunk) => {
  out += chunk;
};
child.stdout.on('data', collect);
child.stderr.on('data', collect);
const code = await new Promise((resolve) => {
  child.on('error', () => resolve(-1));
  child.on('close', resolve);
});
const seconds = ((Date.now() - started) / 1000).toFixed(0);
const counts = [...out.matchAll(/test result: \w+\. (\d+) passed; (\d+) failed/g)];
const passed = counts.reduce((sum, m) => sum + Number(m[1]), 0);
console.log(`  ${code === 0 ? 'PASS' : 'FAIL'}  cargo ${args.join(' ')}  ${seconds} s, ${passed} tests passed`);
if (code !== 0) process.stderr.write(`${out.trim().slice(-4000)}\n`);
process.exit(code === 0 ? 0 : 1);
