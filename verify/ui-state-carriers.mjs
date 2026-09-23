/** Accessibility and non-colour state carriers against the rendered app.
 * Run against Vite: node verify/ui-state-carriers.mjs --url=http://localhost:1420
 */
import assert from 'node:assert/strict';
import { chromium } from 'playwright';

const url = process.argv.find((arg) => arg.startsWith('--url='))?.slice(6) ?? 'http://localhost:1420';
const browser = await chromium.launch({ args: ['--autoplay-policy=no-user-gesture-required'] });

try {
  const page = await browser.newPage({ viewport: { width: 1280, height: 820 } });
  await page.goto(url);
  await page.waitForFunction(() => !!window.__lf);

  const firstTakeTransport = await page.evaluate(async () => {
    const lf = window.__lf;
    await lf.looper.init();
    await lf.looper.recDub(0);
    const button = document.querySelector('button[aria-label="Stop all tracks"]');
    return {
      state: lf.looper.stateOf(0),
      disabled: button?.disabled,
      text: button?.textContent?.trim(),
    };
  });
  assert.equal(firstTakeTransport.state, 'RECORDING');
  assert.equal(firstTakeTransport.disabled, false);
  assert.equal(firstTakeTransport.text, '■ ALL');

  await page.evaluate(() => window.__lf.looper.stop(0));
  await page.waitForFunction(() => window.__lf.looper.stateOf(0) === 'EMPTY');

  await page.evaluate(async () => {
    const lf = window.__lf;
    const { defaultFxStates } = await import('/src/audio/fx/fx.ts');
    const frames = Math.round(lf.engine.ctx.sampleRate * 0.8); // one bar at 300 BPM
    const tracks = [0, 2].map((index) => ({
      index,
      pcm: new Float32Array(frames).fill((index + 1) * 0.05),
      volume: 1,
      muted: false,
      reversed: false,
      state: index === 0 ? 'PLAYING' : 'STOPPED',
      fx: defaultFxStates(),
    }));
    lf.looper.setFixedLengthEnabled(false);
    lf.looper.setRetakeEnabled(false);
    lf.looper.setLoopEndStopEnabled(false);
    await lf.looper.loadSession({ bpm: 300, bars: 1, masterLengthFrames: frames, tracks });
    await lf.looper.recDub(1);
  });
  await page.waitForFunction(() => window.__lf.looper.stateOf(1) === 'PLAYING', undefined, { timeout: 6000 });
  const liveText = await page.locator('.lp [aria-live]').textContent();
  assert.match(liveText ?? '', /Track 2 take recorded/);

  await page.evaluate(async () => {
    const lf = window.__lf;
    await lf.ensureActive();
    lf.inputRouter.handle({ type: 'on', note: 60, velocity: 100, source: 'probe' });
  });
  await page.waitForTimeout(300);
  assert.match(await page.locator('[role="meter"]').getAttribute('aria-valuetext'), /-?\d+ dBFS/);
  await page.evaluate(() =>
    window.__lf.inputRouter.handle({ type: 'off', note: 60, velocity: 0, source: 'probe' }),
  );

  assert.equal(await page.locator('.cmd__lamp').getAttribute('role'), 'img');

  await page.evaluate(async () => {
    const { setAutoDismissMsForProbe } = await import('/src/notify.ts');
    setAutoDismissMsForProbe(1000);
    window.__lf.notify.notifyError('Probe error', 'detail');
  });
  const toast = page.locator('.toast').filter({ hasText: 'Probe error' });
  await toast.waitFor();
  assert.ok((await toast.locator('.toast__body').textContent())?.trim().startsWith('Error'));
  await toast.locator('.toast__close').focus();
  await page.waitForTimeout(2500);
  assert.equal(await toast.count(), 1, 'focused toast must outlive its auto-dismiss timer');
  await page.evaluate(async () => {
    const { setAutoDismissMsForProbe } = await import('/src/notify.ts');
    for (const item of window.__lf.notify.toasts()) window.__lf.notify.dismissToast(item.id);
    setAutoDismissMsForProbe(8000);
  });

  await page.evaluate(() => window.__lf.looper.playStop(0));
  await page.waitForFunction(() => window.__lf.looper.stateOf(0) === 'STOPPED');
  const stoppedCore = page.locator('.lp-lane[aria-label="Track 1"] .lp-core');
  assert.match(await stoppedCore.getAttribute('aria-label'), /play first to overdub/);
  assert.equal(await stoppedCore.getAttribute('title'), 'play first to overdub');

  // Refusal gate (src/ui/looper/gates.ts): a refused Space says the lane button's reason on the
  // looper status line instead of a silent no-op, and changes no state.
  const live = page.locator('.lp__sr-status');
  await page.evaluate(() => {
    window.__lf.looper.selectTrack(0);
    document.activeElement?.blur?.();
  });
  await page.keyboard.press('Space');
  await page.waitForTimeout(200);
  assert.equal(await page.evaluate(() => window.__lf.looper.stateOf(0)), 'STOPPED', 'refused Space must not change state');
  assert.equal((await live.textContent())?.trim(), 'Track 1: play first to overdub');

  // A refused Enter on an EMPTY lane (nothing to play) says so, and changes no state.
  await page.evaluate(() => window.__lf.looper.clearAll());
  await page.waitForFunction(() => [0, 1, 2, 3, 4].every((i) => window.__lf.looper.stateOf(i) === 'EMPTY'));
  await page.keyboard.press('Enter');
  await page.waitForTimeout(200);
  assert.equal(await page.evaluate(() => window.__lf.looper.stateOf(0)), 'EMPTY', 'refused Enter must not change state');
  assert.equal((await live.textContent())?.trim(), 'Track 1: nothing to play, record first');

  // Gate ok: an EMPTY selected lane still arms on Space.
  await page.keyboard.press('Space');
  await page.waitForFunction(() => window.__lf.looper.stateOf(0) === 'RECORDING', undefined, { timeout: 3000 });

  // While lane 1 records, lane 2's core is refused, and its hover and label say why.
  const otherCore = page.locator('.lp-lane[aria-label="Track 2"] .lp-core');
  assert.equal(await otherCore.isDisabled(), true);
  assert.equal(await otherCore.getAttribute('title'), 'another track is recording, stop it first');
  assert.equal(await otherCore.getAttribute('aria-label'), 'Track 2 another track is recording, stop it first');
  await page.evaluate(() => window.__lf.looper.stop(0));
  await page.waitForFunction(() => window.__lf.looper.stateOf(0) === 'EMPTY');

  console.log('=== RESULT: ui-state-carriers passed ===');
} finally {
  await browser.close();
}
