// scripts/rust-check.mjs — the local Rust gate: `cargo check` without and with `asio`, then
// `cargo test`, all `--no-default-features`, from src-tauri/. CI runs only the no-asio check (and
// `cargo test` on native-code changes), so the asio half is proven here.
//
//   pnpm rust:check
//
// Windows node only (from WSL the pnpm wrapper runs it there, on Windows `cargo`). Runs every step even
// after a failure; the full output lands in logs/rust-check.log.

import { spawn } from 'node:child_process';
import { createWriteStream, existsSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { assertWindows } from './native-kill.mjs';

assertWindows('rust:check');

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const logPath = join(root, 'logs', 'rust-check.log');
mkdirSync(dirname(logPath), { recursive: true });
const log = createWriteStream(logPath);

const STEPS = [
  ['check', '--no-default-features'],
  ['check', '--no-default-features', '--features', 'asio'],
  ['test', '--no-default-features'],
];

// asio-sys only rebuilds when its fingerprint changes, so a green asio check can be a stale target/
// cache over a missing SDK (src-tauri/AGENTS.md, ASIO SDK env).
function asioSdkMissing() {
  const dir = process.env.CPAL_ASIO_DIR;
  if (!dir) return 'CPAL_ASIO_DIR is not set';
  if (!existsSync(join(dir, 'common')) || !existsSync(join(dir, 'host', 'pc'))) return `CPAL_ASIO_DIR (${dir}) lacks common/ or host/pc/`;
  return null;
}

function cargo(args) {
  return new Promise((resolve) => {
    log.write(`\n$ cargo ${args.join(' ')}\n`);
    const child = spawn('cargo', args, { cwd: join(root, 'src-tauri'), stdio: ['ignore', 'pipe', 'pipe'] });
    let out = '';
    const collect = (chunk) => {
      log.write(chunk);
      out += chunk;
    };
    child.stdout.on('data', collect);
    child.stderr.on('data', collect);
    child.on('error', (e) => resolve({ code: -1, out: String(e) }));
    child.on('close', (code) => resolve({ code, out }));
  });
}

const failed = [];
for (const args of STEPS) {
  const label = `cargo ${args.join(' ')}`;
  const missing = args.includes('asio') ? asioSdkMissing() : null;
  if (missing) {
    console.log(`  FAIL  ${label}  (${missing})`);
    failed.push(label);
    continue;
  }
  const started = Date.now();
  const { code, out } = await cargo(args);
  const seconds = ((Date.now() - started) / 1000).toFixed(0);
  const tests = args[0] === 'test' ? [...out.matchAll(/test result: \w+\. (\d+) passed; (\d+) failed/g)] : [];
  const counts = tests.length ? `, ${tests.reduce((s, m) => s + Number(m[1]), 0)} tests passed` : '';
  console.log(`  ${code === 0 ? 'PASS' : 'FAIL'}  ${label}  ${seconds} s${counts}`);
  if (code !== 0) {
    failed.push(label);
    process.stderr.write(`${out.trim().slice(-4000)}\n\n`);
  }
}

await new Promise((resolve) => log.end(resolve));
console.log(`\n=== rust:check: ${STEPS.length - failed.length}/${STEPS.length} passed${failed.length ? `; FAILED: ${failed.join(', ')}` : ''} (log: logs/rust-check.log) ===`);
process.exit(failed.length ? 1 : 0);
