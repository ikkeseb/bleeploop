// verify/guards/docs.mjs: docs drift guard. Every backtick path in a tracked .md exists, and so does
// every repo path a code comment cites (the briefings in `//!` and the headers live there), every cited
// 7-hex commit sha resolves, the invariant titles in AGENTS.md and docs/ARCHITECTURE.md match by number,
// the STATUS next-jam list has not grown past its cap, and no tracked .md holds an em dash.
//
// Until 2026-09 doc drift was measured by hand, once per audit (28 dangling refs found on 09-01, 20 of
// them the pre-split `plugin_host.rs`). This makes it a `pnpm check` gate. Paths are checked against the
// repo root, the citing file's directory, and as a relative tail of any tracked file; a path that exists
// nowhere AND is not a known runtime artifact (`logs/`, `dist/`, `session.json`, …) fails. Two tiers:
// the OWNER tier (the docs agents route by) is checked for bare filenames too; every other tracked
// .md only for slashed paths, since its bare names are often upstream crate files. A citation
// may keep a deliberately dead path by saying so in its own clause (the text between `;`, `:` or a
// sentence end; `(now \`…\`)`, "(removed)", "not yet built", "upstream", "e.g.", … — see DELIBERATE);
// a mark covers the one citation nearest it, so a historical reference does not hide a live one
// beside it. A code comment is read for paths from a top-level directory (`src/…`, `verify/…`,
// `.github/…`), bare or in backticks, resolved from the root or the file's own crate, and only real
// comments are read (a lexer skips string, template, char and regex literals). Shas: 7–10 hex WITH at least one letter (an all-digit run is a number, e.g. a
// VST3 param id). The cap keeps STATUS.md's next jam (the numbered items under "## Next jam") a list
// one ordinary jam can cover; the rest waits under "## Not heard yet". Raising NEXT_JAM_CAP is the
// owner's call.
// Run: node verify/guards/docs.mjs
//
// Reads the tracked docs + git and asserts against the tree.

import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
const git = (...args) => execFileSync('git', args, { cwd: ROOT, encoding: 'utf8' });

/** Numbered items under STATUS.md "## Next jam". */
const NEXT_JAM_CAP = 5;

