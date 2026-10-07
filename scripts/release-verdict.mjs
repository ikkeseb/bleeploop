// scripts/release-verdict.mjs — has this commit's own push to main already passed `ci` and `rust-test`?
// build-exe.yml asks before a release: a green verdict on the very commit lets the tag's run skip the
// same two workflows (about 20 min of the wait). It waits for a run that is still going, and anything it
// cannot read as green (no run, a failed or cancelled one, a job that was skipped, an API error, the
// deadline) answers "no", so the tag's run then runs that workflow itself.
//
//   GH_TOKEN=… GH_REPO=owner/repo node scripts/release-verdict.mjs <sha> [--wait-min=35]
//
// Prints `ci=green|` and `rust_test=green|` (and appends them to $GITHUB_OUTPUT when set). Exit 0 always.

import { execFileSync } from 'node:child_process';
import { appendFileSync } from 'node:fs';

// Each workflow's jobs that must have RUN green: a job the diff classifier skipped is no verdict.
const WORKFLOWS = [
  { key: 'ci', file: 'ci.yml', jobs: ['docs-guard', 'js', 'rust', 'engine'] },
  { key: 'rust_test', file: 'rust-test.yml', jobs: ['test'] },
];
const POLL_MS = 30_000;

const sha = process.argv[2];
const waitMin = Number(process.argv.find((a) => a.startsWith('--wait-min='))?.split('=')[1] ?? 35);
const repo = process.env.GH_REPO;
if (!/^[0-9a-f]{40}$/.test(sha ?? '') || !repo || !(waitMin >= 0)) {
  console.error('usage: GH_REPO=owner/repo node scripts/release-verdict.mjs <40-hex sha> [--wait-min=N]');
  process.exit(1);
}
const deadline = Date.now() + waitMin * 60_000;

const api = (path) => JSON.parse(execFileSync('gh', ['api', path], { encoding: 'utf8', maxBuffer: 16 * 1024 * 1024 }));
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** The newest run of `file` that main's push of `sha` started, or undefined. */
function pushRun(file) {
  const { workflow_runs: runs } = api(`repos/${repo}/actions/workflows/${file}/runs?head_sha=${sha}&event=push&branch=main&per_page=20`);
  return runs.filter((r) => r.head_sha === sha && r.event === 'push' && r.head_branch === 'main').sort((a, b) => b.id - a.id)[0];
}

async function verdict({ key, file, jobs }) {
  try {
    let run = pushRun(file);
    while (run && run.status !== 'completed' && Date.now() < deadline) {
      console.log(`${key}: run ${run.id} is ${run.status}, waiting`);
      await sleep(POLL_MS);
      run = pushRun(file);
    }
    if (!run) return [false, 'no run of this commit on main'];
    if (run.status !== 'completed') return [false, `run ${run.id} still ${run.status} at the deadline`];
    if (run.conclusion !== 'success') return [false, `run ${run.id} ended ${run.conclusion}`];
    const latest = api(`repos/${repo}/actions/runs/${run.id}/jobs?filter=latest&per_page=100`).jobs;
    for (const name of jobs) {
      const job = latest.find((j) => j.name === name);
      if (job?.conclusion !== 'success') return [false, `run ${run.id}: job ${name} is ${job?.conclusion ?? 'missing'}`];
    }
    return [true, `run ${run.id}: ${jobs.join(', ')} green`];
  } catch (e) {
    return [false, `could not read it: ${String(e instanceof Error ? e.message : e).split('\n')[0]}`];
  }
}

for (const workflow of WORKFLOWS) {
  const [green, why] = await verdict(workflow);
  const line = `${workflow.key}=${green ? 'green' : ''}`;
  console.log(`${line}  (${why})`);
  if (process.env.GITHUB_OUTPUT) appendFileSync(process.env.GITHUB_OUTPUT, `${line}\n`);
}
