// verify/guards/docs.mjs — docs drift guard: every backtick path in a tracked .md exists, every cited
// 7-hex commit sha resolves, the invariant titles in AGENTS.md and docs/ARCHITECTURE.md match by number,
// and the STATUS gate index has not grown past its cap.
//
// Until 2026-09 doc drift was measured by hand, once per audit (28 dangling refs found on 09-01, 20 of
// them the pre-split `plugin_host.rs`). This makes it a `pnpm check` gate. Paths are checked against the
// repo root, the citing file's directory, and as a relative tail of any tracked file; a path that exists
// nowhere AND is not a known runtime artifact (`logs/`, `dist/`, `session.json`, …) fails. Two tiers:
// the OWNER tier (the docs agents route by) is checked for bare filenames too; every other tracked
// .md only for slashed paths, since its bare names are often upstream crate files. A line
// may keep a deliberately dead path by saying so (`(now \`…\`)`, "not yet built", "upstream", "e.g.",
// … — see DELIBERATE). Shas: 7–10 hex WITH at least one letter (an all-digit run is a number, e.g. a
// VST3 param id). The stop cap flags a STATUS.md rig lap that has outgrown one sitting (the
// numbered items under "## The rig lap"): consolidate related stops or flag it to the owner (AGENTS.md).
// Raising RIG_LAP_STOP_CAP is the owner's call.
// Run: node verify/guards/docs.mjs
//
// Reads the tracked docs + git and asserts against the tree.

import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
const git = (...args) => execFileSync('git', args, { cwd: ROOT, encoding: 'utf8' });

/** Numbered stops under STATUS.md "## The rig lap" — the audit 2026-09-01 § 1 ceiling. */
const RIG_LAP_STOP_CAP = 10;

/** Runtime artifacts + placeholders that are legitimately cited but never tracked. */
const UNTRACKED_OK = [
  /^ringbuf\.js$/, // the npm package, not a file
  /^(feat|fix|chore)\//, // git branch names
  /^logs\//,
  /^dist\//,
  /^node_modules\//,
  /^\.playwright/,
  /^\.git\//,
  /^target\//,
  /^src-tauri\/target\//,
  /^session\.json$/,
  /^tauri\.conf\.json$/, // cited bare from src-tauri/AGENTS.md
  /\.exe$/,
  /\.log$/,
  /\.png$/, // gitignored shot folders
  /\.zip$/,
  /\.wav$/,
  /\.dll$/,
  /\.lib$/,
  /\.vst3$/,
  /\.clap$/,
];

const PATH_EXT = /\.(ts|tsx|mjs|js|rs|md|css|json|html|yml|yaml|toml|ps1|sh|txt)$/;
/** A line may cite a path that deliberately does not exist (renamed, never tracked, not built yet) —
 *  say so on the same line and the guard skips it. */
const DELIBERATE =
  /\b(now `|was `|untracked|never tracked|not yet built|not built|removed|gone|renamed|upstream|scratchpad|registry|e\.g\.)|\b[a-z][a-z-]*-\d+\.\d+\.\d+\b/i;
/** The docs agents route by — checked for bare filenames as well as slashed paths. */
const OWNER_TIER = new Set([
  'AGENTS.md',
  'STATUS.md',
  'README.md',
  'docs/ARCHITECTURE.md',
  'docs/VERIFY.md',
  'verify/README.md',
  'src-tauri/AGENTS.md',
  'src/audio/AGENTS.md',
  'src/ui/AGENTS.md',
]);

let checks = 0;
let fails = 0;
function check(ok, msg) {
  checks++;
  if (!ok) {
    fails++;
    console.error(`  FAIL  ${msg}`);
  }
}

const mdFiles = git('ls-files', '-z', '*.md', '**/*.md').split('\0').filter(Boolean);
const trackedSet = new Set(git('ls-files', '-z').split('\0').filter(Boolean));
const TOP_DIRS = new Set([...trackedSet].map((t) => t.split('/')[0]).filter((d) => !d.includes('.')));

