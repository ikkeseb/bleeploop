// verify/guards/single-instance.mjs: a second launch of the release app focuses the first and exits.
// Two running apps share one IndexedDB and tone folder, each with its own cache, so one's CLEAR ALL can
// delete the other's recovery. `src-tauri/src/single_instance.rs` refuses the second before Tauri starts:
// `run()` (src-tauri/src/lib.rs) calls it in release builds on Windows alone, so `tauri dev` runs beside
// an installed app, and after the `--scan-one` and probe dispatches, whose children must leave `run()`
// first and keep running beside the app that spawned them.
//
// This reads the source: it proves the wiring, not the refusal. The module's decision has unit tests
// (`cargo test -p app single_instance`, the real kernel mutex and no window); only a real second launch
// of a release exe proves the focus. Each mutation below is applied to the repo's own text and must go
// red, so a wiring that a reader stopped seeing fails here.
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
/** What must leave `run()` before the lock. */
const CHILDREN = ['--probe-engine-spike', '--probe-engine', '--probe-share', '--probe-share-child', '--scan-one'];
const RELEASE_CFG = '#[cfg(all(windows, not(debug_assertions)))]';
const REFUSED =
  /if let single_instance::Claim::Exit = single_instance::first_launch\(&context\.config\(\)\.identifier\) \{\s*std::process::exit\(0\);\s*\}/;

const code = (text) => text.replace(/(^|\s)\/\/.*$/gm, '$1');

/** The `{…}` block opening at or after `from`; braces in strings balance in this source. */
function block(text, from) {
  const open = text.indexOf('{', from);
  if (open === -1) return '';
  let depth = 0;
  for (let i = open; i < text.length; i++) {
    if (text[i] === '{') depth++;
    if (text[i] === '}' && --depth === 0) return text.slice(open, i + 1);
  }
  return '';
}

function fnBody(text, signature) {
  const at = text.indexOf(signature);
  return at === -1 ? '' : block(text, at);
}

/** Every way out of a dispatch's block ends the process: a `match` with both arms, or a last statement. */
const leaves = (dispatch) =>
  dispatch.includes('match ')
    ? /Ok\(\(\)\) => std::process::exit\(0\),/.test(dispatch) && /std::process::exit\(1\);\s*\}\s*\}\s*\}$/.test(dispatch)
    : /std::process::exit\([^;]*;\s*\}$/.test(dispatch);

/** What is wrong with `run()`'s wiring of the lock, one line each. */
function runFaults(lib) {
  const source = code(lib);
  const run = fnBody(source, 'pub fn run()');
  if (!run) return ['no run() in lib.rs'];
  const faults = [];
  const builder = run.indexOf('tauri::Builder::default()');
  if (builder === -1) faults.push('no tauri::Builder in run()');
  if (!/#\[cfg\(windows\)\]\s*(#\[cfg_attr\(debug_assertions, allow\(dead_code\)\)\]\s*)?mod single_instance;/.test(source)) {
    faults.push('the module is not declared for Windows alone');
  }

  const refused = run.match(REFUSED);
  if (!refused) faults.push('a refused launch does not end the process: run() must exit(0) on Claim::Exit');
  if (source.split('single_instance::first_launch').length !== 2) faults.push('first_launch must have one caller');
  const call = refused ? refused.index : -1;
  if (refused) {
    if (!run.slice(0, call).trimEnd().endsWith(RELEASE_CFG)) faults.push('the call is not under the release-and-Windows cfg');
    if (builder !== -1 && call > builder) faults.push('the call comes after the builder: a refused launch would start Tauri');
  }

  if (source.split('tauri::generate_context!()').length !== 2) faults.push('generate_context! must run once');
  if (!/let context = tauri::generate_context!\(\);/.test(run) || !run.includes('.build(context)')) {
    faults.push('the identifier does not come from the context the builder gets');
  }

  for (const child of CHILDREN) {
    const test = run.indexOf(`a == "${child}"`);
    if (test === -1) {
      faults.push(`no ${child} dispatch in run(): that child would meet the lock`);
      continue;
    }
    if (call !== -1 && test > call) faults.push(`${child} dispatches after the lock`);
    if (!leaves(block(run, test))) faults.push(`${child} does not leave run() on every path`);
  }
  for (const m of run.matchAll(/"--(?:scan-[a-z-]+|probe-[a-z-]+)"/g)) {
    if (call !== -1 && m.index > call) faults.push(`${m[0]} dispatches after the lock`);
  }
  if (/#\[cfg\(debug_assertions\)\]\s*if let Some\(pos\) = args\.iter\(\)\.position\(\|a\| a == "--scan-one"\)/.test(run)) {
    faults.push('--scan-one dispatches in debug alone: a release scan child would meet the lock');
  }
  return faults;
}

