// verify/guards/version-sync.mjs — the release's version is one number in four places (build-exe.yml's
// release procedure bumps them together): package.json (Help's "About this build"), tauri.conf.json (the
// exe, the installer's name, the updater's comparison and the tag check), src-tauri/Cargo.toml's
// `[package]` and its `name = "app"` entry in Cargo.lock. A missed one ships a mislabelled build or a
// lockfile cargo rewrites on the next build.
//
// Each reader is first run on planted texts (a mismatch in any one must be flagged, a version it cannot
// find too), then on the four files. Run: node verify/guards/version-sync.mjs

import assert from 'node:assert';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');

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

/** The `version` key of `[package]` in a Cargo.toml, or undefined. */
function cargoTomlVersion(text) {
  const section = text.match(/^\[package\]\s*$([\s\S]*?)(?=^\[|(?![\s\S]))/m)?.[1] ?? '';
  return section.match(/^version\s*=\s*"([^"]+)"/m)?.[1];
}

/** The version of the `name = "app"` package in a Cargo.lock, or undefined. */
function cargoLockVersion(text) {
  return text.match(/^\[\[package\]\]\r?\nname = "app"\r?\nversion = "([^"]+)"/m)?.[1];
}

/** Each source's version, and whether all four agree. */
function versions({ pkg, conf, toml, lock }) {
  const found = {
    'package.json': JSON.parse(pkg).version,
    'src-tauri/tauri.conf.json': JSON.parse(conf).version,
    'src-tauri/Cargo.toml [package]': cargoTomlVersion(toml),
    'src-tauri/Cargo.lock "app"': cargoLockVersion(lock),
  };
  const values = Object.values(found);
  return { found, agree: values.every((v) => typeof v === 'string' && v === values[0]) };
}

// ── 1. The readers, on planted texts ───────────────────────────────────────────────────────────────
const TOML = `[package]\nname = "app"\nversion = "1.2.3"\n\n[dependencies]\nfoo = { version = "9.9.9" }\n`;
const LOCK = `[[package]]\nname = "anyhow"\nversion = "1.0.0"\n\n[[package]]\nname = "app"\nversion = "1.2.3"\ndependencies = [\n "anyhow",\n]\n`;
const PLANTED = { pkg: '{"version":"1.2.3"}', conf: '{"productName":"x","version":"1.2.3"}', toml: TOML, lock: LOCK };
check('four equal versions agree', () => assert.ok(versions(PLANTED).agree, JSON.stringify(versions(PLANTED).found)));
check('a [package] version is read past another table', () =>
  assert.strictEqual(cargoTomlVersion(`[workspace]\nversion = "0.0.1"\n\n${TOML}`), '1.2.3'));
check('a dependency table version is not [package]', () =>
  assert.strictEqual(cargoTomlVersion(`[package]\nname = "app"\n\n[dependencies]\nversion = "9.9.9"\n`), undefined));
check('the app entry, not the one before it, is read from Cargo.lock', () => assert.strictEqual(cargoLockVersion(LOCK), '1.2.3'));
check('a CRLF Cargo.lock is read', () => assert.strictEqual(cargoLockVersion(LOCK.replace(/\n/g, '\r\n')), '1.2.3'));
const MISMATCHES = {
  'package.json': { pkg: '{"version":"1.2.4"}' },
  'tauri.conf.json': { conf: '{"version":"1.2.4"}' },
  'Cargo.toml': { toml: TOML.replace('version = "1.2.3"', 'version = "1.2.4"') },
  'Cargo.lock': { lock: LOCK.replace('name = "app"\nversion = "1.2.3"', 'name = "app"\nversion = "1.2.4"') },
  'a Cargo.lock without the app': { lock: LOCK.replace('name = "app"', 'name = "apq"') },
  'a missing version on every side': { pkg: '{}', conf: '{}', toml: '[package]\n', lock: '' },
};
for (const [name, planted] of Object.entries(MISMATCHES)) {
  check(`flags ${name}`, () => assert.ok(!versions({ ...PLANTED, ...planted }).agree));
}

// ── 2. The four files ──────────────────────────────────────────────────────────────────────────────
const read = (path) => readFileSync(join(root, path), 'utf8');
const real = versions({
  pkg: read('package.json'),
  conf: read('src-tauri/tauri.conf.json'),
  toml: read('src-tauri/Cargo.toml'),
  lock: read('src-tauri/Cargo.lock'),
});
check('the four versions agree', () =>
  assert.ok(real.agree, Object.entries(real.found).map(([where, v]) => `${where}: ${v}`).join(', ')));

console.log(`version-sync: ${Object.values(real.found).join(' · ')}`);
console.log(`=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);
