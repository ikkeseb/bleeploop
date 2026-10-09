/**
 * Real pointer/key gestures against the rendered app, in engine mode on the web engine fake
 * (`src/platform/host.web.ts`, the `engine-seam` pattern: `window.__lfEngineFake` set before the app
 * loads, a reset frame, the commands the UI sends read from `__lf.native.sent` and the notes it sends
 * native MIDI's router from `__lf.native.inputSent`):
 *
 * - the BPM field's commit rules: Escape cancels (nothing sent), Enter commits (`SetBpm`), a blur commits;
 * - pointer capture across octave changes: a held on-screen key released off the keyboard after an
 *   octave shift lets go of the pitch it started, under its own owner (`pointer:<id>`);
 * - the highlight is this keyboard's own holds united with native MIDI's held set: a pitch native MIDI
 *   reports held (another owner's) stays lit after the pointer's own release, until native MIDI says it
 *   is up;
 * - the playable upper note range: four octaves up the keyboard ends at 127 and every key is 0..127.
 *
 * Note ownership itself (two owners on one pitch, which release sends `NoteOff`) is native MIDI's router,
 * proven in Rust (`src-tauri/src/engine_io/midi/router.rs` tests). Cannot see the native engine (the notes
 * it plays: lf-engine `tests/synth.rs`), hardware or MIDI devices: the fake answers no command by itself.
 * Run: pnpm probe input-controls
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

const EMPTY = { state: 'Empty', length: 0, armed: false, autoArmed: false, canUndo: false, canReverse: false, reversed: false, stopAt: null, fading: false, retakePass: 0 };

await probe(async ({ open }) => {
  const { page, consoleErrors } = await open({
    viewport: { width: 1280, height: 820 },
    init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)),
  });
  await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });
  await page.evaluate((f) => window.__lf.native.emit(f), {
    seq: 1,
    reset: true,
    settings: [],
    events: [
      ...[0, 1, 2, 3, 4].map((lane) => ({ Lane: { frame: 0, lane, info: EMPTY } })),
      { Transport: { frame: 0, master: 0, bpm: 120, locked: false } },
      { Selected: { frame: 0, lane: 0 } },
    ],
    anchor: { frame: 0, atMs: Date.now(), rate: 48000, grid: 0 },
    meter: { peak: 0, clip: false },
  });
  const sent = () => page.evaluate(() => window.__lf.native.sent.slice());
  const clearSent = () => page.evaluate(() => void (window.__lf.native.sent.length = 0));
  const bpmSent = async () => (await sent()).filter((c) => c.SetBpm !== undefined).map((c) => c.SetBpm);

  const initialBpm = await page.evaluate(() => window.__lf.clock.bpm());
  const edit = async value => {
    await page.getByRole('button', { name: /^\d+ BPM$/ }).click();
    await page.locator('.transport__bpm-input').fill(String(value));
  };
  await clearSent();
  await edit(95);
  await page.keyboard.press('Escape');
  await page.waitForTimeout(100);
  assert.deepEqual(await bpmSent(), [], 'Escape cancels BPM');
  assert.equal(await page.evaluate(() => window.__lf.clock.bpm()), initialBpm, 'Escape cancels BPM');
  await edit(96);
  await page.keyboard.press('Enter');
  await page.waitForTimeout(100);
  assert.deepEqual(await bpmSent(), [96], 'Enter commits BPM');
  await clearSent();
  await edit(97);
  await page.getByRole('button', { name: 'Octave up', exact: true }).click();
  await page.waitForTimeout(100);
  assert.deepEqual(await bpmSent(), [97], 'Blur commits BPM');
  await page.getByRole('button', { name: 'Octave down', exact: true }).click();

  /** What the keyboard sent native MIDI's router since `from`: its notes, as `{ owner, note, velocity, on }`. */
  const notesSince = (from) => page.evaluate((m) => window.__lf.native.inputSent.slice(m).filter((e) => e.note).map((e) => e.note), from);
  const inputMark = () => page.evaluate(() => window.__lf.native.inputSent.length);
  const lit = (note) => page.evaluate((n) => !!document.querySelector(`.kb__key[data-note="${n}"].kb__key--down`), note);
  const pointerDown = async note => {
    const box = await page.locator(`.kb__key[data-note="${note}"]`).boundingBox();
    await page.mouse.move(box.x + box.width / 2, box.y + box.height - 8);
    await page.mouse.down();
    await page.waitForFunction(n => !!document.querySelector(`.kb__key[data-note="${n}"].kb__key--down`), note);
  };
  /** Wait until the keyboard sent a release of `note` since `from`. */
  const released = (from, note) =>
    page.waitForFunction(([m, n]) => window.__lf.native.inputSent.slice(m).some((e) => e.note?.note === n && !e.note.on), [from, note]);

  let from = await inputMark();
  await pointerDown(60);
  await page.keyboard.press('x');
  await page.mouse.move(30, 30);
  await page.mouse.up();
  await released(from, 60);
  const held60 = await notesSince(from);
  console.log('pointer hold across an octave shift', JSON.stringify(held60));
  const press = held60.find((n) => n.on);
  assert.ok(press && press.note === 60 && /^pointer:\d+$/.test(press.owner), 'the pointer press plays its note under its pointer');
  assert.deepEqual(held60.filter((n) => !n.on), [{ owner: press.owner, note: 60, velocity: 0, on: false }], 'its release lets go of the note it started');

  // The keys light this keyboard's holds and every note native MIDI reports held: another owner's pitch
  // stays lit after the pointer's own release.
  await page.evaluate(() => window.__lf.native.midiEmit({ held: { notes: [72], changes: 1 } }));
  await page.waitForFunction(() => !!document.querySelector('.kb__key[data-note="72"].kb__key--down'));
  from = await inputMark();
  await pointerDown(72);
  await page.mouse.up();
  await released(from, 72);
  assert.equal(await lit(72), true, 'a note native MIDI holds stays lit after the pointer lets go');
  await page.evaluate(() => window.__lf.native.midiEmit({ held: { notes: [], changes: 2 } }));
  await page.waitForFunction(() => !document.querySelector('.kb__key--down'));

  for (let i = 0; i < 4; i++) await page.getByRole('button', { name: 'Octave up', exact: true }).click();
  const notes = await page.locator('.kb__key').evaluateAll(keys => keys.map(k => Number(k.dataset.note)));
  assert.equal(notes.at(-1), 127);
  assert.ok(notes.every(note => note >= 0 && note <= 127));
  from = await inputMark();
  await pointerDown(127);
  await page.mouse.up();
  await released(from, 127);
  assert.equal(await lit(127), false, 'the top key goes dark on release');
  assert.deepEqual(consoleErrors, [], 'no console errors');
});
