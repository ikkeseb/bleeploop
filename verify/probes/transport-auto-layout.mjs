/**
 * Drives the rendered command bar and lane through AUTO REC's toggle and sensitivity slider at
 * seven desktop widths (960..1920 px) and asserts the command-bar height and lane top never move:
 * AUTO must not reflow the stage when switched on/off or dragged end-to-end. Screenshots the 1730 px
 * case before and after enabling. Proves layout stability only; it says nothing about auto-record
 * detection itself (that is the AUTO REC section of golden-jam) or anything below the DOM.
 * Run: pnpm probe transport-auto-layout
 */
import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { probe } from '../harness/probe.ts';

await probe(async ({ open }) => {
  await mkdir('logs/layout', { recursive: true });
  const failures = [];
  for (const width of [960, 1280, 1400, 1500, 1600, 1730, 1920]) {
    const { page } = await open({ viewport: { width, height: 900 } });
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
