// scripts/engine-test.mjs — the engine's tests on demand: `cargo test -p lf-engine
// --no-default-features` from src-tauri/ (the pure engine crate, no device, no SDK). Not in
// `pnpm check`: minutes on Windows even warm and side by side (scripts/cargo-test.mjs), so the pre-push
// hook leaves them to CI's engine job, which runs them on every push that touches src-tauri/. Where cargo is absent (the Mac has no Rust toolchain) it warns and skips.
//
//   pnpm test:engine

import { spawnSync } from 'node:child_process';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { cargoTest } from './cargo-test.mjs';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const cwd = join(root, 'src-tauri');
const args = ['test', '-p', 'lf-engine', '--no-default-features'];

const probe = spawnSync('cargo', ['--version'], { cwd, encoding: 'utf8' });
if (probe.error || probe.status !== 0) {
  console.warn(`  SKIP  cargo ${args.join(' ')}  (no cargo on this machine: ${probe.error?.message ?? probe.stderr.trim()})`);
  process.exit(0);
}

const started = Date.now();
const { code, out, passed } = await cargoTest(args.slice(1));
const seconds = ((Date.now() - started) / 1000).toFixed(0);
console.log(`  ${code === 0 ? 'PASS' : 'FAIL'}  cargo ${args.join(' ')}  ${seconds} s, ${passed} tests passed`);
if (code !== 0) process.stderr.write(`${out.trim().slice(-4000)}\n`);
process.exit(code === 0 ? 0 : 1);
