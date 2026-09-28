/**
 * Drives the rendered command bar and lane through AUTO REC's toggle and sensitivity slider at
 * seven desktop widths (960..1920 px), in engine mode on the web engine fake (`window.__lfEngineFake`
 * set before the app loads, a reset frame with five EMPTY lanes), and asserts the command-bar height and
 * lane top never move: AUTO must not reflow the stage when switched on/off or dragged end-to-end.
 * Screenshots the 1730 px case before and after enabling. Proves layout stability only; it says nothing
 * about auto-record detection itself (the engine's: lf-engine `tests/auto_record.rs`) or anything below
 * the DOM. Run: pnpm probe transport-auto-layout
 */
import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { probe } from '../harness/probe.ts';

const EMPTY = { state: 'Empty', length: 0, armed: false, autoArmed: false, canUndo: false, canReverse: false, reversed: false, stopAt: null, fading: false, retakePass: 0 };

await probe(async ({ open }) => {
  await mkdir('logs/layout', { recursive: true });
  const failures = [];
  for (const width of [960, 1280, 1400, 1500, 1600, 1730, 1920]) {
    const { page } = await open({
      viewport: { width, height: 900 },
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
    await page.evaluate(() => window.__lf.looper.setAutoRecordEnabled(false));
    await page.waitForTimeout(150);
    const bounds = () => page.evaluate(() => ({
      bar: document.querySelector('.cmd').getBoundingClientRect().height,
      lane: document.querySelector('.lp-lane').getBoundingClientRect().top,
    }));
    const before = await bounds();
    if (width === 1730) await page.screenshot({ path: 'logs/layout/auto-off.png' });
    const auto = page.getByRole('button', { name: 'Auto record', exact: true });
    assert.equal(await auto.getAttribute('aria-pressed'), 'false');
    await auto.click();
    const slider = page.getByRole('slider', { name: 'Auto record sensitivity', exact: true });
    await slider.waitFor({ state: 'visible' });
    await slider.focus();
    await page.keyboard.press('End');
    assert.equal(await slider.inputValue(), '100');
    await page.waitForTimeout(150);
    const enabled = await bounds();
    if (width === 1730) await page.screenshot({ path: 'logs/layout/auto-on.png' });
    await page.keyboard.press('Home');
    assert.equal(await slider.inputValue(), '1');
    await page.waitForTimeout(150);
    const adjusted = await bounds();
    assert.equal(await auto.getAttribute('aria-pressed'), 'true');
    await auto.click();
    await page.waitForTimeout(150);
    assert.equal(await slider.isVisible(), false);
    const disabled = await bounds();
    const pass = [enabled, adjusted, disabled].every(value =>
      Math.abs(value.bar - before.bar) < 0.5 && Math.abs(value.lane - before.lane) < 0.5);
    console.log(JSON.stringify({ width, before, enabled, adjusted, disabled, pass }));
    if (!pass) failures.push(width);
    await page.close();
  }
  assert.deepEqual(failures, [], 'AUTO moved the stage at these viewport widths');
}, { launch: {} });
