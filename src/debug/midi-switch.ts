/**
 * DEV probe: the UI's note sources through native MIDI's one router, in the real app (`docs/VERIFY.md`,
 * native:midi). `pnpm native:midi` launches the WASAPI dev app once, in a profile of its own
 * (`scripts/midi-probe.tauri.json`), and the probe reloads the WebView once inside that launch
 * (sessionStorage carries it across). No MIDI port is needed: the checks drive the UI side.
 *
 *   silence  the master muted and at volume 0, the click off, before any note; the probe reads the mute
 *            back from the feed's reset frame (the settings the engine host keeps) and plays nothing
 *            unless it is there. No plugin loads, no GO LIVE, no take.
 *   1 resync native MIDI answers a new listener with its resync: the ports (none on a PC without an
 *            input), the bindings, learning null, the held set
 *   2 deny   the WebView's own MIDI API is refused to the page (`src-tauri/src/lib.rs` denies the
 *            permission), so native MIDI keeps the ports
 *   3 note   a PC key's keydown (Keyboard.tsx's handler) reaches the router: native MIDI's held set and
 *            the key light the note. The router records a hold only for an attack its queue admitted
 *            with a device running; the feed carries no instrument level, so the engine's render of the
 *            note is not observed (muted, it would read silence anyway)
 *   4 owners keyup clears the held set; two keys on one note (an octave shift between them) keep it held
 *            until the last lets go
 *   5 blur   the window's blur releases a held key; its later keyup sends no note
 *   7 slot   switching the active slot to the other built-in synth releases a held key; its keyup sends
 *            no note; then back
 *   6 reload a key held across a WebView reload (the old document sends nothing more) is released
 *            natively by the new document's epoch: the new listener's resync holds no note, and a key in
 *            the new document sounds (its epoch is the current one)
 *
 * The probe takes native MIDI's event channel and the feed for a moment to read their resyncs (each has one
 * subscriber), then hands both back to the UI (`startMidi`, `startEngineStore`), which gets a resync of its
 * own. It watches `platform.input.send` to see what the keyboard sent. Check 8 (no Rust ERROR line,
 * host_init's `frontend_epoch=2` for the reloaded document: the page cannot read its epoch) is the
 * runner's, on its log (`scripts/native-probe.mjs`). Native MIDI logs nothing at its start with no input
 * port and a clean store, and the permission deny's warning comes only at a profile's first request
 * (WebView2 keeps the denial), so neither line is required.
 * `[midi-switch]` lines go through `console.error`; the runner fails the run on any other.
 *
 * Trigger: `VITE_LF_PROBE=midi-switch` at Vite start (DEV only). No knobs.
 */
import { platform, input, type EngineCommand, type FeedFrame, type InputEvent, type MidiEvent } from '../platform';
import { engineDevice, startEngineStore } from '../ui/state/engine-store';
import { clock, looper, master } from '../ui/state/audio';
import { activeSlot, selectSynth, setActiveSlot, slotIds, slotPlugins } from '../ui/state/instrument';
import { nativeHostReady } from '../ui/state/instrument-slots';
import { bindings, heldNotes, learning, ports, startMidi } from '../ui/state/midi';
import { keyboardOctave } from '../ui/keyboard/Keyboard';

const TAG = '[midi-switch]';
const STATE_KEY = 'lf.probe.midi-switch';
/** How long a native answer may take after `input_send` resolved (the held event rides its own channel). */
const SETTLE_MS = 300;
/** How long the master's glide to 0 gets before the first note. */
const GLIDE_MS = 600;

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
const log = (...args: unknown[]) => console.error(TAG, ...args);

async function until(label: string, predicate: () => boolean, seconds: number): Promise<void> {
  for (let i = 0; i < seconds * 50; i++) {
    if (predicate()) return;
    await sleep(20);
  }
  throw Error(`timed out after ${seconds} s waiting for ${label}`);
}

function expect(ok: boolean, what: string): asserts ok {
  if (!ok) throw Error(what);
}

