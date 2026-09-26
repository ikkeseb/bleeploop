/**
 * Real pointer/key gestures against the rendered app: no IN FX control in web mode (the input sends are
 * engine-only), BPM field Escape/Enter/blur commit rules,
 * pointer capture across octave changes, independent MIDI ownership (another owner's held pitch
 * survives a pointer's own release), and the playable upper note range. No hardware/native claims.
 * Run: pnpm probe input-controls
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

await probe(async ({ open }) => {
  const { page } = await open({ viewport: { width: 1280, height: 820 } });
  // The input sends live in the native engine alone: the web path renders no IN FX control.
  await page.getByRole('button', { name: 'Mic / line input' }).waitFor();
  assert.equal(await page.getByRole('button', { name: 'Input effects' }).count(), 0, 'no IN FX in web mode');
  assert.equal(await page.locator('.infx__pill').count(), 0, 'no IN FX pill in web mode');
  const initialBpm = await page.evaluate(() => window.__lf.clock.bpm());
  const edit = async value => {
    await page.getByRole('button', { name: 'BPM', exact: true }).click();
    await page.locator('.transport__bpm-input').fill(String(value));
  };
  await edit(95);
  await page.keyboard.press('Escape');
  assert.equal(await page.evaluate(() => window.__lf.clock.bpm()), initialBpm, 'Escape cancels BPM');
  await edit(96);
  await page.keyboard.press('Enter');
  assert.equal(await page.evaluate(() => window.__lf.clock.bpm()), 96, 'Enter commits BPM');
  await edit(97);
  await page.getByRole('button', { name: 'Octave up', exact: true }).click();
  assert.equal(await page.evaluate(() => window.__lf.clock.bpm()), 97, 'Blur commits BPM');
  await page.getByRole('button', { name: 'Octave down', exact: true }).click();

  const pointerDown = async note => {
    const box = await page.locator(`.kb__key[data-note="${note}"]`).boundingBox();
    await page.mouse.move(box.x + box.width / 2, box.y + box.height - 8);
    await page.mouse.down();
    await page.waitForFunction(n => window.__lf.inputRouter.held.has(n), note);
  };
  await pointerDown(60);
  await page.keyboard.press('x');
  await page.mouse.move(30, 30);
  await page.mouse.up();
  await page.waitForFunction(() => window.__lf.inputRouter.held.size === 0);

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
});
