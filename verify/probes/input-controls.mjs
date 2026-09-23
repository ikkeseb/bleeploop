// Real pointer/key gestures against the rendered app, including independent MIDI ownership.
import assert from 'node:assert/strict';
import { chromium } from 'playwright';

const url = process.argv.find(arg => arg.startsWith('--url='))?.slice(6) ?? 'http://localhost:1420';
const browser = await chromium.launch({ args: ['--autoplay-policy=no-user-gesture-required'] });
try {
  const page = await browser.newPage({ viewport: { width: 1280, height: 820 } });
  const errors = [];
  page.on('pageerror', error => errors.push(String(error)));
  await page.goto(url);
  await page.waitForFunction(() => !!window.__lf);
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
  assert.deepEqual(errors, []);
  console.log('PASS: BPM cancel/commit, pointer capture across octave changes, MIDI ownership, and playable upper range');
} finally {
  await browser.close();
}