// ── 1. backtick paths ──────────────────────────────────────────────────────────────────────
/** Reduce a backtick span to a candidate path, or null if it does not look like one. */
function candidatePath(span) {
  let s = span.trim();
  if (!s || /\s/.test(s)) return null; // commands, prose
  if (/[{}*$<>|()=,'"]/.test(s)) return null; // globs, code, templates
  if (/[…]|\.\.\./.test(s)) return null; // elided paths
  s = s.replace(/[:@#].*$/, ''); // `file.ts:123`, `file.ts:527/548`, `file.ts@10-20`, `doc.md#anchor`
  s = s.replace(/^\.\//, '').replace(/\/$/, '');
  if (!s || s.startsWith('http') || s.startsWith('-')) return null;
  const hasExt = PATH_EXT.test(s);
  const hasSlash = s.includes('/');
  // A slash alone is not a path (`note_on/off`, `A/B`, `cpal/asio`): require an extension, or a
  // leading tracked top-level directory.
  if (!hasExt && !(hasSlash && TOP_DIRS.has(s.split('/')[0]))) return null;
  if (!hasSlash && !/[A-Za-z]/.test(s.split('.')[0])) return null;
  return s;
}

function pathExists(p, fromDir) {
  if (existsSync(resolve(ROOT, p))) return true;
  if (existsSync(resolve(fromDir, p))) return true;
  return false;
}

const dangling = [];
for (const md of mdFiles) {
  const text = readFileSync(resolve(ROOT, md), 'utf8');
  const fromDir = dirname(resolve(ROOT, md));
  const seen = new Set();
  for (const m of text.matchAll(/`([^`\n]+)`/g)) {
    const p = candidatePath(m[1]);
    if (!p || seen.has(p)) continue;
    if (!p.includes('/') && !OWNER_TIER.has(md)) continue;
    seen.add(p);
    if (UNTRACKED_OK.some((re) => re.test(p))) continue;
    if (pathExists(p, fromDir)) continue;
    // A relative tail (`machine.ts`, `host/clap.rs`, `audio/plugin-bridge.ts`) is fine when a tracked
    // file ends with it.
    if ([...trackedSet].some((t) => t.endsWith('/' + p) || t === p)) continue;
    const lineNo = text.slice(0, m.index).split('\n').length;
    const lineText = text.split('\n')[lineNo - 1];
    if (DELIBERATE.test(lineText)) continue;
    dangling.push(`${md}:${lineNo}  \`${p}\`   ← ${lineText.trim().slice(0, 110)}`);
  }
}
for (const d of dangling) check(false, `dangling path ref  ${d}`);
check(true, 'path scan ran');

// ── 2. commit shas ─────────────────────────────────────────────────────────────────────────
// A shallow clone (CI's default fetch-depth 1) cannot resolve history, so the sha check is skipped
// there rather than failing on every citation; it runs on every full clone (dev machines, pre-push).
const shallow = git('rev-parse', '--is-shallow-repository').trim() === 'true';
const shaCites = new Map(); // sha -> first citation
for (const md of mdFiles) {
  const text = readFileSync(resolve(ROOT, md), 'utf8');
  for (const m of text.matchAll(/`([0-9a-f]{7,10})`/g)) {
    if (!/[a-f]/.test(m[1])) continue; // all digits = a number, not a sha
    if (!shaCites.has(m[1])) shaCites.set(m[1], `${md}:${text.slice(0, m.index).split('\n').length}`);
  }
}
if (shallow) {
  console.log('docs: shallow clone — commit-sha check skipped');
} else if (shaCites.size > 0) {
  const batch = execFileSync('git', ['cat-file', '--batch-check'], {
    cwd: ROOT,
    encoding: 'utf8',
    input: [...shaCites.keys()].join('\n') + '\n',
  });
  for (const line of batch.trim().split('\n')) {
    if (!line) continue;
    const [sha, type] = line.split(' ');
    const ok = type === 'commit';
    check(ok, `commit sha \`${sha}\` does not resolve (cited at ${shaCites.get(sha)})`);
  }
}

// ── 3. invariant titles: AGENTS.md and docs/ARCHITECTURE.md share one numbering ─────────────────────
// Source comments cite invariants by number. Titles may differ in detail, so the rule is: same count, and
// at each number one normalised title (backticks, bold, a trailing parenthetical and period dropped) is a
// prefix of the other. A reorder, an insertion or a reworded title fails.
/** The numbered items of the first `## ` section whose heading matches `heading`. */
function numbered(file, heading) {
  const text = readFileSync(resolve(ROOT, file), 'utf8');
  const at = text.search(heading);
  if (at === -1) return null;
  const next = text.indexOf('\n## ', at + 1);
  return text.slice(at, next === -1 ? undefined : next).split('\n').filter((l) => /^\d+\. /.test(l))
    .map((l) => l.replace(/^\d+\. /, ''));
}
const norm = (s) => s.replace(/[`*]/g, '').replace(/\s*\([^)]*\)\s*\.?$/, '').replace(/\.$/, '').trim().toLowerCase();
const agentsTitles = numbered('AGENTS.md', /^## Invariants/m);
const archTitles = numbered('docs/ARCHITECTURE.md', /^## Cross-cutting invariants/m)
  ?.map((l) => l.match(/^\*\*(.+?)\*\*/)?.[1] ?? l);
check(!!agentsTitles && !!archTitles, 'invariant lists not found (AGENTS.md "## Invariants", ARCHITECTURE "## Cross-cutting invariants")');
if (agentsTitles && archTitles) {
  check(agentsTitles.length === archTitles.length,
    `invariant count differs: AGENTS.md ${agentsTitles.length}, docs/ARCHITECTURE.md ${archTitles.length}`);
  for (let i = 0; i < Math.min(agentsTitles.length, archTitles.length); i++) {
    const a = norm(agentsTitles[i]);
    const b = norm(archTitles[i]);
    check(a.startsWith(b) || b.startsWith(a),
      `invariant ${i + 1} differs: AGENTS.md "${agentsTitles[i]}" vs docs/ARCHITECTURE.md "${archTitles[i]}"`);
  }
}

// ── 4. STATUS rig-lap stop cap ─────────────────────────────────────────────────────────────
const status = readFileSync(resolve(ROOT, 'STATUS.md'), 'utf8');
const start = status.indexOf('\n## The rig lap');
check(start !== -1, 'STATUS.md has no "## The rig lap" section');
const end = status.indexOf('\n## ', start + 1);
const section = status.slice(start, end === -1 ? undefined : end);
const stops = section.split('\n').filter((l) => /^\d+\. /.test(l));
check(
  stops.length <= RIG_LAP_STOP_CAP,
  `STATUS.md rig lap has ${stops.length} stops, cap is ${RIG_LAP_STOP_CAP} — consolidate related stops or flag it to the owner (AGENTS.md)`,
);

console.log(`docs: ${mdFiles.length} tracked .md, ${shaCites.size} shas, ${stops.length}/${RIG_LAP_STOP_CAP} rig-lap stops`);
console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails === 0 ? 0 : 1);
