/**
 * Real pointer/key gestures against the rendered app, in engine mode on the web engine fake
 * (`src/platform/host.web.ts`, the `engine-seam` pattern: `window.__lfEngineFake` set before the app
 * loads, a reset frame, the commands the UI sends read from `__lf.native.sent`):
 *
 * - the BPM field's commit rules: Escape cancels (nothing sent), Enter commits (`SetBpm`), a blur commits;
 * - pointer capture across octave changes: a held on-screen key released off the keyboard after an
 *   octave shift ends its note (`NoteOff` for the pitch it started);
 * - independent note ownership (`src/ui/state/input-router.ts`): another owner's held pitch survives a
 *   pointer's own release of it;
 * - the playable upper note range: four octaves up the keyboard ends at 127 and every key is 0..127.
 *
 * Cannot see the native engine (the notes it plays: lf-engine `tests/synth.rs`), hardware or MIDI
 * devices: the fake answers no command by itself. Run: pnpm probe input-controls
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
    await page.getByRole('button', { name: 'BPM', exact: true }).click();
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

  const pointerDown = async note => {
    const box = await page.locator(`.kb__key[data-note="${note}"]`).boundingBox();
    await page.mouse.move(box.x + box.width / 2, box.y + box.height - 8);
    await page.mouse.down();
    await page.waitForFunction(n => window.__lf.inputRouter.held.has(n), note);
  };
  await clearSent();
  await pointerDown(60);
  await page.keyboard.press('x');
  await page.mouse.move(30, 30);
  await page.mouse.up();
  await page.waitForFunction(() => window.__lf.inputRouter.held.size === 0);
  const held60 = await sent();
  console.log('pointer hold across an octave shift', JSON.stringify(held60.filter((c) => c.NoteOn || c.NoteOff !== undefined)));
  assert.ok(held60.some((c) => c.NoteOn?.[0] === 60), 'the pointer press plays its note');
  assert.deepEqual(held60.filter((c) => c.NoteOff !== undefined).at(-1), { NoteOff: 60 }, 'its release ends the note it started');

  // Ending a pointer hold must not end another producer's ownership of the same pitch.
  await page.evaluate(() => window.__lf.inputRouter.handle({
    type: 'on', note: 72, velocity: 100, source: 'midi', owner: 'probe-controller',
  }));
  await pointerDown(72);
  await page.keyboard.press('x');
  await page.mouse.move(30, 30);
  await page.mouse.up();
  assert.deepEqual(await page.evaluate(() => [...window.__lf.inputRouter.held]), [72]);
  await page.evaluate(() => window.__lf.inputRouter.handle({
    type: 'off', note: 72, velocity: 0, source: 'midi', owner: 'probe-controller',
  }));
  await page.waitForFunction(() => window.__lf.inputRouter.held.size === 0);

  for (let i = 0; i < 4; i++) await page.getByRole('button', { name: 'Octave up', exact: true }).click();
  const notes = await page.locator('.kb__key').evaluateAll(keys => keys.map(k => Number(k.dataset.note)));
  assert.equal(notes.at(-1), 127);
  assert.ok(notes.every(note => note >= 0 && note <= 127));
  await pointerDown(127);
  await page.mouse.up();
  await page.waitForFunction(() => window.__lf.inputRouter.held.size === 0);
  assert.deepEqual(consoleErrors, [], 'no console errors');
});
