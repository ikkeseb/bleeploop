// verify/guards/third-party.mjs: THIRD-PARTY-NOTICES.md (installed beside the exe) lists every direct
// runtime dependency of what ships, at the version the lockfile resolves, and nothing else. Shipped:
// package.json `dependencies` (pnpm-lock.yaml), and the `[dependencies]` and
// `[target.'…'.dependencies]` tables (and their `.<name>` subtables) of src-tauri/Cargo.toml and the
// lf-engine crate it links (Cargo.lock: the version that crate's own lock entry depends on). Dev and
// build dependencies do not ship; a `path` dependency (lf-engine) is first-party, so its own external
// dependencies count instead. Transitive dependencies are not listed in the notices, so they are not
// checked. midir went missing for a release (v0.8.4) before this guard.
//
// Each reader is first run on planted texts, then on the repo. Run: node verify/guards/third-party.mjs

import assert from 'node:assert';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const read = (f) => readFileSync(join(root, f), 'utf8').replace(/\r\n/g, '\n');

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

/** Direct runtime dependency names of a Cargo.toml, path (first-party) dependencies left out. A
 *  dependency is declared as a key of a dependencies table (`serde = …`, `serde.version = …`) or as a
 *  table of its own (`[dependencies.serde]`, `[target.'…'.dependencies.serde]`). */