// ── What the keyboard sends ────────────────────────────────────────────────────────────────────────
// Every `input_send` batch, in order; `frozen` drops them (the reloading document's last words).
const sent: { events: readonly InputEvent[]; done: Promise<void> }[] = [];
let frozen = false;
function watchInput(): void {
  const real = platform.input.send.bind(platform.input);
  platform.input.send = (events) => {
    if (frozen) return Promise.resolve();
    const done = real(events);
    sent.push({ events, done });
    return done;
  };
}
type NoteEvent = Extract<InputEvent, { note: unknown }>['note'];
const notesFrom = (from: number): NoteEvent[] =>
  sent.slice(from).flatMap((s) => s.events.flatMap((e) => (typeof e === 'object' && 'note' in e ? [e.note] : [])));
/** The outbox's batches have left and native MIDI has had time to answer. */
async function settle(): Promise<void> {
  await sleep(30);
  await Promise.allSettled(sent.map((s) => s.done));
  await sleep(SETTLE_MS);
}

// ── The keyboard, through its own window listeners ─────────────────────────────────────────────────
function key(type: 'keydown' | 'keyup', k: string): void {
  window.dispatchEvent(new KeyboardEvent(type, { key: k, code: `Key${k.toUpperCase()}`, bubbles: true, cancelable: true }));
}
const held = (): number[] => [...heldNotes()].sort((a, b) => a - b);
const heldText = () => `[${held().join(',')}]`;
const lit = (note: number) => document.querySelector(`.kb__key[data-note="${note}"]`)?.classList.contains('kb__key--down') === true;
/** Let go of every key the probe may hold and panic, after a failed check (the master is muted). */
function letGo(): void {
  for (const k of ['a', 's', 'd', 'f', 'k']) key('keyup', k);
  input.allNotesOff();
}

// ── Resyncs ────────────────────────────────────────────────────────────────────────────────────────
/** Take native MIDI's channel for its resync, then hand it back to the UI (which gets its own). */
async function midiResync(): Promise<MidiEvent[]> {
  const got: MidiEvent[] = [];
  platform.midi.subscribe((ev) => got.push(ev));
  await until('native MIDI resync', () => got.some((e) => e.type === 'held'), 10);
  startMidi();
  return got;
}

/** Take the feed for its reset frame's settings (what the engine host keeps), then hand it back. */
async function feedSettings(): Promise<EngineCommand[]> {
  const first: { frame: FeedFrame | null } = { frame: null };
  platform.engine.subscribe((f) => {
    if (f.reset && first.frame === null) first.frame = f;
  });
  await until('a reset frame', () => first.frame !== null, 10);
  startEngineStore();
  return first.frame!.settings ?? [];
}

/** Mute and silence the master, and prove it from the feed before any note. */
async function silence(): Promise<string> {
  master.setVolume(0);
  master.setMuted(true);
  clock.setMetronome(false);
  await sleep(500);
  const settings = await feedSettings();
  const muted = settings.some((c) => typeof c === 'object' && 'SetMasterMute' in c && c.SetMasterMute);
  const volume = settings.find((c): c is { SetMasterVolume: number } => typeof c === 'object' && 'SetMasterVolume' in c);
  const click = settings.some((c) => typeof c === 'object' && 'SetMetronome' in c && c.SetMetronome);
  expect(muted, `the feed's settings do not hold SetMasterMute true: ${JSON.stringify(settings)}`);
  expect(!click, 'the feed says the click is on');
  await until('the UI to read the mute back from the feed', () => master.muted(), 5);
  await sleep(GLIDE_MS);
  return `master muted on the feed (SetMasterMute true, volume ${volume?.SetMasterVolume ?? 'default'}), click off`;
}

// ── The checks ─────────────────────────────────────────────────────────────────────────────────────
interface Saved {
  lines: string[];
  failed: number;
}
const results: Saved = { lines: [], failed: 0 };