/** What is wrong with the module's production half, one line each. */
function moduleFaults(module) {
  const source = code(module);
  const tests = source.indexOf('#[cfg(all(test, windows))]');
  if (tests === -1) return ['the module has no unit tests under #[cfg(all(test, windows))]'];
  const prod = source.slice(0, tests);
  const faults = [];
  if (!/fn first_launch[^]*claim\(&name, QUIT_WAIT, POLL, find_window, focus_window\)/.test(prod)) {
    faults.push('first_launch does not pass the Win32 lookup and focus to claim');
  }
  const focus = fnBody(prod, 'fn focus_window(');
  for (const call of ['IsIconic(window)', 'ShowWindowAsync(window, SW_RESTORE)', 'SetForegroundWindow(window)']) {
    if (!focus.includes(call)) faults.push(`focus_window does not reach ${call}`);
  }
  const find = fnBody(prod, 'fn find_window(');
  for (const call of ['FindWindowW(WINDOW_CLASS', 'IsWindowVisible(window)']) {
    if (!find.includes(call)) faults.push(`find_window does not reach ${call}`);
  }
  if (!/Found::Visible\(window\) => \{\s*close\(handle\);\s*focus\(window\);\s*return Claim::Exit;/.test(fnBody(prod, 'pub fn claim<W>('))) {
    faults.push('claim does not focus a visible window and exit');
  }
  for (const call of prod.matchAll(/\b(SendMessage\w*|ShowWindow)\(/g)) {
    faults.push(`${call[1]} waits on the first's thread: a hung first would hang the launch`);
  }
  return faults;
}

const seconds = (text, name) => Number(text.match(new RegExp(`const ${name}: Duration = Duration::from_secs\\((\\d+)\\)`))?.[1]);

/** What is wrong across the module, its window class and the exit path it waits for, one line each. */
function boundFaults(module, conf, mode) {
  const faults = [];
  const constant = code(module).match(/const WINDOW_CLASS: PCWSTR = w!\("([^"]+)"\);/)?.[1];
  const window = JSON.parse(conf).app.windows[0];
  if (!constant || window.windowClassname !== constant) {
    faults.push(`the conf's windowClassname (${window.windowClassname}) is not the module's WINDOW_CLASS (${constant})`);
  }
  const wait = seconds(module, 'QUIT_WAIT');
  const shutdown = seconds(mode, 'SHUTDOWN_WAIT');
  if (!(wait > shutdown)) faults.push(`QUIT_WAIT (${wait} s) must pass SHUTDOWN_WAIT (${shutdown} s), or a reopen during a quit overlaps it`);
  return faults;
}

/** What is left of the plugin, one line each. */
function pluginFaults(texts) {
  return Object.entries(texts)
    .filter(([, text]) => text.includes(CRATE) || text.includes(CRATE.replaceAll('-', '_')))
    .map(([file]) => `${CRATE} is still in ${file}`);
}

const repo = {
  lib: read('src-tauri/src/lib.rs'),
  module: read('src-tauri/src/single_instance.rs'),
  conf: read('src-tauri/tauri.conf.json'),
  mode: read('src-tauri/src/engine_io/mode.rs'),
  toml: read('src-tauri/Cargo.toml'),
  lock: read('src-tauri/Cargo.lock'),
  notices: read('THIRD-PARTY-NOTICES.md'),
};
const faultsIn = (t) => [
  ...runFaults(t.lib),
  ...moduleFaults(t.module),
  ...boundFaults(t.module, t.conf, t.mode),
  ...pluginFaults({ 'src-tauri/Cargo.toml': t.toml, 'src-tauri/Cargo.lock': t.lock, 'src-tauri/src/lib.rs': code(t.lib), 'THIRD-PARTY-NOTICES.md': t.notices }),
];

// ── 1. The repo ────────────────────────────────────────────────────────────────────────────────────
check('the lock is wired, the children leave first, the plugin is gone', () => {
  const faults = faultsIn(repo);
  assert.deepStrictEqual(faults, [], faults.join('\n  '));
});

// ── 2. The readers: each mutation of the repo's own text goes red ─────────────────────────────────
const CALL = repo.lib.match(/ *if let single_instance::Claim::Exit[^]*?\n {4}\}\n/)?.[0] ?? '';
const SCAN = repo.lib.match(/ *if let Some\(pos\) = args\.iter\(\)\.position\(\|a\| a == "--scan-one"\) \{[^]*?\n {8}\}\n/)?.[0] ?? '';
const ENGINE = repo.lib.match(/ *#\[cfg\(debug_assertions\)\]\n *if let Some\(pos\) = args\.iter\(\)\.position\(\|a\| a == "--probe-engine"\) \{[^]*?\n {8}\}\n/)?.[0] ?? '';
const swap = (key, from, to) => (t) => {
  assert.ok(from && t[key].includes(from), `the mutation's text is in ${key}: ${from}`);
  return { ...t, [key]: t[key].replace(from, to) };
};
const MUTATIONS = {
  'a call that ignores the result': [
    swap('lib', CALL, '    let _ = single_instance::first_launch(&context.config().identifier);\n'),
    /does not end the process/,
  ],
  'an Exit that does not exit': [swap('lib', 'identifier) {\n        std::process::exit(0);\n    }', 'identifier) {\n    }'), /does not end the process/],
  'a commented-out call': [swap('lib', CALL, CALL.replace(/^/gm, '// ')), /does not end the process/],
  'a no-op focus': [swap('module', 'find_window, focus_window)', 'find_window, |_| {})'), /does not pass the Win32 lookup/],
  'a focus that stops short of Win32': [swap('module', 'let _ = SetForegroundWindow(window);', ''), /does not reach SetForegroundWindow/],
  'a claim that drops the focus': [swap('module', 'focus(window);\n                return', 'return'), /does not focus a visible window/],
  'a --scan-one child that stays in run()': [
    swap('lib', 'std::process::exit(host::scan_one_main(path));', 'let _ = host::scan_one_main(path);'),
    /--scan-one does not leave/,
  ],
  'a removed --scan-one dispatch': [swap('lib', SCAN, ''), /no --scan-one dispatch/],
  'a probe child that stays in run()': [
    swap('lib', 'engine_io::probe::run(&args[pos + 1..]) {\n                Ok(()) => std::process::exit(0),', 'engine_io::probe::run(&args[pos + 1..]) {\n                Ok(()) => {}'),
    /--probe-engine does not leave/,
  ],
  'a removed probe dispatch': [swap('lib', ENGINE, ''), /no --probe-engine dispatch/],
  'a --scan-one dispatch in debug alone': [swap('lib', SCAN, `        #[cfg(debug_assertions)]\n${SCAN}`), /in debug alone/],
  'a call before the dispatches': [
    (t) => swap('lib', 'pub fn run() {\n', `pub fn run() {\n${RELEASE_CFG}\n${CALL}`)(swap('lib', `    ${RELEASE_CFG}\n${CALL}`, '')(t)),
    /dispatches after the lock/,
  ],
  'a call in debug builds too': [swap('lib', `    ${RELEASE_CFG}\n${CALL}`, CALL), /release-and-Windows/],
  'a module on every platform': [swap('lib', '#[cfg(windows)]\n#[cfg_attr(debug_assertions, allow(dead_code))]\nmod single_instance;', 'mod single_instance;'), /declared for Windows alone/],
  'a synchronous send to the first': [swap('module', 'focus(window);\n', 'SendMessageW(window, 0, None, None);\n focus(window);\n'), /SendMessageW waits/],
  'the plugin back in Cargo.toml': [swap('toml', '[dependencies]\n', `[dependencies]\n${CRATE} = "2"\n`), /still in src-tauri\/Cargo\.toml/],
  'the plugin back in Cargo.lock': [(t) => ({ ...t, lock: `${t.lock}\n[[package]]\nname = "${CRATE}"\n` }), /still in src-tauri\/Cargo\.lock/],
  'a class the module does not look for': [swap('conf', '"BleepLoopMainWindow"', '"Tauri Window"'), /not the module's WINDOW_CLASS/],
  'a bound below the shutdown': [swap('module', 'QUIT_WAIT: Duration = Duration::from_secs(10)', 'QUIT_WAIT: Duration = Duration::from_secs(5)'), /must pass SHUTDOWN_WAIT/],
};
for (const [name, [mutate, expected]] of Object.entries(MUTATIONS)) {
  check(`${name} is flagged`, () => assert.match(faultsIn(mutate(repo)).join('\n'), expected));
}

console.log(`\n=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);
