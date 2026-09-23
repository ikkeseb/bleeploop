// fs-mirror-drift-verify.mjs — the PORT-DRIFT CANARY (a meta-guard over the other guards).
//
// Most guards in verify/ IMPORT the real source (Node TS type-stripping) and so cannot drift. A handful
// cannot be imported under Node (they touch Web Audio / Tone / AudioWorklet / solid-js at module load) and
// instead PORT the pure math by hand. A hand port can go silently stale: the source function it mirrors is
// refactored, the port keeps asserting the OLD math, and the guard stays green while proving nothing. This
// meta-guard catches exactly that — "source moved, the port didn't".
//
// HOW IT WORKS
//   A ported guard carries, next to its ported math, a tag naming the source range it mirrors:
//       // MIRRORS: src/audio/clock.ts@348-382 sha256:1a2b3c4d5e6f7a8b
//   This meta-guard reads every *-verify.mjs, parses those tags, and re-hashes the referenced source
//   line range. If the recorded hash no longer matches, the port's source moved or changed and the guard
//   FAILS with an actionable message.
//
// WHAT IS HASHED (deliberately noise-tolerant)
//   The NORMALIZED CODE of the source line range (1-indexed, inclusive): each line trimmed, then blank
//   lines and FULL-LINE comments (`//`, `/*`, `*`, `*/`) dropped, joined with '\n', sha256, first 16 hex.
//   => Editing a COMMENT BLOCK inside the range never trips the canary (the core's comment archaeology is
//      churned constantly); changing the CODE does. An inline trailing `// ...` on a code line is kept, so
//      a code-line's trailing-comment-only edit can trip it — harmless, `--update` re-baselines in one step.
//
// ON FAILURE the message distinguishes two cases so the fixer knows what to do:
//   • MOVED  — the exact same code now lives at a different line range (e.g. an unrelated edit above shifted
//              it). Benign: just re-point + re-baseline with `node verify/fs-mirror-drift-verify.mjs --update`.
//   • CHANGED — the code in the range is genuinely different. Re-read the source, confirm the PORT still
//              mirrors it faithfully (fix the port if not), THEN `--update` to record the new hash.
//
// RE-BASELINE / SEED HASHES
//   `node verify/fs-mirror-drift-verify.mjs --update` rewrites each tag's `sha256:` to the current source
//   hash (seeding `sha256:PENDING` tags on first use), and auto-repoints a tag's line range when the same
//   code is found MOVED. Run it after you have re-verified the affected port by hand.
//
// This script is itself a `*-verify.mjs`, so `pnpm verify` (and thus `pnpm check` + CI) runs it in check
// mode. See verify/README.md ("Mirrored ports and the drift canary").

import { readFileSync, writeFileSync, readdirSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { dirname, join, resolve } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, '..');
const UPDATE = process.argv.includes('--update');

// // MIRRORS: <relative src path>@<startLine>-<endLine> sha256:<hex|PENDING>
const TAG_RE = /MIRRORS:\s*([^\s@]+)@(\d+)-(\d+)\s+sha256:([0-9a-fA-F]+|PENDING)/;

// Precompute, per line (0-indexed), whether the line STARTS inside an open /* … */ block comment.
// A pragmatic single-pass scanner (this is a canary, not a parser): it tracks block-comment state,
// skips `//` line comments to EOL, and skips string literals so a `/*` inside a string doesn't open a
// block. Strings are assumed not to span lines (reset at EOL) — good enough for the source we mirror.
function computeBlockStarts(lines) {
  const starts = new Array(lines.length);
  let inBlock = false;
  for (let i = 0; i < lines.length; i++) {
    starts[i] = inBlock;
    const line = lines[i];
    let inStr = null; // quote char while inside a string literal
    let j = 0;
    while (j < line.length) {
      const c = line[j];
      const c2 = line[j + 1];
      if (inBlock) {
        if (c === '*' && c2 === '/') { inBlock = false; j += 2; continue; }
        j++; continue;
      }
      if (inStr) {
        if (c === '\\') { j += 2; continue; }
        if (c === inStr) { inStr = null; }
        j++; continue;
      }
      if (c === '/' && c2 === '*') { inBlock = true; j += 2; continue; }
      if (c === '/' && c2 === '/') break; // line comment — rest of line is irrelevant to block state
      if (c === "'" || c === '"' || c === '`') { inStr = c; j++; continue; }
      j++;
    }
    // Pragmatic: non-block string state does not carry across lines (block state does).
  }
  return starts;
}