/** Runtime artifacts + placeholders that are legitimately cited but never tracked. */
const UNTRACKED_OK = [
  /^ringbuf\.js$/, // the npm package, not a file
  /^(feat|fix|chore)\//, // git branch names
  /^logs\//,
  /^dist\//,
  /^node_modules\//,
  /^\.playwright/,
  /^\.git\//,
  /^(src-tauri\/)?target(\/|$)/, // the build dir, cited bare too (a candidate path loses its trailing slash)
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
/** A citation may name a path that deliberately does not exist (renamed, removed, never tracked, not
 *  built yet): say so in its clause and the guard skips it. */
const DELIBERATE =
  /\b(now `|was `|untracked|never tracked|not yet built|not built|removed|deleted|git history|gone|renamed|upstream|scratchpad|registry|e\.g\.)|\b[a-z][a-z-]*-\d+\.\d+\.\d+\b/i;
/** The docs agents route by — checked for bare filenames as well as slashed paths. */
const OWNER_TIER = new Set([
  'AGENTS.md',
  'STATUS.md',
  'README.md',
  'docs/ARCHITECTURE.md',
  'docs/VERIFY.md',
  'verify/README.md',
  'src-tauri/AGENTS.md',
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

// Tracked files as the working tree has them: a deletion not yet committed is gone already.
const onDisk = (f) => existsSync(resolve(ROOT, f));
const mdFiles = git('ls-files', '-z', '*.md', '**/*.md').split('\0').filter(Boolean).filter(onDisk);
const trackedSet = new Set(git('ls-files', '-z').split('\0').filter(Boolean).filter(onDisk));
const TOP_DIRS = new Set([...trackedSet].filter((t) => t.includes('/')).map((t) => t.split('/')[0]));

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

/** A cited path that is no artifact, not under the root or `fromDir`, and (unless `tail` is off) no
 *  relative tail (`machine.ts`, `host/clap.rs`, `audio/plugin-bridge.ts`) of a tracked file. */
function dead(p, fromDir, tail = true) {
  if (UNTRACKED_OK.some((re) => re.test(p))) return false;
  if (pathExists(p, fromDir)) return false;
  if (!tail) return true;
  return ![...trackedSet].some((t) => t.endsWith('/' + p) || t === p);
}

const dangling = [];
for (const md of mdFiles) {
  const fromDir = dirname(resolve(ROOT, md));
  const reported = new Set();
  readFileSync(resolve(ROOT, md), 'utf8').split('\n').forEach((lineText, i) => {
    const cites = [];
    for (const m of lineText.matchAll(/`([^`\n]+)`/g)) {
      const p = candidatePath(m[1]);
      if (p && (p.includes('/') || OWNER_TIER.has(md))) cites.push({ p, at: m.index, end: m.index + m[0].length });
    }
    for (const { p } of unmarked(lineText, cites)) {
      if (reported.has(p) || !dead(p, fromDir)) continue;
      reported.add(p);
      dangling.push(`${md}:${i + 1}  \`${p}\`   ← ${lineText.trim().slice(0, 110)}`);
    }
  });
}

// ── 1b. paths in code comments ────────────────────────────────────────────────────────────────────
/** The comment text of a JS/TS or Rust source, one `[line, text]` per source line a comment covers.
 *  String, template, char and regex literals are skipped, so a path in a string (a fixture's file
 *  name, a log line) is never read as a citation. */
function comments(src, rust) {
  const out = [];
  const n = src.length;
  let line = 1;
  let prev = ''; // the last code character, to tell a regex literal from a division
  let word = ''; // the identifier or keyword `prev` ends, if any (`return /x/` is a regex)
  const tpl = []; // per open template interpolation (`${`), its unclosed `{` count
  const emit = (text) => {
    for (const t of text.split('\n')) out.push([line++, t]);
    line--;
  };
  /** Index past a quoted literal opened at `i`; JS quotes end at the line, as the language does. */
  const quoted = (i, q) => {
    let j = i + 1;
    while (j < n && src[j] !== q) {
      if (src[j] === '\\') j++;
      else if (src[j] === '\n' && !rust) break;
      j++;
    }
    return j + 1;
  };
  /** Index past the template text from `i`: past its closing backtick, or past a `${`, which opens an
   *  interpolation (code, comments included) that a `}` at its own depth closes. */
  const template = (i) => {
    let j = i;
    while (j < n && src[j] !== '`' && !(src[j] === '$' && src[j + 1] === '{')) j += src[j] === '\\' ? 2 : 1;
    if (j < n && src[j] === '$') {
      tpl.push(0);
      return j + 2;
    }
    return j + 1;
  };
  const JS_BEFORE_REGEX = /^(return|typeof|instanceof|in|of|new|delete|void|throw|case|do|else|yield|await)$/;
  let i = 0;
  while (i < n) {
    const c = src[i];
    const d = src[i + 1];
    let end = -1;
    if (c === '/' && d === '/') {
      end = src.indexOf('\n', i);
      if (end === -1) end = n;
      emit(src.slice(i + 2, end));
      i = end;
      continue;
    }
    if (c === '/' && d === '*') {
      let depth = 1; // Rust block comments nest
      let j = i + 2;
      while (j < n && depth > 0) {
        if (src[j] === '*' && src[j + 1] === '/') depth--;
        else if (rust && src[j] === '/' && src[j + 1] === '*') depth++;
        else {
          j++;
          continue;
        }
        j += 2;
      }
      emit(src.slice(i + 2, Math.max(i + 2, j - 2)));
      i = j;
      continue;
    }
    if (rust && /[rbc]/.test(c) && !/[\w]/.test(src[i - 1] ?? '')) {
      const raw = /^[bc]?r(#*)"/.exec(src.slice(i, i + 300));
      if (raw) end = src.indexOf('"' + raw[1], i + raw[0].length) + 1 + raw[1].length;
    }
    if (end === -1 && !rust && c === '`') end = template(i + 1);
    if (end === -1 && !rust && c === '}' && tpl.length && tpl[tpl.length - 1] === 0) {
      tpl.pop();
      end = template(i + 1);
    }
    if (end === -1 && (c === '"' || (!rust && c === "'"))) end = quoted(i, c);
    if (end === -1 && rust && c === "'") {
      // A char literal ('x', '\n', '\u{2014}'), or a lifetime ('a), which has no closing quote.
      const cp = src.codePointAt(i + 1) ?? 0;
      if (src[i + 1] === '\\') end = src.indexOf("'", i + 3) + 1;
      else if (src[i + 1 + (cp > 0xffff ? 2 : 1)] === "'") end = i + 2 + (cp > 0xffff ? 2 : 1);
    }
    if (end === -1 && !rust && c === '/' && (!/[\w)\]$]/.test(prev) || JS_BEFORE_REGEX.test(word))) {
      let j = i + 1; // a regex literal: up to the closing slash outside a class
      let cls = false;
      while (j < n && src[j] !== '\n' && (cls || src[j] !== '/')) {
        if (src[j] === '\\') j++;
        else if (src[j] === '[') cls = true;
        else if (src[j] === ']') cls = false;
        j++;
      }
      end = j + 1;
    }
    if (end > i) {
      for (let k = i; k < end && k < n; k++) if (src[k] === '\n') line++;
      prev = src[end - 1] === '{' ? '{' : 'x'; // past a `${`, an expression starts
      word = '';
      i = end;
      continue;
    }
    if (tpl.length && c === '{') tpl[tpl.length - 1]++;
    if (tpl.length && c === '}') tpl[tpl.length - 1]--;
    if (c === '\n') line++;
    else if (!/\s/.test(c)) {
      word = /\w/.test(c) ? (/\w/.test(src[i - 1] ?? '') ? word + c : c) : '';
      prev = c;
    }
    i++;
  }
  return out;
}

