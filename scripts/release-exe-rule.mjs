// scripts/release-exe-rule.mjs — whether `pnpm release:smoke` drives an exe built from the checkout's HEAD.
//
// The input-take rule reads the checkout, and the release is tagged on its commit. A stale exe must
// not pass for code it was not built from: Help's build line must name HEAD, without "-dirty" or
// "unknown", before the smoke goes on to the engine checks.
//
// Pure: the smoke hands in what the page and git answered; a missing answer fails the run.
// Guard: verify/guards/release-exe-rule.mjs.

/**
 * @param {object} build
 * @param {string | null} build.label Help's build line as the exe shows it (`BleepLoop <version> · <commit>`); null when it could not be read
 * @param {string | null} build.head `git rev-parse HEAD` in the checkout (40 hex); null when git gave no answer
 * @returns {{ ok: boolean, built: string | null, why: string }} `built`: the commit as the label names it, null when it names none
 */
export function exeBuildRule({ label, head }) {
  const match = typeof label === 'string' ? label.trim().match(/^BleepLoop \S+ · (\S+)$/) : null;
  if (!match) {
    return {
      ok: false,
      built: null,
      why: typeof label === 'string' ? `Help's build line ${JSON.stringify(label)} names no commit` : `Help's build line could not be read`,
    };
  }
  const built = match[1];
  if (built === 'unknown') {
    return { ok: false, built, why: 'the exe was built without git, so it names no commit: build again where git answers' };
  }
  if (built.endsWith('-dirty')) {
    return { ok: false, built, why: `the exe names ${built}, a build of a tree with uncommitted changes, which no commit holds: commit, then build again` };
  }
  if (!/^[0-9a-f]{7,40}$/.test(built)) {
    return { ok: false, built, why: `Help's build line names ${JSON.stringify(built)}, which is no commit (7 to 40 lowercase hex)` };
  }
  if (typeof head !== 'string' || !/^[0-9a-f]{40}$/.test(head)) {
    return { ok: false, built, why: `git gave no answer about the checkout's HEAD` };
  }
  if (!head.startsWith(built)) {
    return { ok: false, built, why: `the exe was built from ${built}, the checkout is at ${head.slice(0, built.length)}: check out the commit the exe was built from, or build this one` };
  }
  return { ok: true, built, why: `built from ${built}, the checkout's HEAD` };
}