async function check(n: number, run: () => Promise<string>): Promise<void> {
  let line: string;
  try {
    line = `check ${n} PASS: ${await run()}`;
  } catch (e) {
    results.failed++;
    line = `check ${n} FAIL: ${e instanceof Error ? e.message : String(e)}`;
    letGo();
    await settle();
  }
  results.lines.push(line);
  log(line);
}

/** The first event of `type` among `events`. */
function first<T extends MidiEvent['type']>(events: MidiEvent[], type: T): Extract<MidiEvent, { type: T }> | undefined {
  return events.find((e): e is Extract<MidiEvent, { type: T }> => e.type === type);
}

function resyncText(events: MidiEvent[]): string {
  const p = first(events, 'ports');
  const b = first(events, 'bindings');
  const l = first(events, 'learning');
  const h = first(events, 'held');
  expect(p !== undefined && b !== undefined && l !== undefined && h !== undefined, `the resync lacks a part: ${events.map((e) => e.type).join(', ')}`);
  expect(l.learning === null, `learning is ${JSON.stringify(l.learning)}`);
  return `resync ${events.map((e) => e.type).join(', ')}: ${p.ports.length} port(s)${p.ports.length ? ` (${p.ports.map((x) => `${x.name} ${x.state}`).join(', ')})` : ''}, ${b.bindings.length} binding(s), learning null, held [${h.notes.join(',')}]`;
}

async function checkResync(): Promise<string> {
  const events = await midiResync();
  const text = resyncText(events);
  const portCount = first(events, 'ports')?.ports.length;
  const bindingCount = first(events, 'bindings')?.bindings.length;
  await until('the UI state to follow its own resync', () => ports().length === portCount && bindings().length === bindingCount, 5);
  expect(learning() === null, 'the UI says MIDI learn is listening');
  return text;
}

async function checkDeny(): Promise<string> {
  // The one Web MIDI name under `src/` (`verify/guards/web-midi.mjs` names this file as its exception):
  // the probe asks for it to prove the WebView refuses it.
  if (typeof navigator.requestMIDIAccess !== 'function') return 'the page has no Web MIDI request at all';
  const answer = await Promise.race([
    navigator.requestMIDIAccess({ sysex: false }).then(
      () => 'granted',
      (e: unknown) => `refused: ${e instanceof Error ? `${e.name}: ${e.message}` : String(e)}`,
    ),
    sleep(15_000).then(() => 'no answer in 15 s'),
  ]);
  expect(answer.startsWith('refused'), `the Web MIDI request was ${answer}`);
  return `the Web MIDI request was ${answer}`;
}

async function checkNote(): Promise<string> {
  const from = sent.length;
  key('keydown', 'a');
  await until('native MIDI to hold note 60', () => heldNotes().has(60), 5);
  const ons = notesFrom(from);
  expect(ons.length === 1 && ons[0].owner === 'key:KeyA' && ons[0].note === 60 && ons[0].on && ons[0].velocity === 100, `the keydown sent ${JSON.stringify(ons)}`);
  await settle();
  expect(lit(60), 'the key of note 60 is not lit');
  expect(held().join() === '60', `held ${heldText()}`);
  return `keydown a → input_send note 60 on (key:KeyA, velocity 100) → native held [60], key lit; the router admitted the attack with a device running (the feed shows no instrument level)`;
}

