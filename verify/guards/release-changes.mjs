// verify/guards/release-changes.mjs — guard for scripts/release-changes.mjs, the git calls that feed
// `pnpm release:smoke`'s input-take rule (D26, scripts/release-input-rule.mjs).
//
// Runs the REAL collector on a throwaway repository under the OS temp directory (removed at the end),
// built commit by commit like a release history. Covers: with no tag, or only this version's own, the
// base is null; the base is the nearest `v*` tag that is not this version's own, with that tag on HEAD
// and on an ancestor; a file moved out of src-tauri/src/engine_io/ is reported at its old path; a
// tracked file with a non-ASCII name is reported with its real name; an untracked file, an unstaged
// edit and a staged edit undone in the tree each make the tree not clean, even with untracked files
// hidden in the config; a clean tree after the version bump reads clean; the manifest and the lock
// come back as they were at the base and as they are now; outside a repository the answer is the one
// the rule reads as required. Each answer is also put through the real rule.
// Independent of the developer's git config: the guard's git, and the collector's under it, read an
// empty global config and no system config.
// Cannot see: the developer's real repository, tags and config; a shallow clone.
// Run: node verify/guards/release-changes.mjs

import assert from 'node:assert';
import { execFileSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, renameSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { releaseChanges } from '../../scripts/release-changes.mjs';
import { inputTakeRule } from '../../scripts/release-input-rule.mjs';

let passed = 0;
let failed = 0;

function check(name, fn) {
  try {
    fn();
    passed++;
  } catch (e) {
    failed++;
    console.error(`FAIL: ${name}\n  ${e.message}`);
  }
}

const tmp = mkdtempSync(join(tmpdir(), 'bleeploop-release-changes-'));
const repo = join(tmp, 'repo');
writeFileSync(join(tmp, 'gitconfig'), '');
Object.assign(process.env, {
  GIT_CONFIG_GLOBAL: join(tmp, 'gitconfig'),
  GIT_CONFIG_NOSYSTEM: '1',
  GIT_AUTHOR_NAME: 'guard',
  GIT_AUTHOR_EMAIL: 'guard@example.invalid',
  GIT_COMMITTER_NAME: 'guard',
  GIT_COMMITTER_EMAIL: 'guard@example.invalid',
});
for (const key of ['GIT_DIR', 'GIT_WORK_TREE', 'GIT_INDEX_FILE']) delete process.env[key]; // a hook's own repository

const git = (...args) => execFileSync('git', args, { cwd: repo, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });
const write = (path, text) => {
  mkdirSync(dirname(join(repo, path)), { recursive: true });
  writeFileSync(join(repo, path), text);
};
const commit = (message) => {
  git('add', '-A');
  git('commit', '-q', '-m', message);
};
const manifest = (version) => `[package]\nname = "app"\nversion = "${version}"\n\n[dependencies]\ncpal = "=0.18.1"\n`;
const lockFile = (version) => `version = 4\n\n[[package]]\nname = "app"\nversion = "${version}"\n\n[[package]]\nname = "cpal"\nversion = "0.18.1"\n`;
const bump = (version) => {
  write('package.json', `${JSON.stringify({ name: 'app', version })}\n`);
  write('src-tauri/Cargo.toml', manifest(version));
  write('src-tauri/Cargo.lock', lockFile(version));
};
const DRIVER = 'src-tauri/src/engine_io/driver.rs';
const NAMED = 'src-tauri/src/host/løkke.rs';

try {
  check('outside a repository the answer has no base and no paths, and the rule requires the take', () => {
    const answer = releaseChanges(tmp);
    assert.deepStrictEqual(answer, { base: null, paths: null });
    assert.strictEqual(inputTakeRule(answer).required, true);
  });

  mkdirSync(repo);
  git('init', '-q', '-b', 'main');
  git('config', 'core.autocrlf', 'false');
  git('config', 'status.showUntrackedFiles', 'no'); // the collector asks for untracked files itself
  bump('0.1.0');
  write(DRIVER, '// the driver\n');
  write('README.md', 'one\n');
  commit('first');
  check('with no tag at all the base is null', () => assert.deepStrictEqual(releaseChanges(repo), { base: null, paths: null }));
  git('tag', '-a', '-m', 'v0.1.0', 'v0.1.0');
  check('with only this version\'s own tag the base is null, and the rule requires the take', () => {
    const answer = releaseChanges(repo);
    assert.deepStrictEqual(answer, { base: null, paths: null });
    assert.match(inputTakeRule(answer).why[0], /no release tag/);
  });

  // The next release: the version bump, the driver moved out of engine_io/, a host file with a non-ASCII name.
  bump('0.2.0');
  mkdirSync(join(repo, 'src-tauri/src/moved'), { recursive: true });
  renameSync(join(repo, DRIVER), join(repo, 'src-tauri/src/moved/driver.rs'));
  write(NAMED, '// a loop\n');
  commit('Release prep: v0.2.0');
  const bumped = releaseChanges(repo);
  check('after the version bump the base is the last release\'s tag', () => assert.strictEqual(bumped.base, 'v0.1.0'));
  check('a clean tree after the version bump reads clean', () => {
    assert.strictEqual(bumped.clean, true);
    assert.deepStrictEqual(bumped.dirty, []);
  });
  check('a file moved out of engine_io/ is reported at its old path', () => {
    assert.ok(bumped.paths.includes(DRIVER), `paths: ${JSON.stringify(bumped.paths)}`);
    assert.ok(bumped.paths.includes('src-tauri/src/moved/driver.rs'));
  });
  check('a tracked file with a non-ASCII name is reported with its real name', () =>
    assert.ok(bumped.paths.includes(NAMED), `paths: ${JSON.stringify(bumped.paths)}`));
  check('every changed path is listed, and nothing else', () =>
    assert.deepStrictEqual([...bumped.paths].sort(), ['package.json', 'src-tauri/Cargo.lock', 'src-tauri/Cargo.toml', DRIVER, NAMED, 'src-tauri/src/moved/driver.rs'].sort()));
  check('the manifest and the lock come back as at the base and as now', () => {
    assert.strictEqual(bumped.manifestBefore, manifest('0.1.0'));
    assert.strictEqual(bumped.manifestAfter, manifest('0.2.0'));
    assert.strictEqual(bumped.lockBefore, lockFile('0.1.0'));
    assert.strictEqual(bumped.lockAfter, lockFile('0.2.0'));
  });
  check('the rule names the moved file\'s old path and the host file, and nothing from the bump', () =>
    assert.deepStrictEqual(inputTakeRule(bumped), { required: true, why: [DRIVER, NAMED] }));

  git('tag', 'v0.2.0');
  check('with HEAD carrying this version\'s own tag the base is still the release before', () => assert.strictEqual(releaseChanges(repo).base, 'v0.1.0'));
  write('README.md', 'two\n');
  commit('a fix after the tag');
  const fixed = releaseChanges(repo);
  check('with this version\'s own tag on an ancestor the base is still the release before', () => {
    assert.strictEqual(fixed.base, 'v0.1.0');
    assert.strictEqual(fixed.clean, true);
  });

  // The release after: nothing on the device side, so a clean tree lets the synth take stand in.
  bump('0.3.0');
  commit('Release prep: v0.3.0');
  const next = releaseChanges(repo);
  check('the base is the nearest tag of another version, not an older one', () => assert.strictEqual(next.base, 'v0.2.0'));
  check('a release with nothing on the device side does not require the take', () => {
    assert.deepStrictEqual([...next.paths].sort(), ['README.md', 'package.json', 'src-tauri/Cargo.lock', 'src-tauri/Cargo.toml']);
    assert.deepStrictEqual(inputTakeRule(next), { required: false, why: [] });
  });

  write('src-tauri/src/engine_io/new.rs', '// not added\n');
  check('an untracked file makes the tree not clean, though no diff shows it', () => {
    const answer = releaseChanges(repo);
    assert.strictEqual(answer.clean, false);
    assert.deepStrictEqual(answer.dirty, ['?? src-tauri/src/engine_io/']);
    assert.ok(!answer.paths.some((p) => p.startsWith('src-tauri/src/engine_io/')));
    assert.match(inputTakeRule(answer).why.join(' '), /not clean.*commit first/);
  });
  rmSync(join(repo, 'src-tauri/src/engine_io'), { recursive: true });
  check('with the untracked file gone the tree reads clean again', () => assert.strictEqual(releaseChanges(repo).clean, true));

  write('README.md', 'three\n');
  check('an unstaged edit makes the tree not clean', () => {
    const answer = releaseChanges(repo);
    assert.strictEqual(answer.clean, false);
    assert.deepStrictEqual(answer.dirty, [' M README.md']);
  });
  git('add', 'README.md');
  write('README.md', 'two\n');
  check('a staged edit undone in the tree makes the tree not clean, though the diff reads as on the clean tree', () => {
    const answer = releaseChanges(repo);
    assert.strictEqual(answer.clean, false);
    assert.deepStrictEqual(answer.paths, next.paths);
    assert.deepStrictEqual(answer.dirty, ['MM README.md']);
    assert.strictEqual(inputTakeRule(answer).required, true);
  });
} finally {
  rmSync(tmp, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
}

console.log(`=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);