/** The repo paths a comment line cites: a path from a top-level directory, bare or in backticks. A
 *  relative path (`looper/machine.ts`, Tone's `effect/Reverb.js`) is a port's history or an upstream
 *  file by the engine briefing's own convention (`src-tauri/crates/lf-engine/src/lib.rs`), so it is not
 *  read. */
function citedPaths(text) {
  const found = []; // every occurrence, so a marked one never hides an unmarked one
  const add = (raw, at, end) => {
    const p = candidatePath(raw);
    if (p && p.includes('/') && TOP_DIRS.has(p.split('/')[0])) found.push({ p, at, end });
  };
  for (const m of text.matchAll(/`([^`]+)`/g)) add(m[1], m.index, m.index + m[0].length);
  const bare = text.replace(/`[^`]+`/g, (span) => ' '.repeat(span.length)); // a span is read above
  for (const m of bare.matchAll(/(?<![\w./@-])\.?[a-z][\w-]*\/[\w./@-]*\w/gi)) add(m[0], m.index, m.index + m[0].length);
  return found.sort((a, b) => a.at - b.at);
}

/** The citations of one line (`{ p, at, end }`, `at`..`end` the span with its backticks, in text
 *  order) that no deliberate mark covers. A mark covers one citation: the nearest in its own clause,
 *  or for "(now `…`)" / "(was `…`)" the one before it (the old name). A citation's own name
 *  (a path ending in "gone.ts") is no mark. */
function unmarked(text, cites) {
  const chars = [...text];
  for (const c of cites) for (let k = c.at; k < c.end; k++) if (text[k] !== '`') chars[k] = ' ';
  const blank = chars.join('');
  const covered = new Set();
  for (const m of blank.matchAll(new RegExp(DELIBERATE.source, 'gi'))) {
    const from = m.index;
    const to = from + m[0].length;
    const [lo, hi] = clause(text, from);
    const near = cites.filter((c) => c.at >= lo && c.end <= hi);
    let pick;
    if (m[0].endsWith('`')) pick = near.filter((c) => c.end <= from).at(-1);
    else {
      const gap = (c) => Math.max(0, c.at - to, from - c.end);
      for (const c of near) if (!pick || gap(c) < gap(pick)) pick = c;
    }
    if (pick) covered.add(pick);
  }
  return cites.filter((c) => !covered.has(c));
}