async function checkOwners(): Promise<string> {
  // Note 60 from check 3 is still held by key:KeyA.
  key('keyup', 'a');
  await until('the held set to clear', () => heldNotes().size === 0, 5);
  await settle();
  expect(!lit(60), 'note 60 stays lit after its keyup');
  // k at C4 is 72; x shifts to C5, where a is 72 too.
  expect(keyboardOctave() === 4, `the keyboard is at octave ${keyboardOctave()}`);
  const from = sent.length;
  key('keydown', 'k');
  await until('note 72 held', () => heldNotes().has(72), 5);
  key('keydown', 'x');
  key('keyup', 'x');
  await until('octave 5', () => keyboardOctave() === 5, 2);
  key('keydown', 'a');
  await settle();
  const ons = notesFrom(from).filter((e) => e.on);
  expect(ons.length === 2 && ons.every((e) => e.note === 72) && ons[0].owner === 'key:KeyK' && ons[1].owner === 'key:KeyA', `the two presses sent ${JSON.stringify(ons)}`);
  expect(held().join() === '72', `held ${heldText()} with two owners on 72`);
  key('keyup', 'k');
  await settle();
  expect(held().join() === '72', `held ${heldText()} after the first owner let go`);
  expect(lit(72), 'note 72 went dark while its second owner holds it');
  key('keyup', 'a');
  await until('the held set to clear after the last owner', () => heldNotes().size === 0, 5);
  await settle();
  expect(!lit(72), 'note 72 stays lit after both keyups');
  key('keydown', 'z');
  key('keyup', 'z');
  await until('octave 4 again', () => keyboardOctave() === 4, 2);
  return 'keyup a → held []; k (72) then x (octave 5) then a (72): held [72] with two owners, still [72] after k up, [] after a up';
}

async function checkBlur(): Promise<string> {
  key('keydown', 's');
  await until('note 62 held', () => heldNotes().has(62), 5);
  const from = sent.length;
  window.dispatchEvent(new FocusEvent('blur'));
  await until('the blur to clear the held set', () => heldNotes().size === 0, 5);
  await settle();
  const blurSent = sent.slice(from).some((s) => s.events.includes('blur'));
  expect(blurSent, 'the blur sent no blur event');
  expect(!lit(62), 'note 62 stays lit after the blur');
  const after = sent.length;
  key('keyup', 's');
  await settle();
  const late = notesFrom(after);
  expect(late.length === 0, `the keyup after the blur sent ${JSON.stringify(late)}`);
  expect(heldNotes().size === 0, `held ${heldText()} after the late keyup`);
  return 'keydown s → held [62]; window blur → input_send blur → held [], key dark; the later keyup sent nothing, held stays []';
}

async function checkSlotSwitch(): Promise<string> {
  expect(activeSlot() === 0, `slot ${activeSlot() + 1} is active`);
  const [a, b] = slotIds();
  expect(slotPlugins()[0] === null && slotPlugins()[1] === null, 'a slot holds a plugin');
  key('keydown', 'd');
  await until('note 64 held', () => heldNotes().has(64), 5);
  const from = sent.length;
  setActiveSlot(1);
  await until('the slot switch to clear the held set', () => heldNotes().size === 0, 5);
  await settle();
  const targets = sent.slice(from).flatMap((s) => s.events.filter((e) => typeof e === 'object' && 'selectTarget' in e));
  expect(targets.length > 0, 'the switch sent no note target');
  expect(!lit(64), 'note 64 stays lit after the switch');
  const after = sent.length;
  key('keyup', 'd');
  await settle();
  const late = notesFrom(after);
  expect(late.length === 0, `the keyup after the switch sent ${JSON.stringify(late)}`);
  expect(heldNotes().size === 0, `held ${heldText()} after the late keyup`);
  setActiveSlot(0);
  await settle();
  expect(activeSlot() === 0 && heldNotes().size === 0, `back on slot ${activeSlot() + 1}, held ${heldText()}`);
  return `keydown d → held [64] on slot A (${a}); slot B (${b}) active → ${JSON.stringify(targets[0])} → held [], key dark; the keyup sent nothing; back to slot A, held []`;
}