function cargoRuntimeDeps(toml) {
  const deps = new Map(); // name -> whether it is a path dependency
  let runtime = false;
  let own = null; // the dependency a `[…dependencies.<name>]` table declares
  let current = null; // the dependency whose value is still open (a multi-line inline table)
  for (const line of toml.split('\n')) {
    const header = line.match(/^\[([^\]]+)\]\s*(#.*)?$/);
    if (header) {
      const table = header[1].trim().match(/^(?:target\.(?:'[^']*'|"[^"]*"|[^.]+)\.)?dependencies(?:\.(.+))?$/);
      runtime = !!table;
      own = table?.[1]?.replace(/^["']|["']$/g, '') ?? null;
      if (own && !deps.has(own)) deps.set(own, false);
      current = null;
      continue;
    }
    if (!runtime) continue;
    const isPath = /\bpath\s*=/.test(line.replace(/#.*$/, ''));
    if (own) {
      if (isPath) deps.set(own, true);
      continue;
    }
    const entry = line.match(/^([A-Za-z0-9_-]+)(\.[A-Za-z0-9_.-]+)?\s*=/);
    if (entry) {
      current = entry[1];
      if (!deps.has(current)) deps.set(current, false);
    }
    if (current && isPath) deps.set(current, true);
  }
  return [...deps].filter(([, path]) => !path).map(([name]) => name);
}

/** name → resolved version for the direct dependencies of one package in a Cargo.lock. */
function cargoLockDeps(lock, pkg) {
  const packages = lock.split(/\n(?=\[\[package\]\])/).map((block) => ({
    name: block.match(/^name = "([^"]+)"/m)?.[1],
    version: block.match(/^version = "([^"]+)"/m)?.[1],
    deps: [...(block.match(/^dependencies = \[\n([\s\S]*?)^\]/m)?.[1] ?? '').matchAll(/"([^"]+)"/g)].map((m) => m[1]),
  }));
  const own = packages.find((p) => p.name === pkg);
  assert.ok(own, `Cargo.lock has no package "${pkg}"`);
  const resolved = new Map();
  for (const dep of own.deps) {
    // An entry carries its version only when the lock holds more than one of that crate.
    const [name, version] = dep.split(' ');
    resolved.set(name, version ?? packages.find((p) => p.name === name)?.version);
  }
  return resolved;
}

/** name → resolved version for the root importer's `dependencies` in a pnpm-lock.yaml (v9). */
function pnpmLockDeps(lock) {
  const block = lock.match(/^importers:\n\n {2}\.:\n {4}dependencies:\n([\s\S]*?)(?=^ {4}\S|^\S)/m)?.[1] ?? '';
  const resolved = new Map();
  for (const m of block.matchAll(/^ {6}'?([^':\n]+)'?:\n {8}specifier: .*\n {8}version: ([^\s(]+)/gm)) {
    resolved.set(m[1], m[2]);
  }
  return resolved;
}

/** name → version of the rows in the table under a `## ` heading of THIRD-PARTY-NOTICES.md. */
function noticeRows(md, heading) {
  const at = md.indexOf(`\n## ${heading}`);
  assert.ok(at !== -1, `THIRD-PARTY-NOTICES.md has no "## ${heading}"`);
  const next = md.indexOf('\n## ', at + 1);
  const rows = new Map();
  for (const line of md.slice(at, next === -1 ? undefined : next).split('\n')) {
    const cells = line.match(/^\|([^|]+)\|([^|]+)\|/);
    if (!cells) continue;
    const name = cells[1].trim().replace(/^\[([^\]]+)\]\(.*\)$/, '$1');
    const version = cells[2].trim();
    if (!/^\d/.test(version)) continue; // the header and the separator row
    rows.set(name, version);
  }
  return rows;
}

/** The differences between what ships (name → version) and the notices' rows, one line each. */
function compare(shipped, rows) {
  const out = [];
  for (const [name, version] of shipped) {
    if (!rows.has(name)) out.push(`${name} ${version} ships but has no row`);
    else if (rows.get(name) !== version) out.push(`${name}: the row says ${rows.get(name)}, the lock resolves ${version}`);
  }
  for (const name of rows.keys()) if (!shipped.has(name)) out.push(`${name} has a row but is not a direct runtime dependency`);
  return out;
}

// ── 1. The readers, on planted texts ───────────────────────────────────────────────────────────────
const TOML = `[package]
name = "app"

[build-dependencies]
tauri-build = "2"

[dependencies]
serde = { version = "1.0", features = ["derive"] }
first-party = { path = "crates/first-party" }
log.version = "0.4"
log.features = ["std"]
local.path = "crates/local"

[target.'cfg(windows)'.dependencies]
windows = { version = "0.61", features = [
    "Win32_Foundation",
] }
asio-sys = { version = "=0.3.0", optional = true }

[target.'cfg(windows)'.dev-dependencies]
clap-sys = "=0.5.0"

[target.'cfg(windows)'.dependencies.sha2]
version = "0.10"

[dependencies.other-first-party]
path = "crates/other"

[dev-dependencies.proptest]
version = "1"

[profile.dev.package."*"]
opt-level = 3
`;
const LOCK = `version = 4

[[package]]
name = "app"
version = "1.0.0"
dependencies = [
 "serde",
 "windows 0.61.3",
]

[[package]]
name = "serde"
version = "1.0.229"

[[package]]
name = "windows"
version = "0.58.0"

[[package]]
name = "windows"
version = "0.61.3"
`;
const PNPM = `lockfileVersion: '9.0'

importers:

  .:
    dependencies:
      '@tauri-apps/api':
        specifier: ^2.11.1
        version: 2.11.1
      solid-js:
        specifier: ^1.9.15
        version: 1.9.15
    devDependencies:
      vite:
        specifier: ^8.3.0
        version: 8.3.0(@types/node@25.9.3)
`;
const NOTICES = `# Notices

## JavaScript / frontend dependencies

| Package | Version | License |
|---|---|---|
| [solid-js](https://example.com) | 1.9.15 | MIT |

## Rust / native dependencies

| Crate | Version | License |
|---|---|---|
| serde | 1.0.229 | MIT OR Apache-2.0 |
`;
check('runtime tables and subtables only: no build, dev, profile or path dependency', () =>
  assert.deepStrictEqual(cargoRuntimeDeps(TOML), ['serde', 'log', 'windows', 'asio-sys', 'sha2']));
check('a lock entry with a version picks that version among several', () =>
  assert.deepStrictEqual([...cargoLockDeps(LOCK, 'app')], [['serde', '1.0.229'], ['windows', '0.61.3']]));
check('pnpm: runtime dependencies only, scoped names read', () =>
  assert.deepStrictEqual([...pnpmLockDeps(PNPM)], [['@tauri-apps/api', '2.11.1'], ['solid-js', '1.9.15']]));
check('a notices table is read under its heading, links stripped', () => {
  assert.deepStrictEqual([...noticeRows(NOTICES, 'JavaScript')], [['solid-js', '1.9.15']]);
  assert.deepStrictEqual([...noticeRows(NOTICES, 'Rust')], [['serde', '1.0.229']]);
});
check('a missing row, a stale version and a stale row are each flagged', () => {
  const shipped = new Map([['serde', '1.0.229'], ['midir', '0.11.0'], ['log', '0.4.34']]);
  const rows = new Map([['serde', '1.0.229'], ['log', '0.4.33'], ['old', '1.0.0']]);
  assert.deepStrictEqual(compare(shipped, rows), [
    'midir 0.11.0 ships but has no row',
    'log: the row says 0.4.33, the lock resolves 0.4.34',
    'old has a row but is not a direct runtime dependency',
  ]);
});

// ── 2. The repo ────────────────────────────────────────────────────────────────────────────────────
const notices = read('THIRD-PARTY-NOTICES.md');
const lock = read('src-tauri/Cargo.lock');

check('JavaScript: package.json dependencies match the notices', () => {
  const declared = Object.keys(JSON.parse(read('package.json')).dependencies ?? {});
  const resolved = pnpmLockDeps(read('pnpm-lock.yaml'));
  const shipped = new Map(declared.map((name) => [name, resolved.get(name)]));
  for (const [name, version] of shipped) assert.ok(version, `${name}: not in pnpm-lock.yaml's importers`);
  const diff = compare(shipped, noticeRows(notices, 'JavaScript'));
  assert.deepStrictEqual(diff, [], diff.join('\n  '));
});

check('Rust: the app and lf-engine runtime dependencies match the notices', () => {
  const shipped = new Map();
  for (const [toml, pkg] of [['src-tauri/Cargo.toml', 'app'], ['src-tauri/crates/lf-engine/Cargo.toml', 'lf-engine']]) {
    const resolved = cargoLockDeps(lock, pkg);
    for (const name of cargoRuntimeDeps(read(toml))) {
      assert.ok(resolved.get(name), `${name} (${toml}): not among "${pkg}"'s dependencies in Cargo.lock`);
      shipped.set(name, resolved.get(name));
    }
  }
  const diff = compare(shipped, noticeRows(notices, 'Rust'));
  assert.deepStrictEqual(diff, [], diff.join('\n  '));
});

console.log(`\n=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);