/** The bounds `[from, to)` of the clause of `text` around `at`: cut at `;`, `:` or a sentence end. */
function clause(text, at) {
  const cut = /[;:]\s|(?<!\b(?:e\.g|i\.e))\.\s/g;
  let from = 0;
  let to = text.length;
  for (const m of text.matchAll(cut)) {
    if (m.index < at) from = m.index + 1;
    else {
      to = m.index;
      break;
    }
  }
  return [from, to];
}

// Dead citations in engine tests other work was changing when this scan landed (2026-10), one
// `file  path` each. Each wants its reference marked removed ("Ports the removed `…`", "the removed
// `…`") and its entry deleted here.
const LF = 'src-tauri/crates/lf-engine/';
const DEAD_OK = new Set(
  [
    // The removed Web Audio guards and probes the engine's tests port.
    'tests/first_take.rs  verify/guards/free-stop.mjs',
    'tests/golden_jam.rs  verify/probes/golden-jam.mjs',
    'tests/overdub_undo_reverse.rs  verify/guards/overdub.mjs',
    'tests/overdub_undo_reverse.rs  verify/probes/capture-loss.mjs',
    'tests/overdub_undo_reverse.rs  verify/probes/overdub-window.mjs',
    'tests/retake.rs  verify/guards/retake.mjs',
    // The rig the Web Audio guards ran on, removed in Stage 6.
    'tests/common/mod.rs  verify/harness/rig.ts',
  ].map((e) => LF + e),
);
const deadOkUsed = new Set();

const CODE = /\.(rs|ts|tsx|mjs)$/;
const codeFiles = [...trackedSet].filter((f) => CODE.test(f) && !f.startsWith('node_modules/'));
/** The directory of the crate a file belongs to (its nearest tracked Cargo.toml), else the root. A
 *  code comment's path resolves from the root or from its own crate (an lf-engine test cites the
 *  crate's grid.rs from the crate root), never as the tail of any file: a crate's lib.rs cited from the
 *  root package is dead there. */
function crateDir(file) {
  for (let dir = dirname(file); dir !== '.'; dir = dirname(dir)) {
    if (trackedSet.has(`${dir}/Cargo.toml`)) return resolve(ROOT, dir);
  }
  return ROOT;
}

for (const file of codeFiles) {
  const fromDir = crateDir(file);
  const reported = new Set();
  for (const [lineNo, text] of comments(readFileSync(resolve(ROOT, file), 'utf8'), file.endsWith('.rs'))) {
    for (const { p } of unmarked(text, citedPaths(text))) {
      if (reported.has(p) || !dead(p, fromDir, false)) continue;
      if (DEAD_OK.has(`${file}  ${p}`)) {
        deadOkUsed.add(`${file}  ${p}`);
        continue;
      }
      reported.add(p);
      dangling.push(`${file}:${lineNo}  \`${p}\`   ← ${text.trim().slice(0, 110)}`);
    }
  }
}

