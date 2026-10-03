// scripts/release-changes.mjs — what git says changed since the last release, for the input-take rule
// (D26, scripts/release-input-rule.mjs). `pnpm release:smoke` calls it when its input take reads flat.
//
// The base is the nearest `v*` tag that is not this version's own (package.json): on a commit that
// already carries its tag, or after a fix that will move it, the release would otherwise be compared
// with itself and a device-side change in it would go unseen. Before the version bump that is the
// release before the last one, so the run belongs after the bump. There is no way to pick another base.
// Only the tag named `v<version>` counts as this version's own: a second tag for the same release under
// another name (an `-rc`, say) would be taken as the base. The repo has never tagged one.
//
// The comparison is the tree as it stands against that tag, and it only means something on a clean
// tree: git's diff does not see an untracked file, and an unstaged edit can cancel a staged one. So
// the answer says whether the tree is clean, and the rule requires the input take when it is not.
// Renames are reported as a removal and an addition (the old path is what leaves a device directory),
// and names come NUL-separated, so git quotes none of them.
//
// Guard: verify/guards/release-changes.mjs (throwaway repositories).

import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const MANIFEST = 'src-tauri/Cargo.toml';
const LOCK = 'src-tauri/Cargo.lock';

/**
 * @param {string} root the repository root (where package.json lives)
 * @returns {{
 *   base: string | null,
 *   clean?: boolean,
 *   dirty?: string[],
 *   paths: string[] | null,
 *   manifestBefore?: string, manifestAfter?: string,
 *   lockBefore?: string, lockAfter?: string,
 * }} `base` null: no release tag of another version. `paths` null: a git call or a read failed, which
 *   the rule reads as required. `dirty` holds `git status`'s entries (`XY path`) when the tree is not clean.
 */
export function releaseChanges(root) {
  const git = (...args) => execFileSync('git', args, { cwd: root, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'], maxBuffer: 64 * 1024 * 1024 });
  const nul = (out) => out.split('\0').filter(Boolean);
  let base = null;
  try {
    const { version } = JSON.parse(readFileSync(join(root, 'package.json'), 'utf8'));
    // Fails when no tag of another version reaches HEAD: the catch, with `base` still null.
    base = git('describe', '--tags', '--abbrev=0', '--match', 'v*', '--exclude', `v${version}`).trim() || null;
    if (!base) return { base, paths: null };
    // Untracked files asked for by name: a `status.showUntrackedFiles=no` in the config would hide them.
    const dirty = nul(git('status', '--porcelain', '-z', '--untracked-files=normal'));
    const paths = nul(git('diff', '--name-only', '--no-renames', '-z', base, '--'));
    return {
      base,
      clean: dirty.length === 0,
      dirty,
      paths,
      manifestBefore: git('show', `${base}:${MANIFEST}`),
      manifestAfter: readFileSync(join(root, MANIFEST), 'utf8'),
      lockBefore: git('show', `${base}:${LOCK}`),
      lockAfter: readFileSync(join(root, LOCK), 'utf8'),
    };
  } catch {
    return { base, paths: null };
  }
}