const srcCache = new Map();
function srcInfo(relPath) {
  if (!srcCache.has(relPath)) {
    const lines = readFileSync(join(repoRoot, relPath), 'utf8').split(/\r?\n/);
    srcCache.set(relPath, { lines, blockStarts: computeBlockStarts(lines) });
  }
  return srcCache.get(relPath);
}

// Normalize a 1-indexed inclusive line range to code-only text (blank + full-line comments dropped).
// Block-comment-aware: a line starting with '*' (or '*/') is a comment-continuation ONLY when it begins
// INSIDE an open block comment; a genuine CODE line that happens to start with '*' (e.g. a wrapped
// multiplication `* b`) is KEPT so a real code change can't hide behind a leading '*'. All other rules are
// byte-identical to the pre-block-aware version (trim; drop blank; drop full-line '//'; drop lines starting
// with '/*'), so existing hashes are unchanged wherever this blind spot was never exercised.
function normalize(info, start, end) {
  const { lines, blockStarts } = info;
  const out = [];
  for (let i = start - 1; i < end; i++) {
    const raw = lines[i];
    if (raw === undefined) continue;
    const l = raw.trim();
    if (l.length === 0) continue;
    if (l.startsWith('//')) continue;
    if (l.startsWith('/*')) continue;
    if (l.startsWith('*')) {
      if (blockStarts[i]) continue; // '*'/'*/' continuation inside an open block comment → drop
      // else: real code line beginning with '*' → keep it (this is the weakness-1 fix)
    }
    out.push(l);
  }
  return out.join('\n');
}
function hashOf(info, start, end) {
  return createHash('sha256').update(normalize(info, start, end)).digest('hex').slice(0, 16);
}

// Find where a block of `span` source lines normalizes to `wantHash` (used to detect a benign MOVE).
function locate(info, span, wantHash) {
  const last = info.lines.length - span + 1;
  for (let s = 1; s <= last; s++) {
    if (hashOf(info, s, s + span - 1) === wantHash) return { start: s, end: s + span - 1 };
  }
  return null;
}

// ── Collect every MIRRORS tag across the guards ────────────────────────────────────────────────────
const guards = readdirSync(here).filter((f) => f.endsWith('-verify.mjs') && f !== 'fs-mirror-drift-verify.mjs');
const tags = [];
for (const f of guards) {
  const abs = join(here, f);
  const text = readFileSync(abs, 'utf8');
  const guardLines = text.split(/\r?\n/);
  for (let i = 0; i < guardLines.length; i++) {
    const m = guardLines[i].match(TAG_RE);
    if (m) tags.push({ file: f, abs, lineNo: i + 1, src: m[1], start: Number(m[2]), end: Number(m[3]), recorded: m[4] });
  }
}

// ── UPDATE mode: seed / re-baseline hashes (and auto-repoint a MOVED range) ─────────────────────────
if (UPDATE) {
  const edits = new Map(); // abs -> text (accumulate rewrites per guard file)
  let changed = 0;
  let failed = 0;
  for (const t of tags) {
    const info = srcInfo(t.src);
    let { start, end } = t;
    let cur = hashOf(info, start, end);
    // If the recorded hash was real and no longer matches here, try to find the block MOVED elsewhere and
    // re-point the range to it (so --update fixes a stale pointer, not just the hash).
    if (t.recorded !== 'PENDING' && t.recorded.toLowerCase() !== cur) {
      const moved = locate(info, end - start + 1, t.recorded.toLowerCase());
      if (moved) {
        start = moved.start;
        end = moved.end;
        cur = t.recorded.toLowerCase();
      } else {
        // CHANGED (+ possibly MOVED): the old code exists NOWHERE, so auto-repoint is impossible and
        // re-hashing @start-end could baseline unrelated code if the mirrored function also moved. Refuse
        // to write this tag. The human must re-point it by hand, then run --update again.
        console.log(
          `  FAIL CHANGED ${t.file}:${t.lineNo}  ${t.src}@${start}-${end} — the old code is not found ` +
            'ANYWHERE in the source. Tag NOT updated; re-read the port and re-point the range by hand.',
        );
        failed++;
        continue;
      }
    }
    const oldTag = `MIRRORS: ${t.src}@${t.start}-${t.end} sha256:${t.recorded}`;
    const newTag = `MIRRORS: ${t.src}@${start}-${end} sha256:${cur}`;
    if (oldTag !== newTag) {
      const text = edits.get(t.abs) ?? readFileSync(t.abs, 'utf8');
      edits.set(t.abs, text.replace(oldTag, newTag));
      changed++;
      console.log(`  update ${t.file}:${t.lineNo}  ${t.src}@${start}-${end} -> sha256:${cur}`);
    }
  }
  for (const [abs, text] of edits) writeFileSync(abs, text);
  console.log(
    `\n=== RESULT: ${tags.length - failed}/${tags.length} checks passed, ${failed} failed ===  ` +
      `(updated ${changed} tag${changed === 1 ? '' : 's'})`,
  );
  process.exit(failed === 0 ? 0 : 1);
}

