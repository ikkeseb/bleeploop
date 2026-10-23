// scripts/pre-push.mjs — the pre-push hook: gate only refs that peel to a clean HEAD, so the checked tree is the one
// being pushed. Deletions need no gate. Git supplies ref updates on stdin; remote arguments are unused.

import { spawnSync } from 'node:child_process';
import { readFileSync, realpathSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

function pushedRefs(stdin) {
  return stdin.split(/\r?\n/).map((line) => line.trim()).filter(Boolean)
    .map((line) => line.split(/\s+/)).filter((fields) => !/^0+$/.test(fields[1]));
}

function refusal(reason) {
  return { action: 'refuse', message: `pre-push: ${reason}\nPush each ref from a clean checkout of its commit.` };
}

// The guard supplies HEAD, status and a peel function without starting git.
export function decidePush(stdin, head, peel, status) {
  const refs = pushedRefs(stdin);
  if (refs.length === 0) return { action: 'nothing', message: 'pre-push: nothing is pushed that needs the gate.' };
  for (const fields of refs) {
    const [ref, sha] = fields;
    if (fields.length !== 4 || !/^(?:[a-f0-9]{40}|[a-f0-9]{64})$/i.test(sha)) {
      return refusal(`invalid ref update: ${fields.join(' ')}`);
    }
    let commit;
    try {
      commit = peel(sha);
    } catch {
      return refusal(`${ref} (${sha}) could not be peeled to a commit.`);
    }
    if (!commit) return refusal(`${ref} (${sha}) could not be peeled to a commit.`);
    if (commit !== head) return refusal(`${ref} (${sha}) points to commit ${commit}, not HEAD ${head}.`);
  }
  const dirty = status.split(/\r?\n/).filter(Boolean);
  if (dirty.length) {
    const more = dirty.length > 5 ? `\n... ${dirty.length - 5} more status lines` : '';
    return refusal(`working tree or index is not clean:\n${dirty.slice(0, 5).join('\n')}${more}`);
  }
  return { action: 'gate' };
}

// Subprocess results are injectable for the guard; the installed hook always uses spawnSync.
export function runPrePush(stdin, run = spawnSync) {
  try {
    let decision;
    if (pushedRefs(stdin).length === 0) {
      decision = decidePush(stdin, '', () => '', '');
    } else {
      const git = (args) => {
        const result = run('git', args, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'] });
        if (result.error || result.status !== 0) throw new Error(`git ${args.join(' ')} failed`);
        return result.stdout;
      };
      const head = git(['rev-parse', 'HEAD']).trim();
      const status = git(['status', '--porcelain', '--untracked-files=all']);
      decision = decidePush(stdin, head, (sha) => git(['rev-parse', '--verify', `${sha}^{commit}`]).trim(), status);
    }
    if (decision.action !== 'gate') return { code: decision.action === 'nothing' ? 0 : 1, message: decision.message };
    // One command string through the shell: `.cmd` shims need it on Windows, and Node 24 warns
    // (DEP0190) when an argument list goes through one.
    const result = run('pnpm check', [], { stdio: 'inherit', shell: true });
    if (result.error) return { code: 1, message: `pre-push: could not start pnpm check: ${result.error.message}` };
    return { code: result.status ?? 1 };
  } catch (e) {
    return { code: 1, message: refusal(e.message).message };
  }
}

// realpath: import.meta.url is resolved through symlinks and junctions, argv[1] is not, and a
// mismatch here would skip the gate and pass the push.
if (process.argv[1] && import.meta.url === pathToFileURL(realpathSync(process.argv[1])).href) {
  try {
    const result = runPrePush(readFileSync(0, 'utf8'));
    if (result.message) (result.code === 0 ? console.log : console.error)(result.message);
    process.exitCode = result.code;
  } catch (e) {
    console.error(`pre-push: could not read ref updates: ${e.message}`);
    process.exitCode = 1;
  }
}
