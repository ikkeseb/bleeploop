// verify/guards/single-instance.mjs: a second launch of the release app focuses the first and exits.
// Two running apps share one IndexedDB and tone folder, each with its own cache, so one's CLEAR ALL can
// delete the other's recovery. `tauri-plugin-single-instance` refuses the second, from the Tauri path of
// `run()` (src-tauri/src/lib.rs) and only there: registered in release builds alone, so `tauri dev` runs
// beside an installed app; first in the builder, so a refused launch starts nothing; after the
// `--scan-one` and probe dispatches, so those children keep running beside the app that spawned them.
//
// This reads the source: it proves the wiring, not the refusal. Only a real second launch of a release
// exe proves that. The reader is first run on planted texts, then on the repo.
// Run: node verify/guards/single-instance.mjs

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

const CRATE = 'tauri-plugin-single-instance';
const REGISTERED =
  /let builder = tauri::Builder::default\(\);\s*#\[cfg\(all\(windows, not\(debug_assertions\)\)\)\]\s*let builder = builder\.plugin\(tauri_plugin_single_instance::init\(/;

/** What is wrong with Cargo.toml's declaration of the plugin, one line each. */
function manifestFaults(toml) {
  let table = null;
  for (const line of toml.split('\n')) {
    table = line.match(/^\[([^\]]+)\]/)?.[1] ?? table;
    if (!line.startsWith(`${CRATE} `) && !line.startsWith(`${CRATE}=`)) continue;
    const faults = [];
    if (table !== "target.'cfg(windows)'.dependencies") faults.push(`declared under [${table}], not the Windows dependencies`);
    if (!/=\s*"=\d/.test(line)) faults.push('not pinned exactly: newer minors want a newer tauri than the lock holds');
    return faults;
  }
  return [`${CRATE} is not a dependency`];
}

/** What is wrong with `run()`'s wiring of the plugin, one line each. */
function wiringFaults(lib) {
  const code = lib.replace(/^\s*\/\/.*$/gm, '');
  const start = code.indexOf('tauri::Builder::default()');
  if (start === -1) return ['no tauri::Builder in the source'];
  const faults = [];
  if (!REGISTERED.test(code)) faults.push('the plugin is not the first thing on the builder, under the release cfg');
  if (code.split('tauri_plugin_single_instance::').length !== 2) faults.push('the plugin must be registered exactly once');
  const dispatches = [...code.matchAll(/"--(?:scan-one|probe-[a-z-]+)"/g)];
  if (!dispatches.some((m) => m[0] === '"--scan-one"')) faults.push('no --scan-one dispatch in the source');
  for (const m of dispatches) if (m.index > start) faults.push(`${m[0]} dispatches after the builder`);
  for (const call of ['unminimize', 'show', 'set_focus']) {
    if (!code.includes(`window.${call}()`)) faults.push(`the first window is not brought back: no ${call}()`);
  }
  return faults;
}

// ── 1. The readers, on planted texts ───────────────────────────────────────────────────────────────
const TOML = `[dependencies]
tauri = "2"

[target.'cfg(windows)'.dependencies]
${CRATE} = "=2.4.5"
`;
const LIB = `pub fn run() {
    if args.iter().position(|a| a == "--probe-engine") {}
    if args.iter().position(|a| a == "--scan-one") {}
    let builder = tauri::Builder::default();
    #[cfg(all(windows, not(debug_assertions)))]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _, _| front(app)));
    builder.plugin(tauri_plugin_updater::Builder::new().build())
}
fn front(window: W) { window.unminimize(); window.show(); window.set_focus(); }
`;
const without = (text, from, to) => {
  assert.ok(text.includes(from), `the plant holds ${from}`);
  return text.replace(from, to);
};
check('a good manifest and a good run() read clean', () => {
  assert.deepStrictEqual(manifestFaults(TOML), []);
  assert.deepStrictEqual(wiringFaults(LIB), []);
});
check('a missing, a loose and a misplaced dependency are each flagged', () => {
  assert.strictEqual(manifestFaults('[dependencies]\ntauri = "2"\n').length, 1);
  assert.match(manifestFaults(without(TOML, '"=2.4.5"', '"2"')).join(), /not pinned/);
  assert.match(manifestFaults(without(TOML, "[target.'cfg(windows)'.dependencies]", '[dependencies]')).join(), /Windows dependencies/);
});
check('a plugin in debug builds too, or not first, is flagged', () => {
  assert.match(wiringFaults(without(LIB, '    #[cfg(all(windows, not(debug_assertions)))]\n', '')).join(), /not the first/);
  assert.match(wiringFaults(without(LIB, 'let builder = tauri::Builder::default();\n', 'let builder = tauri::Builder::default();\n    let builder = builder.plugin(other);\n')).join(), /not the first/);
});
check('a commented-out registration does not count', () => {
  assert.match(wiringFaults(LIB.replace(/^ {4}(#\[cfg|let builder = builder)/gm, '    // $1')).join(), /not the first/);
});
check('a dispatch after the builder is flagged', () => {
  const late = LIB.replace('    if args.iter().position(|a| a == "--scan-one") {}\n', '').replace('builder.plugin(tauri_plugin_updater', 'if args.iter().position(|a| a == "--scan-one") {}\n    builder.plugin(tauri_plugin_updater');
  assert.match(wiringFaults(late).join(), /--scan-one" dispatches after the builder/);
});
check('a callback that does not restore a minimised window is flagged', () => {
  assert.match(wiringFaults(without(LIB, 'window.unminimize(); ', '')).join(), /no unminimize/);
});

// ── 2. The repo ────────────────────────────────────────────────────────────────────────────────────
check('Cargo.toml declares the plugin for Windows, pinned', () => {
  const faults = manifestFaults(read('src-tauri/Cargo.toml'));
  assert.deepStrictEqual(faults, [], faults.join('\n  '));
});
check("lib.rs wires it into run()'s Tauri path, in release builds alone", () => {
  const faults = wiringFaults(read('src-tauri/src/lib.rs'));
  assert.deepStrictEqual(faults, [], faults.join('\n  '));
});

console.log(`\n=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);