// ── CHECK mode (the default; what pnpm verify runs) ─────────────────────────────────────────────────
let passed = 0;
let failed = 0;
console.log(`\nfs-mirror-drift: ${tags.length} MIRRORS tag(s) across ${guards.length} guard(s)`);
for (const t of tags) {
  const info = srcInfo(t.src);
  if (t.recorded === 'PENDING') {
    failed++;
    console.log(`  FAIL  ${t.file}:${t.lineNo}  ${t.src}@${t.start}-${t.end} is sha256:PENDING`);
    console.log(`        → seed it: node verify/fs-mirror-drift-verify.mjs --update`);
    continue;
  }
  const cur = hashOf(info, t.start, t.end);
  if (cur === t.recorded.toLowerCase()) {
    passed++;
    continue;
  }
  failed++;
  const moved = locate(info, t.end - t.start + 1, t.recorded.toLowerCase());
  console.log(`  FAIL  ${t.file}:${t.lineNo}  MIRRORS ${t.src}@${t.start}-${t.end}`);
  if (moved) {
    console.log(`        MOVED — the same code now sits at @${moved.start}-${moved.end} (benign shift).`);
    console.log(`        → re-point + re-baseline: node verify/fs-mirror-drift-verify.mjs --update`);
  } else {
    console.log(`        CHANGED — the source code in that range is different (recorded ${t.recorded}, now ${cur}).`);
    console.log(`        → re-read ${t.src}, confirm the port in ${t.file} still mirrors it (fix the port if not),`);
    console.log(`          then re-baseline: node verify/fs-mirror-drift-verify.mjs --update`);
  }
}

// ── COVERAGE: every guard must be DRIFT-PROTECTED by one of three mechanisms ─────────────────────────
// A guard that neither imports real source nor carries a MIRRORS tag gets ZERO drift protection and nothing
// would notice. Assert each *-verify.mjs (except this meta-guard) is covered by (a) a real '../src/…' import
// (imports can't drift), (b) ≥1 MIRRORS tag, or (c) an explicit `// MIRRORS-EXEMPT: <reason>` for guards that
// genuinely port nothing (pure spec constants / self-contained format checks).
// './harness/rig.ts' loads the real src/ modules, so a rig guard counts as an import.
const SRC_IMPORT_RE = /['"](?:\.\.\/src\/|\.\/harness\/rig\.ts['"])/;
const EXEMPT_RE = /MIRRORS-EXEMPT:/;
const tagFiles = new Set(tags.map((t) => t.file));
console.log(`\nfs-mirror-drift: coverage over ${guards.length} guard(s)`);
for (const f of guards) {
  const text = readFileSync(join(here, f), 'utf8');
  if (SRC_IMPORT_RE.test(text) || tagFiles.has(f) || EXEMPT_RE.test(text)) {
    passed++;
    continue;
  }
  failed++;
  console.log(`  FAIL  ${f}  is drift-UNPROTECTED (no '../src/…' import, no MIRRORS tag, no exemption).`);
  console.log(`        Give it exactly ONE of:`);
  console.log(`          (a) import the real source via a '../src/…' path (an import can't drift), OR`);
  console.log(`          (b) a  // MIRRORS: src/<path>@<start>-<end> sha256:PENDING  tag beside the ported math, then --update, OR`);
  console.log(`          (c) a  // MIRRORS-EXEMPT: <reason>  line if it genuinely ports nothing (spec constants / self-contained format checks).`);
}

console.log(`\n=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);
