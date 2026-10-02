/**
 * Accessibility and non-colour state carriers against the rendered app, in engine mode on the web engine
 * fake (`src/platform/host.web.ts`, the `engine-seam` pattern: `window.__lfEngineFake` set before the app
 * loads, lane states scripted on the feed through `__lf.native.emit`, the commands the UI sends read from
 * `__lf.native.sent`):
 *
 * - transport, meter, lamp, looper-announcement and toast state carriers: ■ ALL during a first take, the
 *   record meter's dBFS text, the lamp's role, "Track 2 take recorded" on the live region, a focused
 *   toast outliving its auto-dismiss; the lamp across a lost device (amber and "no device open" while
 *   none runs, neutral and "running" once one runs again);
 * - the refusal carriers: a STOPPED lane core's title/label reason; the transport keys send the engine's
 *   action (the engine gates it now: lf-engine `tests/actions.rs`) and an engine refusal is announced on
 *   the looper status line with its lane and reason; while a lane records, another lane's core is
 *   disabled and its hover and label say why;
 * - toggles: each toggle it presses (CLICK, FIXED, RETAKE, AUTO, the master mute, lane 1's MUTE and REV)
 *   keeps ONE accessible name in both states and carries its state in aria-pressed alone; the lane core,
 *   whose name says the action, carries no aria-pressed. REV's state is the engine's: the probe echoes it
 *   on the feed as the engine would. Not pressed here: END STOP, lane FX and the keyboard show/hide cap
 *   (which still flips its name).
 *
 * Cannot see the native engine (its gates, what a take records, the meter's source): the fake answers no
 * command by itself, so every state the DOM shows was scripted.
 * Run: pnpm probe ui-state-carriers
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

const RATE = 48000;
const BAR = 2 * RATE; // one 4/4 bar at 120 BPM

const lane = (state, extra = {}) => ({
  state,
  length: 0,
  armed: false,
  autoArmed: false,
  canUndo: false,
  canReverse: false,
  reversed: false,
  stopAt: null,
  fading: false,
  retakePass: 0,
  ...extra,
});
const committed = (state, extra = {}) => lane(state, { length: BAR, canReverse: true, ...extra });
const laneEvent = (i, info) => ({ Lane: { frame: 0, lane: i, info } });
const transport = (master, locked) => ({ Transport: { frame: 0, master, bpm: 120, locked } });

/**
 * Presses the toggle named `name` twice with real clicks and returns each step: how many buttons carry
 * that exact name, the state it drives (`state`, an expression read through __lf, never the DOM) and
 * its aria-pressed. `echo(on)` scripts the engine's answer to a press when the state is the engine's. A
 * stable toggle shows one button in every step, aria-pressed equal to the state, and a state that flips
 * and flips back.
 */
