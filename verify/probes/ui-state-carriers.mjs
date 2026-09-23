/** Accessibility and non-colour state carriers against the rendered app: transport, meter, lamp,
 * looper-announcement and toast state carriers, plus the looper refusal gates (a refused lane
 * core's title/label reason, and a refused Space/Enter announcing that reason with no state change).
 * Toggles: every command-bar and lane toggle keeps ONE accessible name in both states and carries its
 * state in aria-pressed alone; the lane core, whose name says the action, carries no aria-pressed. The
 * mic toggle arms through a substituted device open, so it says nothing about a real input device.
 * Run: pnpm probe ui-state-carriers
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

/**
 * Presses the toggle named `name` twice with real clicks and returns each step: how many buttons carry
 * that exact name, the state it drives (`state`, an expression read through __lf, never the DOM) and
 * its aria-pressed. A stable toggle shows one button in every step, aria-pressed equal to the state,
 * and a state that flips and flips back.
 */
async function pressToggle(page, name, state) {
  const button = page.getByRole('button', { name, exact: true });
  const steps = [];
  for (let press = 0; press <= 2; press++) {
    if (press > 0) {
      if ((await button.count()) !== 1) break;
      const before = await page.evaluate(state);
      const clicked = await button.click({ timeout: 3000 }).then(() => true, () => false);
      if (!clicked) break;
      await page.waitForFunction(`(${state}) !== ${before}`, undefined, { timeout: 3000 }).catch(() => {});
    }
    const count = await button.count();
    steps.push({ count, state: await page.evaluate(state), pressed: count === 1 ? await button.getAttribute('aria-pressed') : null });
  }
  const ok = steps.length === 3 && steps.every((s) => s.count === 1 && s.pressed === String(s.state))
    && steps[1].state !== steps[0].state && steps[2].state === steps[0].state;
  console.log(JSON.stringify({ toggle: name, ok, steps }));
  return ok;
}

await probe(async ({ open }) => {
  const { page } = await open({ viewport: { width: 1280, height: 820 } });

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
  // The core's name says the action ("Track 1 stop recording"), so a pressed state would contradict it.
  const recordingCorePressed = await page.locator('.lp-lane[aria-label="Track 1"] .lp-core').getAttribute('aria-pressed');
  console.log(JSON.stringify({ recordingCorePressed }));
  await page.evaluate(() => window.__lf.looper.stop(0));
  await page.waitForFunction(() => window.__lf.looper.stateOf(0) === 'EMPTY');

  // Toggles: one stable name, state in aria-pressed (the END STOP pattern). No loop yet, so the tempo
  // is unlocked and AUTO REC is enabled.
  assert.equal(await page.evaluate(() => window.__lf.clock.bpmLocked()), false);
  await page.evaluate(() => {
    const lf = window.__lf;
    window.__realInputOpen = lf.platform.audioInput.open;
    lf.platform.audioInput.open = async () => ({ node: lf.engine.ctx.createGain(), sampleRate: lf.engine.ctx.sampleRate, close() {} });
  });
  const unstable = [];
  for (const [name, state] of [
    ['Metronome click', 'window.__lf.clock.metronomeOn()'],
    ['Fixed take length', 'window.__lf.looper.fixedLengthEnabled()'],
    ['Retake', 'window.__lf.looper.retakeEnabled()'],
    ['Auto record', 'window.__lf.looper.autoRecordEnabled()'],
    ['Mic / line input', 'window.__lf.looper.inputArmed()'],
    ['Mute master', 'window.__lf.master.muted()'],
  ]) if (!(await pressToggle(page, name, state))) unstable.push(name);
  await page.evaluate(() => { window.__lf.platform.audioInput.open = window.__realInputOpen; });

  await page.evaluate(async () => {
    const lf = window.__lf;
    const { defaultFxStates } = await import('/src/audio/fx/fx.ts');
    const frames = Math.round(lf.engine.ctx.sampleRate * 0.8);
    await lf.looper.loadSession({ bpm: 300, bars: 1, masterLengthFrames: frames, tracks: [{ index: 0,
      pcm: new Float32Array(frames).fill(0.05), volume: 1, muted: false, reversed: false, state: 'STOPPED', fx: defaultFxStates() }] });
  });
  await page.waitForFunction(() => window.__lf.looper.stateOf(0) === 'STOPPED');
  for (const [name, state] of [
    ['Track 1 mute', 'window.__lf.looper.trackMuted(0)'],
    ['Track 1 reverse', 'window.__lf.looper.trackInfo(0).reversed'],
  ]) if (!(await pressToggle(page, name, state))) unstable.push(name);
  assert.deepEqual(unstable, [], 'these toggles change their name with their state or lose aria-pressed');
  assert.equal(recordingCorePressed, null, 'the lane core names its action; it must not also claim a pressed state');
});
