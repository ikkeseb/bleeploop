// scripts/engine-test.mjs — the engine's tests on demand: `cargo test -p lf-engine
// --no-default-features` from src-tauri/ (the pure engine crate, no device, no SDK). Not in
// `pnpm check`: minutes on Windows even warm and side by side (scripts/cargo-test.mjs), so the pre-push
// hook leaves them to CI's engine job, which runs them on every push that touches src-tauri/. Where cargo is absent (the Mac has no Rust toolchain) it warns and skips; a cargo that is there but
// fails (a broken toolchain, a refused spawn, a signal) fails the run with its error.
//
//   pnpm test:engine

import { spawnSync } from 'node:child_process';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { cargoTest } from './cargo-test.mjs';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const cwd = join(root, 'src-tauri');
const args = ['test', '-p', 'lf-engine', '--no-default-features'];

/**
 * What `cargo --version`'s outcome means for the run: `skip` only when no cargo executable exists
 * (the spawn's ENOENT), `fail` for any other spawn error, a non-zero exit or a signal, else `run`.
 * @param {import('node:child_process').SpawnSyncReturns<string>} probe
 * @returns {{ verdict: 'run' | 'skip' | 'fail', why: string }}
 */
export function cargoPresence(probe) {
  if (probe.error) {
    const code = /** @type {NodeJS.ErrnoException} */ (probe.error).code;
    return { verdict: code === 'ENOENT' ? 'skip' : 'fail', why: probe.error.message };
  }
  if (probe.signal) return { verdict: 'fail', why: `cargo --version was killed by ${probe.signal}` };
  if (probe.status !== 0) return { verdict: 'fail', why: `cargo --version exited ${probe.status}: ${String(probe.stderr ?? '').trim()}` };
  return { verdict: 'run', why: String(probe.stdout ?? '').trim() };
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const presence = cargoPresence(spawnSync('cargo', ['--version'], { cwd, encoding: 'utf8' }));
  if (presence.verdict === 'skip') {
    console.warn(`  SKIP  cargo ${args.join(' ')}  (no cargo on this machine: ${presence.why})`);
    process.exit(0);
  }
  if (presence.verdict === 'fail') {
    console.error(`  FAIL  cargo ${args.join(' ')}  (cargo is installed but does not start: ${presence.why})`);
    process.exit(1);
  }

  const started = Date.now();
  const { code, out, passed } = await cargoTest(args.slice(1));
  const seconds = ((Date.now() - started) / 1000).toFixed(0);
  console.log(`  ${code === 0 ? 'PASS' : 'FAIL'}  cargo ${args.join(' ')}  ${seconds} s, ${passed} tests passed`);
  if (code !== 0) process.stderr.write(`${out.trim().slice(-4000)}\n`);
  process.exit(code === 0 ? 0 : 1);
}
