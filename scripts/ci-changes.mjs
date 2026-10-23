// scripts/ci-changes.mjs — ci.yml's changed-path classification (its `docs_guard` job's `changes` step).
// Fails OPEN: an unusable base (first push, force-push, all-zero `event.before`) or a git error
// yields an empty list, which classifies as NOT docs-only, so the heavy gates run. `--no-renames`
// lists both sides of a move, so a file moved into `docs/` still counts as a source change.
// `native` fails open the same way: false only for a readable diff with no file the `rust` and
// `engine` jobs read (src-tauri/, a cargo config or toolchain file above it, fixtures, the engine
// job's deny script, this classifier or its workflow), and never on a tag: a release's call runs
// every job. Partial stdout from a failed git command is never a readable diff.

import { spawnSync } from 'node:child_process';
import { appendFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

/** First match wins: documentation inside a native directory is still documentation. */
export function classifyFiles(files, refType = '') {
  let docs_only = files.length > 0;
  let native = files.length === 0 || refType === 'tag';
  for (const file of files) {
    if (file.endsWith('.md') || file.endsWith('.MD') || file.startsWith('docs/')) continue;
    docs_only = false;
    if (file.startsWith('src-tauri/') || file.startsWith('.cargo/') ||
        file.startsWith('rust-toolchain') || file.startsWith('verify/fixtures/') ||
        file === 'scripts/engine-deny.mjs' || file === 'scripts/ci-changes.mjs' ||
        file === '.github/workflows/ci.yml') native = true;
  }
  return { docs_only, native };
}

/** The runner returns spawnSync's status, error and stdout fields; tests inject git results. */
export function changedFiles(base, sha, git = (args) => spawnSync('git', args, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'], maxBuffer: 64 * 1024 * 1024 })) {
  if (!base || /^0+$/.test(base)) return [];
  // A failed spawnSync sets `error` (an overflowed buffer can still report status 0) or a non-zero
  // or null status (a signal).
  const succeeded = (result) => !result.error && result.status === 0;
  try {
    if (!succeeded(git(['cat-file', '-e', `${base}^{commit}`]))) return [];
    const diff = git(['diff', '--no-renames', '--name-only', base, sha]);
    if (!succeeded(diff)) return [];
    const stdout = diff.stdout.replace(/\n+$/, '');
    return stdout ? stdout.split('\n') : [];
  } catch {
    return [];
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const { CI_BASE_SHA: base = '', CI_SHA: sha, CI_REF_TYPE: refType } = process.env;
  const { docs_only, native } = classifyFiles(changedFiles(base, sha), refType);
  appendFileSync(process.env.GITHUB_OUTPUT, `docs_only=${docs_only}\nnative=${native}\n`);
  console.log(`base=${base} docs_only=${docs_only} native=${native}`);
}