// The lexer and the clause rule, on planted text: a path in a string or regex is not a comment.
const plantedTs = "const a = '// src/gone.ts'; // src/live.ts\nconst r = /\\/\\/ src\\/x.ts/; /* `src/b.ts` */\nconst t = `${x} // src/t.ts`;";
check(
  JSON.stringify(comments(plantedTs, false)) === JSON.stringify([[1, ' src/live.ts'], [2, ' `src/b.ts` ']]),
  `the TS lexer read ${JSON.stringify(comments(plantedTs, false))}`,
);
const plantedRs = 'let s = r#"// src/raw.rs"#; let c = \'"\'; // src/live.rs\nfn f<\'a>() {} /* a /* nested */ src/n.rs */';
check(
  JSON.stringify(comments(plantedRs, true)) === JSON.stringify([[1, ' src/live.rs'], [2, ' a /* nested */ src/n.rs ']]),
  `the Rust lexer read ${JSON.stringify(comments(plantedRs, true))}`,
);
const plantedJs = 'function f() { return /["/]/; } // src/re.ts\nconst t = `a ${ /* src/i.ts */ {b: `${c}`}.b } // src/no.ts`; // src/yes.ts';
check(
  JSON.stringify(comments(plantedJs, false)) ===
    JSON.stringify([[1, ' src/re.ts'], [2, ' src/i.ts '], [2, ' src/yes.ts']]),
  `the TS lexer, on a regex after a keyword and template interpolations, read ${JSON.stringify(comments(plantedJs, false))}`,
);
/** The citations of a planted comment no mark covers. */
const open = (text) => unmarked(text, citedPaths(text)).map((c) => c.p).join(' ');
for (const [text, want, why] of [
  ['`src/a.ts` (removed); `src/b.ts` holds it; e.g. `src/c.ts`', 'src/b.ts', 'a mark reaches past its own clause, or stops at an "e.g."'],
  ['`src/old.ts` (removed), now implemented in `src/new.ts`', 'src/new.ts', 'a mark covers a second path in its clause'],
  ['`src/old.ts` (now `src/new.ts`)', 'src/new.ts', 'a "(now `…`)" covers the new name'],
  ['src/x.ts (removed) was the first; src/x.ts is the second', 'src/x.ts', 'a marked citation hides an unmarked one of the same path'],
  ['see `src/gone.ts`', 'src/gone.ts', "a path's own name counted as its mark"],
  ['see .github/workflows/x.yml', '.github/workflows/x.yml', 'a hidden top-level directory is not read'],
]) {
  check(open(text) === want, `${why}: "${text}" left "${open(text)}" unmarked, want "${want}"`);
}
check(
  dead('src/lib.rs', crateDir('src/app.tsx'), false) && !dead('src/grid.rs', crateDir(`${LF}tests/retake.rs`), false),
  'a code comment resolves a path other than from the root or its own crate',
);

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

// ── 4. STATUS next-jam cap ──────────────────────────────────────────────────────────────────
const status = readFileSync(resolve(ROOT, 'STATUS.md'), 'utf8');
const start = status.indexOf('\n## Next jam');
check(start !== -1, 'STATUS.md has no "## Next jam" section');
const end = status.indexOf('\n## ', start + 1);
const section = status.slice(start, end === -1 ? undefined : end);
const jam = section.split('\n').filter((l) => /^\d+\. /.test(l));
check(
  jam.length <= NEXT_JAM_CAP,
  `STATUS.md's next jam has ${jam.length} items, cap is ${NEXT_JAM_CAP}: move the rest under "## Not heard yet" (AGENTS.md)`,
);

// ── 5. no em dashes in tracked docs ─────────────────────────────────────────────────────────
// An em dash reads as machine-written prose; a period, a comma or a colon does the same job.
const EM_DASH = '\u2014';
for (const md of mdFiles) {
  const lines = readFileSync(resolve(ROOT, md), 'utf8').split('\n');
  const hits = lines.flatMap((l, i) => (l.includes(EM_DASH) ? [i + 1] : []));
  check(hits.length === 0, `${md}:${hits.join(',')} has an em dash: write a period, a comma or a colon`);
}

console.log(`docs: ${mdFiles.length} tracked .md, ${codeFiles.length} code files, ${shaCites.size} shas, ${jam.length}/${NEXT_JAM_CAP} next-jam items`);
const stale = [...DEAD_OK].filter((e) => !deadOkUsed.has(e));
if (stale.length) console.log(`docs: DEAD_OK entries no longer needed (delete them):\n  ${stale.join('\n  ')}`);
console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
process.exit(fails === 0 ? 0 : 1);
