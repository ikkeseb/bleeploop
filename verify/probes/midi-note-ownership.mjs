/**
 * The on-screen and PC keyboard as note sources for native MIDI's one router, on the web engine fake
 * (`src/platform/host.web.ts`, the engine-seam pattern): what the keyboard sends is read from
 * `__lf.native.inputSent` (`input.note(owner, …)`, `input.blur()`), what native MIDI holds is scripted
 * with `__lf.native.midiEmit`:
 *
 * - two pointers on one on-screen key are two owners: each press and each release goes out under its own
 *   `pointer:<id>`; the first release keeps the key lit (the other pointer's hold), the last unlights it;
 * - a PC key is owned by its physical key (`key:<code>`), and the note target goes out before the note
 *   (`ensureActive`'s `selectTarget`, then the note, in one order);
 * - native MIDI's held set lights and clears the same on-screen key (a MIDI controller's note);
 * - a window blur sends `blur` (native MIDI lets go of this document's pointers and keys) and drops the
 *   keyboard's own lit keys; that key's later key-up sends nothing;
 * - hiding the keyboard while a key is held releases it under its own owner (no blur).
 *
 * What moved to Rust with the router (`src-tauri/src/engine_io/midi/router.rs` tests): one port's or
 * channel's release never ending another's note, per-owner sustain (CC64), CC123, an unplugged port's
 * release, the wheels, two owners on one note sending one `NoteOn` and one `NoteOff`. Cannot see the native
 * engine (what a note sounds like: lf-engine `tests/synth.rs`, `tests/voices.rs`), real MIDI hardware or
 * Tauri IPC. Run: pnpm probe midi-note-ownership
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

await probe(async ({ open }) => {
  const { page, consoleErrors } = await open({
    init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)),
  });
  await page.waitForFunction(() => window.__lf.native.opened.length === 1);
  await page.evaluate(() => {
    const lane = { state: 'Empty', length: 0, armed: false, autoArmed: false, canUndo: false, canReverse: false, reversed: false, stopAt: null, fading: false, retakePass: 0 };
    window.__lf.native.emit({
      seq: 1,
      reset: true,
      settings: [],
      events: [...[0, 1, 2, 3, 4].map((i) => ({ Lane: { frame: 0, lane: i, info: lane } })),
        { Transport: { frame: 0, master: 0, bpm: 120, locked: false } }, { Selected: { frame: 0, lane: 0 } }],
      anchor: { frame: 0, atMs: Date.now(), rate: 48000, grid: 0 },
      meter: { peak: 0, clip: false },
    });
  });

  const result = await page.evaluate(async () => {
    const lf = window.__lf;
    lf.selectSynth(0, 'organ'); lf.setActiveSlot(0);
    const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
    await pause(50);
    /** What the keyboard sent since the last call: notes as 'on:60@pointer:901', blurs, note targets. */
    let seen = lf.native.inputSent.length;
    const sent = async () => {
      await pause(10); // the outbox flushes on a microtask
      const all = lf.native.inputSent.slice(seen);
      seen = lf.native.inputSent.length;
      return all.map((e) => (e === 'blur' ? 'blur' : e.note ? `${e.note.on ? 'on' : 'off'}:${e.note.note}@${e.note.owner}` : e.selectTarget ? `target:${JSON.stringify(e.selectTarget.target)}` : JSON.stringify(e)));
    };
    const key = document.querySelector('.kb__key[data-note="60"]');
    if (!key) throw new Error('the probe needs the visible keyboard');
    const lit = () => key.classList.contains('kb__key--down');

    // Two pointers on one key.
    const capture = key.setPointerCapture;
    key.setPointerCapture = () => {}; // synthetic events cannot acquire browser-native pointer capture
    const pointer = (type, pointerId) => key.dispatchEvent(new PointerEvent(type, { bubbles: true, pointerId, pointerType: 'touch' }));
    pointer('pointerdown', 901); pointer('pointerdown', 902); await pause(30);
    const pointersDown = { sent: (await sent()).filter((s) => !s.startsWith('target:')), lit: lit() };
    pointer('pointerup', 901); await pause(30);
    const firstPointerUp = { sent: await sent(), lit: lit() };
    pointer('pointerup', 902); await pause(30);
    const lastPointerUp = { sent: await sent(), lit: lit() };
    key.setPointerCapture = capture;

    // Native MIDI's held set lights the same key (a controller's note), and clears it.
    lf.native.midiEmit({ held: { notes: [60], changes: 1 } }); await pause(30);
    const midiDown = lit();
    lf.native.midiEmit({ held: { notes: [], changes: 2 } }); await pause(30);
    const midiUp = lit();
    return { pointersDown, firstPointerUp, lastPointerUp, midiDown, midiUp };
  });
  console.log(JSON.stringify(result));
  assert.deepEqual(result.pointersDown.sent.map((s) => s.replace(/^on:60@/, '')), ['pointer:901', 'pointer:902'], 'each pointer presses under its own owner');
  assert.equal(result.pointersDown.lit, true, 'the key lights at the press');
  assert.deepEqual(result.firstPointerUp, { sent: ['off:60@pointer:901'], lit: true }, 'the first release is its own owner\'s, and the key stays lit');
  assert.deepEqual(result.lastPointerUp, { sent: ['off:60@pointer:902'], lit: false }, 'the last release unlights the key');
  assert.deepEqual([result.midiDown, result.midiUp], [true, false], 'native MIDI\'s held set lights and clears the key');

  // A PC key: the note target, then the note, under its physical key.
  const mark = () => page.evaluate(() => window.__lf.native.inputSent.length);
  const since = (from) => page.evaluate((m) => window.__lf.native.inputSent.slice(m), from);
  await page.evaluate(() => document.activeElement instanceof HTMLElement && document.activeElement.blur());
  let from = await mark();
  await page.keyboard.down('a');
  await page.waitForFunction((m) => window.__lf.native.inputSent.slice(m).some((e) => e.note?.on), from);
  const pressed = await since(from);
  console.log('PC key', JSON.stringify(pressed));
  const noteAt = pressed.findIndex((e) => e.note);
  assert.deepEqual(pressed[noteAt - 1], { selectTarget: { slot: 0, target: { Builtin: 'organ' } } }, 'the active slot is routed before the note');
  assert.equal(pressed[noteAt].note.owner, 'key:KeyA', 'a PC key is owned by its physical key');
  const heldNote = pressed[noteAt].note.note;
  const keyLit = () => page.evaluate((n) => !!document.querySelector(`.kb__key[data-note="${n}"].kb__key--down`), heldNote);
  assert.equal(await keyLit(), true, 'the PC key lights its on-screen key');

  // A window blur: `blur` goes out, the lit key goes dark, and the later key-up sends nothing.
  from = await mark();
  await page.evaluate(() => window.dispatchEvent(new Event('blur')));
  await page.waitForFunction((m) => window.__lf.native.inputSent.length > m, from);
  assert.deepEqual(await since(from), ['blur'], 'a window blur sends blur, and no per-note release');
  assert.equal(await keyLit(), false, 'a blur drops the keyboard\'s own lit keys');
  from = await mark();
  await page.keyboard.up('a');
  await page.waitForTimeout(50);
  assert.deepEqual(await since(from), [], 'the key-up after a blur sends nothing');

  // Hiding the keyboard while a key is held releases it under its owner.
  await page.keyboard.down('s');
  await page.waitForFunction(() => window.__lf.native.inputSent.some((e) => e.note?.on && e.note.owner === 'key:KeyS'));
  from = await mark();
  await page.evaluate(() => window.__lf.layoutStore.setKeyboardPlacement('hidden'));
  await page.locator('.kb').waitFor({ state: 'detached' });
  await page.waitForFunction((m) => window.__lf.native.inputSent.length > m, from);
  const hidden = await since(from);
  console.log('hidden', JSON.stringify(hidden));
  assert.deepEqual(hidden.map((e) => e.note && { owner: e.note.owner, on: e.note.on }), [{ owner: 'key:KeyS', on: false }], 'hiding the keyboard releases its hold under its owner');
  await page.keyboard.up('s');
  await page.evaluate(() => window.__lf.layoutStore.setKeyboardPlacement('bottom'));
  assert.deepEqual(consoleErrors, [], 'no console errors');
});
