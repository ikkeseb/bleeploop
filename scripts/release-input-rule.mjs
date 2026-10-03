// scripts/release-input-rule.mjs — when `pnpm release:smoke` must record the device input (D26).
//
// The smoke's input take needs a loopback cable from an output into the picked input. The cable
// proves the release build's input path, which only needs proving again when the device side changed
// since the last release. When the input take reads flat, the smoke asks this rule: a required input
// take fails the run, one that is not required lets a built-in synth take stand in.
//
// Pure: scripts/release-changes.mjs makes the git calls and the smoke hands in what they answered.
// Anything missing from that answer requires the input take. Guard: verify/guards/release-input-rule.mjs.

/** The device side: the engine's device code and the plugin host, by directory. */
const DEVICE_DIRS = ['src-tauri/src/engine_io/', 'src-tauri/src/host/'];
/** The device side's single files. */
const DEVICE_FILES = ['src-tauri/src/audio_output.rs', 'src-tauri/src/asio_startup.rs'];
/** The crates that open the device. */
const DEVICE_CRATES = ['cpal', 'asio-sys'];

/** Every `[[package]]` entry of the device crates in a Cargo.lock, as one comparable string. An entry
 * runs from its `[[...]]` header to the next one, whatever blank lines lie inside it. */
function devicePackages(lock) {
  return lock
    .split(/^(?=\[\[)/m)
    .map((block) => block.split(/\r?\n/).map((line) => line.trimEnd()).filter(Boolean))
    .filter((lines) => DEVICE_CRATES.some((crate) => lines.includes(`name = "${crate}"`)))
    .map((lines) => lines.join('\n'))
    .sort()
    .join('\n\n');
}

/** A Cargo.toml's lines without its `[package]` table's `version = "..."` line (the release bump). The
 * tables are read line by line, which a multi-line string could fool (a `[package]` line inside one):
 * a manifest that holds one is compared whole, the version line included. */
function manifestLines(toml) {
  let table = '';
  const lines = toml.split(/\r?\n/);
  if (/"""|'''/.test(toml)) return lines;
  return lines.filter((line) => {
    const header = line.match(/^\s*\[\[?\s*([^\]]*?)\s*\]\]?\s*(?:#.*)?$/);
    if (header) table = header[1];
    return !(table === 'package' && /^\s*version\s*=\s*"[^"]*"\s*(?:#.*)?$/.test(line));
  });
}

/**
 * Whether this release must pass the input take, and why.
 *
 * @param {object} change what `releaseChanges` (scripts/release-changes.mjs) answered
 * @param {string | null} change.base the last release tag; null when there is none
 * @param {string[] | null} change.paths the changed paths, repo-relative with `/`; null when git gave no answer
 * @param {boolean} [change.clean] whether `git status` printed nothing (untracked files included)
 * @param {string[]} [change.dirty] `git status`'s entries when it did, for the reason
 * @param {string} [change.manifestBefore] `src-tauri/Cargo.toml` at `base`
 * @param {string} [change.manifestAfter] `src-tauri/Cargo.toml` now
 * @param {string} [change.lockBefore] `src-tauri/Cargo.lock` at `base`
 * @param {string} [change.lockAfter] `src-tauri/Cargo.lock` now
 * @returns {{ required: boolean, why: string[] }} `why` names what requires the input take; empty when it is not required
 */
export function inputTakeRule({ base, paths, clean, dirty = [], manifestBefore, manifestAfter, lockBefore, lockAfter }) {
  // Fail closed: with nothing to compare against, the input path counts as changed.
  if (!base) return { required: true, why: ['no release tag of another version to compare against'] };
  const texts = [manifestBefore, manifestAfter, lockBefore, lockAfter];
  if (!Array.isArray(paths) || typeof clean !== 'boolean' || !texts.every((t) => typeof t === 'string')) {
    return { required: true, why: [`git gave no answer about the changes since ${base}`] };
  }
  const why = [];
  // Git's diff misses an untracked file, and an unstaged edit can cancel a staged one.
  if (!clean) {
    why.push(`the tree is not clean${dirty.length ? ` (${dirty.slice(0, 3).join(', ')}${dirty.length > 3 ? `, and ${dirty.length - 3} more` : ''})` : ''}: commit first, then run again`);
  }
  why.push(...paths.filter((p) => DEVICE_DIRS.some((dir) => p.startsWith(dir)) || DEVICE_FILES.includes(p)));
  // The manifest as a whole: a change under a crate's own table need not name the crate on its line.
  const before = manifestLines(manifestBefore);
  const after = manifestLines(manifestAfter);
  if (before.join('\n') !== after.join('\n')) {
    const had = new Set(before);
    const has = new Set(after);
    const moved = [...after.filter((l) => !had.has(l)), ...before.filter((l) => !has.has(l))].map((l) => l.trim()).filter(Boolean);
    why.push(`src-tauri/Cargo.toml differs from ${base} beyond its [package] version${moved.length ? ` (${moved.slice(0, 3).join(' | ')})` : ''}`);
  }
  // The lock entry by entry: a version bump changes the lines under the crate's name.
  if (devicePackages(lockBefore) !== devicePackages(lockAfter)) {
    why.push(`a ${DEVICE_CRATES.join(' or ')} entry of src-tauri/Cargo.lock differs from ${base}`);
  }
  return { required: why.length > 0, why };
}