async function pressToggle(page, name, state, echo) {
  const button = page.getByRole('button', { name, exact: true });
  const steps = [];
  for (let press = 0; press <= 2; press++) {
    if (press > 0) {
      if ((await button.count()) !== 1) break;
      const before = await page.evaluate(state);
      const clicked = await button.click({ timeout: 3000 }).then(() => true, () => false);
      if (!clicked) break;
      if (echo) await echo(!before);
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
  const { page, consoleErrors } = await open({
    viewport: { width: 1280, height: 820 },
    init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)),
  });
  let seq = 0;
  const emit = (frame) => page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], ...frame });
  const sent = () => page.evaluate(() => window.__lf.native.sent.slice());
  const clearSent = () => page.evaluate(() => void (window.__lf.native.sent.length = 0));
  const actions = async () => (await sent()).filter((c) => c.Action !== undefined).map((c) => c.Action);
  await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });
  await emit({
    reset: true,
    settings: [],
    events: [...[0, 1, 2, 3, 4].map((i) => laneEvent(i, lane('Empty'))), transport(0, false), { Selected: { frame: 0, lane: 0 } }],
    anchor: { frame: 0, atMs: Date.now(), rate: RATE, grid: 0 },
    meter: { peak: 0, clip: false },
  });

  // ■ ALL during a first take (its count-in).
  await emit({ events: [laneEvent(0, lane('Recording', { armed: true }))] });
  const firstTakeTransport = await page.evaluate(() => {
    const button = document.querySelector('button[aria-label="Stop all tracks"]');
    return { state: window.__lf.looper.stateOf(0), disabled: button?.disabled, text: button?.textContent?.trim() };
  });
  assert.equal(firstTakeTransport.state, 'RECORDING');
  assert.equal(firstTakeTransport.disabled, false);
  assert.equal(firstTakeTransport.text, '■ ALL');
  await emit({ events: [laneEvent(0, lane('Empty'))] });

  // A loop on lanes 1 and 3; a later take on lane 2 commits and is announced.
  await emit({ events: [transport(BAR, true), laneEvent(0, committed('Playing')), laneEvent(2, committed('Stopped'))] });
  await emit({ events: [laneEvent(1, lane('Recording'))] });
  await emit({ events: [laneEvent(1, committed('Playing'))] });
  await page.waitForFunction(() => window.__lf.looper.stateOf(1) === 'PLAYING', undefined, { timeout: 6000 });
  const liveText = await page.locator('.lp [aria-live]').textContent();
  assert.match(liveText ?? '', /Track 2 take recorded/);

  // The record meter carries its level as text.
  await emit({ meter: { peak: 0.5, clip: false } });
  await page.waitForTimeout(300);
  assert.match(await page.locator('[role="meter"]').getAttribute('aria-valuetext'), /-?\d+ dBFS/);
  await emit({ meter: { peak: 0, clip: false } });

  assert.equal(await page.locator('.cmd__lamp').getAttribute('role'), 'img');

  // The lamp across a lost device: amber with a truthful engine state while none runs, back to normal
  // once one runs again. The interface yanked is a `status: null` frame plus the `Lost` device event.
  const lampState = () => page.locator('.cmd__lamp').evaluate((el) => ({
    warn: el.classList.contains('cmd__lamp--warn'),
    engine: el.getAttribute('title')?.match(/^engine: (.*)$/m)?.[1],
  }));
  const STATUS = { backend: 'Wasapi', sampleRate: RATE, block: 256, inputName: 'Fake input', outputName: 'Fake output', alignFrames: 4800, inputFrames: 0, inputOpen: true };
  await emit({ status: STATUS });
  assert.deepEqual(await lampState(), { warn: false, engine: 'running' }, 'a running device: the lamp is neutral');
  await emit({ status: null, device: [{ Lost: { backend: 'Wasapi', reason: 'the device was unplugged' } }] });
  assert.deepEqual(await lampState(), { warn: true, engine: 'no device open' }, 'a lost device: the lamp is amber');
  const lostLog = consoleErrors.findIndex((e) => e.includes('device lost'));
  assert.ok(lostLog >= 0, 'the lost device is logged');
  consoleErrors.splice(lostLog, 1);
  await emit({ status: STATUS });
  assert.deepEqual(await lampState(), { warn: false, engine: 'running' }, 'the device back: the lamp is neutral again');

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

  await emit({ events: [laneEvent(0, committed('Stopped'))] });
  await page.waitForFunction(() => window.__lf.looper.stateOf(0) === 'STOPPED');
  const stoppedCore = page.locator('.lp-lane[aria-label="Track 1"] .lp-core');
  assert.match(await stoppedCore.getAttribute('aria-label'), /play first to overdub/);
  assert.equal(await stoppedCore.getAttribute('title'), 'play first to overdub');

  // The transport keys send the engine's action on its selection; the engine gates it and names a
  // refusal on the feed, which the looper status line says with its lane.
  const live = page.locator('.lp__sr-status');
  await page.evaluate(() => document.activeElement?.blur?.());
  await clearSent();
  await page.keyboard.press('2');
  await emit({ events: [{ Selected: { frame: 0, lane: 1 } }] });
  await page.keyboard.press('Space');
  await page.keyboard.press('Enter');
  await page.waitForTimeout(100);
  assert.ok((await sent()).some((c) => c.SelectTrack === 1), '2 selects track 2');
  assert.deepEqual(await actions(), ['RecDub', 'PlayStop'], 'Space/Enter on track 2 send the engine actions');
  await clearSent();
  await page.keyboard.press('1');
  await emit({ events: [{ Selected: { frame: 0, lane: 0 } }] });
  await page.keyboard.press('Space');
  await page.waitForTimeout(100);
  assert.deepEqual(await actions(), ['RecDub'], 'Space on track 1 goes to the engine, which gates it');
  await emit({ events: [{ Refused: { frame: 0, lane: 0, reason: 'PlayFirst' } }] });
  assert.equal((await live.textContent())?.trim(), 'Track 1: play first to overdub');

  // A refused Enter on an EMPTY lane (nothing to play) says so.
  await clearSent();
  await emit({ events: [transport(0, false), ...[0, 1, 2, 3, 4].map((i) => ({ Cleared: { frame: 0, lane: i } })), ...[0, 1, 2, 3, 4].map((i) => laneEvent(i, lane('Empty')))] });
  await page.waitForFunction(() => [0, 1, 2, 3, 4].every((i) => window.__lf.looper.stateOf(i) === 'EMPTY'));
  await page.keyboard.press('Enter');
  await page.waitForTimeout(100);
  assert.deepEqual(await actions(), ['PlayStop'], 'Enter on an EMPTY lane goes to the engine, which gates it');
  await emit({ events: [{ Refused: { frame: 0, lane: 0, reason: 'Empty' } }] });
  assert.equal((await live.textContent())?.trim(), 'Track 1: nothing to play, record first');

  // An EMPTY selected lane arms on Space.
  await clearSent();
  await page.keyboard.press('Space');
  await page.waitForTimeout(100);
  assert.deepEqual(await actions(), ['RecDub'], 'Space on an EMPTY lane sends REC/DUB');
  await emit({ events: [laneEvent(0, lane('Recording'))] });
  await page.waitForFunction(() => window.__lf.looper.stateOf(0) === 'RECORDING', undefined, { timeout: 3000 });

  // While lane 1 records, lane 2's core is refused, and its hover and label say why.
  const otherCore = page.locator('.lp-lane[aria-label="Track 2"] .lp-core');
  assert.equal(await otherCore.isDisabled(), true);
  assert.equal(await otherCore.getAttribute('title'), 'another track is recording, stop it first');
  assert.equal(await otherCore.getAttribute('aria-label'), 'Track 2 another track is recording, stop it first');
  // The core's name says the action ("Track 1 stop recording"), so a pressed state would contradict it.
  const recordingCorePressed = await page.locator('.lp-lane[aria-label="Track 1"] .lp-core').getAttribute('aria-pressed');
  console.log(JSON.stringify({ recordingCorePressed }));
  await emit({ events: [laneEvent(0, lane('Empty'))] });
  await page.waitForFunction(() => window.__lf.looper.stateOf(0) === 'EMPTY');

  // Toggles: one stable name, state in aria-pressed (the END STOP pattern). No loop yet, so the tempo
  // is unlocked and AUTO REC is enabled.
  assert.equal(await page.evaluate(() => window.__lf.clock.bpmLocked()), false);
  const unstable = [];
  for (const [name, state] of [
    ['Metronome click', 'window.__lf.clock.metronomeOn()'],
    ['Fixed take length', 'window.__lf.looper.fixedLengthEnabled()'],
    ['Retake', 'window.__lf.looper.retakeEnabled()'],
    ['Auto record', 'window.__lf.looper.autoRecordEnabled()'],
    ['Mute master', 'window.__lf.master.muted()'],
  ]) if (!(await pressToggle(page, name, state))) unstable.push(name);

  await emit({ events: [transport(BAR, true), laneEvent(0, committed('Stopped'))] });
  await page.waitForFunction(() => window.__lf.looper.stateOf(0) === 'STOPPED');
  // REV's state is the engine's: echo the lane as the engine reports it after a Reverse.
  const echoReverse = (on) => emit({ events: [laneEvent(0, committed('Stopped', { reversed: on }))] });
  await clearSent();
  for (const [name, state, echo] of [
    ['Track 1 mute', 'window.__lf.looper.trackMuted(0)'],
    ['Track 1 reverse', 'window.__lf.looper.trackInfo(0).reversed', echoReverse],
  ]) if (!(await pressToggle(page, name, state, echo))) unstable.push(name);
  const laneSent = await sent();
  console.log('lane toggles sent', JSON.stringify(laneSent));
  assert.equal(laneSent.filter((c) => c.SetMute?.[0] === 0).length, 2, 'MUTE sends SetMute on each press');
  assert.equal(laneSent.filter((c) => c.Reverse === 0 || c.ActionOn?.[1] === 'Reverse').length, 2, 'REV sends the reverse on each press');
  assert.deepEqual(unstable, [], 'these toggles change their name with their state or lose aria-pressed');
  assert.equal(recordingCorePressed, null, 'the lane core names its action; it must not also claim a pressed state');
  assert.deepEqual(consoleErrors, [], 'no console errors');
});
