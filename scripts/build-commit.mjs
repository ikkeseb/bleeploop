// scripts/build-commit.mjs — the commit a bundle names in Help → About this build (vite.config.ts), and
// the answer the diagnostics probe expects. CI's GITHUB_SHA, else git's HEAD with "-dirty" when tracked
// files differ from it (the bundle then runs code no commit holds), else "unknown". A missing git never
// fails a build.
import { execFileSync } from 'node:child_process';

export function buildCommit() {
  const sha = process.env.GITHUB_SHA;
  if (sha) return sha.slice(0, 7);
  try {
    const head = execFileSync('git', ['rev-parse', '--short', 'HEAD'], { stdio: ['ignore', 'pipe', 'ignore'] }).toString().trim();
    if (!head) return 'unknown';
    const dirty = execFileSync('git', ['status', '--porcelain', '--untracked-files=no'], { stdio: ['ignore', 'pipe', 'ignore'] }).toString().trim();
    return dirty ? `${head}-dirty` : head;
  } catch {
    return 'unknown';
  }
}
