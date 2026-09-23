/** Transport keys (Space/Enter/1–5) vs. focus: the transport yields only to controls the user is
 * operating, never to focus the app moved itself (popover panel, trigger focus return after Escape).
 * Run against Vite: node verify/transport-focus.mjs --url=http://localhost:1420
 */
import assert from 'node:assert/strict';
import { chromium } from 'playwright';

const url = process.argv.find((arg) => arg.startsWith('--url='))?.slice(6) ?? 'http://localhost:1420';
const shots = process.argv.find((arg) => arg.startsWith('--shots='))?.slice(8);
const browser = await chromium.launch({ args: ['--autoplay-policy=no-user-gesture-required'] });

try {
  const page = await browser.newPage({ viewport: { width: 1280, height: 820 } });
  await page.goto(url);
  await page.waitForFunction(() => !!window.__lf);
  await page.evaluate(async () => {
    await window.__lf.ensureActive();
    await window.__lf.looper.init();
  });

  const state = (i) => page.evaluate((idx) => window.__lf.looper.stateOf(idx), i);
  const reset = async () => {
    await page.evaluate(() => {
      for (let i = 0; i < 5; i++) window.__lf.looper.stop(i);
      window.__lf.looper.selectTrack(0);
    });
    await page.waitForFunction(() => window.__lf.looper.stateOf(0) === 'EMPTY');
  };
  const helpBtn = page.getByRole('button', { name: 'Keyboard & layout help' });
  const helpPanel = page.locator('#lf-help-popover');

  // 1. Pointer-open Help, Escape closes it, Space arms REC (not a popover re-open via the returned focus).
  await helpBtn.click();
  await helpPanel.waitFor();
  await page.keyboard.press('Escape');
  await helpPanel.waitFor({ state: 'detached' });
  await page.keyboard.press('Space');
  await page.waitForTimeout(150);
  assert.equal(await helpPanel.count(), 0, 'Space after Escape re-opened Help');
  assert.notEqual(await state(0), 'EMPTY', 'Space after Escape did not arm lane 1');
  await reset();

  // 2. Help open (pointer): Space arms lane 1 with Help still open; 2 selects lane 2.
  await helpBtn.click();
  await helpPanel.waitFor();
  await page.keyboard.press('Space');
  await page.waitForTimeout(150);
  assert.equal(await helpPanel.count(), 1, 'Help closed on Space');
  assert.notEqual(await state(0), 'EMPTY', 'Space with Help open did not arm lane 1');
  await page.keyboard.press('2');
  assert.equal(await page.evaluate(() => window.__lf.looper.selectedTrack()), 1, '2 with Help open did not select lane 2');
  if (shots) await page.screenshot({ path: `${shots}/help-open-armed.png` });
  await page.keyboard.press('Escape');
  await helpPanel.waitFor({ state: 'detached' });
  await reset();

  // 3. Real controls keep the key: Space in the BPM field does not arm; Tab to BPM plus + Enter
  //    activates that button, not PLAY/STOP.
  await page.getByRole('button', { name: 'BPM', exact: true }).click();
  // The field takes focus one task after it mounts (Transport.tsx startEditFocused): wait for the
  // focus, not the element, or Space lands on <body> and arms lane 1 — a race no hand can win.
  await page.locator('.transport__bpm-input').waitFor();
  await page.waitForFunction(() => document.activeElement?.classList.contains('transport__bpm-input'));
  await page.keyboard.press('Space');
  await page.waitForTimeout(150);
  assert.equal(await state(0), 'EMPTY', 'Space in the BPM input armed the looper');
  await page.keyboard.press('Tab');
  assert.equal(await page.evaluate(() => document.activeElement?.getAttribute('aria-label')), 'BPM plus');
  const bpmBefore = await page.evaluate(() => window.__lf.clock.bpm());
  await page.keyboard.press('Enter');
  await page.waitForTimeout(150);
  assert.equal(await page.evaluate(() => window.__lf.clock.bpm()), bpmBefore + 1, 'Enter on BPM plus did not step BPM');
  assert.equal(await state(0), 'EMPTY', 'Enter on a Tab-focused button drove the looper');

  console.log('transport-focus: PASS');
} finally {
  await browser.close();
}
