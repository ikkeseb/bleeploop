// verify/guards/web-midi.mjs — the app never opens Web MIDI (`docs/plans/native-midi.md` decision 2): MIDI
// is native (`src-tauri/src/engine_io/midi/`), WinMM input ports are exclusive, and a Web MIDI request
// would take a controller from it (or wait on a permission the WebView denies). No file under `src/` may
// name the Web MIDI API: `requestMIDIAccess`, `MIDIAccess`, `MIDIInput`, `onmidimessage`, `MIDIMessageEvent`,
// comments included (a comment that names it is how a revival starts; describe it in other words).
//
// `webMidiUses` is first run on planted sources (each must be flagged, a clean one must pass), then on
// every file under `src/`. It cannot see an API reached by a computed name. Run: node verify/guards/web-midi.mjs

import assert from 'node:assert';
import { readFileSync, readdirSync } from 'node:fs';
import { dirname, join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const SRC = join(root, 'src');
const API = /\b(?:requestMIDIAccess|MIDIAccess|MIDIInput|MIDIInputMap|MIDIMessageEvent|onmidimessage)\b/g;

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

/** Each Web MIDI name `text` holds, with its line number. */
function webMidiUses(text) {
  const uses = [];
  text.split('\n').forEach((line, i) => {
    for (const m of line.matchAll(API)) uses.push(`${i + 1}: ${m[0]}`);
  });
  return uses;
}

function* files(dir) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) yield* files(path);
    else if (/\.(ts|tsx|js|mjs)$/.test(entry.name)) yield path;
  }
}

// ── The matcher, on planted sources ─────────────────────────────────────────────────────────────────
const PLANTED = {
  'a request': `const access = await navigator.requestMIDIAccess({ sysex: false });`,
  'an optional request': `if (!navigator.requestMIDIAccess) return null;`,
  'a type': `let access: MIDIAccess | null = null;`,
  'a handler': `input.onmidimessage = (e) => parse(e);`,
  'an event type': `const msg = ev as MIDIMessageEvent;`,
  'an input type': `const inputs: MIDIInput[] = [];`,
  'a comment': `// W3C Web MIDI via navigator.requestMIDIAccess`,
};
for (const [name, text] of Object.entries(PLANTED)) {
  check(`flags ${name}`, () => assert.ok(webMidiUses(text).length > 0, text));
}
check('passes native MIDI', () =>
  assert.deepEqual(webMidiUses(`platform.midi.learn('recDub', null); // native MIDI's MidiHost\nconst port: MidiPort = p;`), []),
);

// ── The source ──────────────────────────────────────────────────────────────────────────────────────
let scanned = 0;
for (const path of files(SRC)) {
  scanned++;
  const uses = webMidiUses(readFileSync(path, 'utf8'));
  check(`${relative(root, path)} names no Web MIDI API`, () => assert.deepEqual(uses, []));
}
check('the scan reached the source', () => assert.ok(scanned > 50, `only ${scanned} files under src/`));

console.log(`\n=== RESULT: ${passed}/${passed + failed} checks passed, ${failed} failed ===`);
process.exit(failed === 0 ? 0 : 1);
