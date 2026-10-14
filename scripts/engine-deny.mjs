// scripts/engine-deny.mjs — keep lf-engine pure: its normal and build dependency tree, on every target,
// may not contain a host, device or plugin crate (the crate briefing: src-tauri/crates/lf-engine/src/lib.rs).
//
//   node scripts/engine-deny.mjs
//
// Runs `cargo tree` from src-tauri/ on any OS (rust:check runs it on the PC, ci.yml on ubuntu).

import { execFileSync } from 'node:child_process';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

// Whole families by prefix (tauri-*, clack-*, webview2-com*), single crates by name: the audio device
// (cpal, asio-sys), native MIDI's ports (midir), the OS bindings and the plugin ABIs.
const DENIED_PREFIXES = ['tauri', 'clack', 'webview2'];
const DENIED = ['cpal', 'asio-sys', 'midir', 'windows', 'windows-core', 'clap-sys', 'vst3'];

const cwd = join(dirname(fileURLToPath(import.meta.url)), '..', 'src-tauri');
const tree = execFileSync(
  'cargo',
  ['tree', '-p', 'lf-engine', '-e', 'normal,build', '--target', 'all', '--prefix', 'none', '--format', '{p}'],
  { cwd, encoding: 'utf8' },
);
const names = new Set(tree.split('\n').map((line) => line.trim().split(' ')[0]).filter(Boolean));
if (!names.has('lf-engine')) {
  console.error('engine-deny: cargo tree printed no lf-engine root; the check cannot run');
  process.exit(1);
}
const found = [...names].filter((name) => DENIED.includes(name) || DENIED_PREFIXES.some((p) => name.startsWith(p)));
if (found.length > 0) {
  console.error(`engine-deny: FAIL, lf-engine depends on ${found.join(', ')}`);
  process.exit(1);
}
console.log(`engine-deny: ok (${names.size} crates, none denied)`);
