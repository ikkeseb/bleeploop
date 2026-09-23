/** AUTO must not move the stage when toggled or adjusted. Requires the browser rig. */
import assert from 'node:assert/strict';
import { chromium } from 'playwright';
import { mkdir } from 'node:fs/promises';

const url = process.argv.find(arg => arg.startsWith('--url='))?.slice(6) ?? 'http://localhost:1420';
const browser = await chromium.launch();
await mkdir('logs/layout', { recursive: true });
const failures = [];
try {
  for (const width of [960, 1280, 1400, 1500, 1600, 1730, 1920]) {
    const page = await browser.newPage({ viewport: { width, height: 900 } });
    await page.goto(url);
    await page.waitForFunction(() => !!window.__lf);
    await page.evaluate(() => window.__lf.looper.setAutoRecordEnabled(false));
    await page.waitForTimeout(150);
    const bounds = () => page.evaluate(() => ({
      bar: document.querySelector('.cmd').getBoundingClientRect().height,
      lane: document.querySelector('.lp-lane').getBoundingClientRect().top,
    }));
    const before = await bounds();
    if (width === 1730) await page.screenshot({ path: 'logs/layout/auto-off.png' });
    await page.getByRole('button', { name: 'Auto record off', exact: true }).click();
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
    await page.getByRole('button', { name: 'Auto record on', exact: true }).click();
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
} finally { await browser.close(); }
