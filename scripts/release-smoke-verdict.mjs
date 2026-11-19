// scripts/release-smoke-verdict.mjs — did `pnpm release:smoke` pass on this exact commit? build-exe.yml's
// `smoke-verdict` job asks before a tag's `release` may publish (the rule: scripts/release-smoke-rule.mjs).
//
//   GH_TOKEN=… GH_REPO=owner/repo node scripts/release-smoke-verdict.mjs <sha>
//
// Prints the verdict; exit 0 only on a pass. An API error fails closed.

import { execFileSync } from 'node:child_process';
import { smokeVerdict, statusesOf } from './release-smoke-rule.mjs';

const sha = process.argv[2];
const repo = process.env.GH_REPO;
if (!/^[0-9a-f]{40}$/.test(sha ?? '') || !repo) {
  console.error('usage: GH_REPO=owner/repo node scripts/release-smoke-verdict.mjs <40-hex sha>');
  process.exit(1);
}

let statuses = null;
try {
  // --paginate --slurp: one JSON array of pages, however many statuses the commit holds.
  const pages = JSON.parse(
    execFileSync('gh', ['api', '--paginate', '--slurp', `repos/${repo}/commits/${sha}/statuses?per_page=100`], { encoding: 'utf8', maxBuffer: 16 * 1024 * 1024 }),
  );
  statuses = statusesOf(pages);
} catch (e) {
  console.error(`could not read the statuses: ${String(e instanceof Error ? e.message : e).split('\n')[0]}`);
}
const { ok, why } = smokeVerdict(statuses);
console.log(`release-smoke=${ok ? 'green' : ''}  (${why})`);
process.exit(ok ? 0 : 1);
