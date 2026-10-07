// scripts/cargo-test.mjs — `cargo test`, its test binaries run side by side. Cargo runs them one after
// another, and most of the wall time is a few binaries with one long test each: the same binaries in a
// pool finish in about the longest one's time. Built by cargo (`--no-run`), each binary is then run as
// cargo runs it (its package's directory, `CARGO_MANIFEST_DIR`, the target's `deps` on PATH), and the
// doc-tests go through `cargo test --doc`. A binary's own tests keep libtest's threads.
//
//   node scripts/cargo-test.mjs [--jobs N] <cargo test's package and feature arguments>
//   node scripts/cargo-test.mjs --workspace --no-default-features
//
// `--jobs` is how many binaries run at once (default: one per logical processor, so the long ones start
// at once; on the dev PC's 16 that ran the workspace's 47 binaries in 143 s against cargo's 443);
// `--jobs 1` is cargo's own order. Exit 0 = every binary and the doc-tests passed.

import { spawn } from 'node:child_process';
import { availableParallelism } from 'node:os';
import { delimiter, dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const cwd = join(dirname(fileURLToPath(import.meta.url)), '..', 'src-tauri');

function run(command, args, options, onChunk) {
  return new Promise((resolve) => {
    const child = spawn(command, args, { stdio: ['ignore', 'pipe', 'pipe'], ...options });
    let [stdout, stderr] = ['', ''];
    child.stdout.on('data', (chunk) => {
      stdout += chunk;
      onChunk?.(chunk, 'stdout');
    });
    child.stderr.on('data', (chunk) => {
      stderr += chunk;
      onChunk?.(chunk, 'stderr');
    });
    child.on('error', (e) => resolve({ code: -1, stdout, stderr: `${stderr}${e}\n` }));
    child.on('close', (code) => resolve({ code: code ?? -1, stdout, stderr }));
  });
}

/**
 * `cargo test <args>` with the test binaries in a pool of `jobs`.
 * @param {string[]} args cargo test's package and feature arguments
 * @param {{ jobs?: number, log?: (text: string) => void }} [options] `log` takes everything cargo and the tests print
 * @returns {Promise<{ code: number, out: string, binaries: number, passed: number }>} `out`: the failed steps' output
 */
export async function cargoTest(args, { jobs = availableParallelism(), log = () => {} } = {}) {
  // The build: cargo's messages name each test binary and its package's manifest.
  const build = await run('cargo', ['test', ...args, '--no-run', '--message-format=json-render-diagnostics'], { cwd }, (chunk, stream) => {
    if (stream === 'stderr') log(String(chunk));
  });
  if (build.code !== 0) return { code: build.code, out: build.stderr, binaries: 0, passed: 0 };
  const binaries = [];
  for (const line of build.stdout.split('\n')) {
    if (!line.startsWith('{')) continue;
    const message = JSON.parse(line);
    if (message.reason === 'compiler-artifact' && message.profile?.test && message.executable) {
      binaries.push({ exe: message.executable, manifest: message.manifest_path, name: message.target.name });
    }
  }
  if (binaries.length === 0) return { code: 1, out: 'cargo test --no-run named no test binary\n', binaries: 0, passed: 0 };

  let [next, passed] = [0, 0];
  const failed = [];
  const worker = async () => {
    while (next < binaries.length) {
      const { exe, manifest, name } = binaries[next++];
      const env = { ...process.env, CARGO_MANIFEST_DIR: dirname(manifest), PATH: `${dirname(exe)}${delimiter}${process.env.PATH ?? ''}` };
      const started = Date.now();
      const result = await run(exe, [], { cwd: dirname(manifest), env });
      const text = `\n     Running ${name} (${exe}) ${((Date.now() - started) / 1000).toFixed(1)} s\n${result.stdout}${result.stderr}`;
      log(text);
      for (const m of result.stdout.matchAll(/test result: \w+\. (\d+) passed;/g)) passed += Number(m[1]);
      if (result.code !== 0) failed.push(text);
    }
  };
  await Promise.all(Array.from({ length: Math.min(jobs, binaries.length) }, worker));

  const docs = await run('cargo', ['test', ...args, '--doc'], { cwd }, (chunk) => log(String(chunk)));
  for (const m of docs.stdout.matchAll(/test result: \w+\. (\d+) passed;/g)) passed += Number(m[1]);
  if (docs.code !== 0) failed.push(`\n     Doc-tests\n${docs.stdout}${docs.stderr}`);
  return { code: failed.length ? 1 : 0, out: failed.join('\n'), binaries: binaries.length, passed };
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  const args = process.argv.slice(2);
  const at = args.indexOf('--jobs');
  const jobs = at < 0 ? undefined : Number(args.splice(at, 2)[1]);
  if (jobs !== undefined && !(Number.isInteger(jobs) && jobs > 0)) {
    console.error('--jobs: expected a whole number above 0');
    process.exit(1);
  }
  const started = Date.now();
  const result = await cargoTest(args, { jobs, log: (text) => process.stdout.write(text) });
  if (result.code !== 0) process.stderr.write(`\n=== FAILED ===\n${result.out.trim().slice(-8000)}\n`);
  console.log(`\n=== cargo test ${args.join(' ')}: ${result.code === 0 ? 'ok' : 'FAILED'}, ${result.passed} tests passed in ${result.binaries} binaries, ${((Date.now() - started) / 1000).toFixed(0)} s ===`);
  process.exit(result.code === 0 ? 0 : 1);
}