/** The first document: everything but the reload's second half. */
async function firstDocument(): Promise<void> {
  await until('the engine device', () => engineDevice() !== null, 90);
  // host_init has given this document its epoch, without which the native router takes no note.
  await until('the plugin host', () => nativeHostReady(), 90);
  const device = engineDevice()!;
  if (device.backend !== 'Wasapi') return log(`FAIL the device is ${device.backend}: this probe runs on WASAPI only`);
  // Two built-in synths, slot A active, an empty looper; nothing sounds yet.
  if (looper.masterLengthFrames() !== 0) return log('FAIL the looper holds audio in a profile of its own: refusing to play');
  selectSynth(1, 'bass');
  selectSynth(0, 'lead');
  let safe: string;
  try {
    safe = await silence();
  } catch (e) {
    return log(`FAIL silence not proven, no note played: ${e instanceof Error ? e.message : String(e)}`);
  }
  log(`device: ${device.backend} ${device.outputName}, ${device.sampleRate} Hz; ${safe}`);
  const el = document.activeElement;
  if (el instanceof HTMLElement && el !== document.body) el.blur();
  expect(document.querySelector('.kb__key') !== null, 'the on-screen piano is not shown');

  await check(1, checkResync);
  await check(2, checkDeny);
  await check(3, checkNote);
  await check(4, checkOwners);
  await check(5, checkBlur);
  await check(7, checkSlotSwitch);

  // Check 6, first half: a key down, then this document goes quiet and reloads.
  try {
    key('keydown', 'f');
    await until('note 65 held before the reload', () => heldNotes().has(65), 5);
    await settle();
  } catch (e) {
    results.failed++;
    results.lines.push(`check 6 FAIL: ${e instanceof Error ? e.message : String(e)}`);
    log(results.lines.at(-1));
    letGo();
    await settle();
    return verdict();
  }
  frozen = true;
  sessionStorage.setItem(STATE_KEY, JSON.stringify(results));
  log('reloading the WebView with note 65 held (key:KeyF); this document sends nothing more');
  location.reload();
}

/** The reloaded document: check 6's second half, then the verdict. */
async function secondDocument(saved: Saved): Promise<void> {
  sessionStorage.removeItem(STATE_KEY);
  results.lines = saved.lines;
  results.failed = saved.failed;
  await check(6, async () => {
    await until('the engine device after the reload', () => engineDevice() !== null, 60);
    await until('the plugin host after the reload (host_init: the new epoch)', () => nativeHostReady(), 60);
    const safe = await silence();
    const events = await midiResync();
    const text = resyncText(events);
    const notes = first(events, 'held')?.notes ?? [];
    expect(notes.length === 0, `the new listener's resync holds [${notes.join(',')}]: the old document's note 65 was not released`);
    await settle();
    expect(heldNotes().size === 0 && !lit(65), `held ${heldText()}, note 65 ${lit(65) ? 'lit' : 'dark'}`);
    const from = sent.length;
    key('keydown', 'a');
    await until('note 60 held in the new document', () => heldNotes().has(60), 5);
    key('keyup', 'a');
    await until('note 60 released in the new document', () => heldNotes().size === 0, 5);
    const ons = notesFrom(from);
    expect(ons.length === 2, `the new document's key sent ${JSON.stringify(ons)}`);
    return `after the reload (${safe}): new listener's ${text}; note 65 dark; a key in the new document holds [60] and lets go (its epoch is the router's current one)`;
  });
  verdict();
}

function verdict(): void {
  if (results.failed === 0) log(`complete: ${results.lines.length} checks passed (1-7; check 8 is the runner's log check)`);
  else log(`FAIL ${results.failed} of ${results.lines.length} check(s) failed: ${results.lines.filter((l) => l.includes(' FAIL: ')).map((l) => l.replace(/:.*/, '')).join(', ')}`);
}

export async function runMidiSwitch(): Promise<void> {
  watchInput();
  let saved: Saved | null = null;
  try {
    const raw = sessionStorage.getItem(STATE_KEY);
    saved = raw === null ? null : (JSON.parse(raw) as Saved);
  } catch {
    saved = null;
  }
  try {
    if (saved) await secondDocument(saved);
    else await firstDocument();
  } catch (e) {
    letGo();
    log(`FAIL ${e instanceof Error ? e.message : String(e)}`);
  }
}
