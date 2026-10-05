/**
 * FADE and DUB FEEDBACK in engine mode, on the web engine fake (`src/platform/host.web.ts`, the
 * `engine-seam` pattern: an init script sets `window.__lfEngineFake`, the probe scripts the feed through
 * `__lf.native` and reads what the UI sends):
 *
 * - FADE (the command bar, `src/ui/transport/Transport.tsx`): the pill beside ■ ALL sends the engine's
 *   `FadeAll` action; its bars stepper steps 1, 2, 4, 8 and sends `SetFadeBars`; it is disabled with its
 *   reason while nothing plays and while a lane records. Lanes the feed reports fading read FADING (the
 *   word where ENDING shows, the well's FADING OUT) in the looper, and in the stage view by their chips'
 *   names and its message line; the pill reads
 *   FADING and a press stops the fade; an engine refusal (`Fading`) lands on its lane. A reset frame's
 *   remembered bars are adopted. The bars persist: across launches of one browser profile, a fresh
 *   engine gets the bars last chosen, and an engine's remembered bars win and are kept.
 * - `fadeAll` ('Fade out all', `src/app/actions.ts`): in MIDI learn's picker; it sends the engine's
 *   action.
 * - DUB FEEDBACK (the lane's FX drawer, `src/ui/looper/FxPanel.tsx`): the slider starts at 100 %, sends
 *   `SetDubFeedback` as a fraction, reads REPLACE at 0; a reset frame's value is adopted, COPY hands the
 *   copy the value the engine's `Copied` says it copied (the source may have moved since), and CLEAR
 *   resets it (`Cleared`), each as the lane's `Mix` the fake reports after it.
 *
 * Cannot see the native engine (lf-engine `tests/fade.rs` and `tests/dub_feedback.rs` hold the audio),
 * Tauri IPC or any timing: the fake answers no command by itself but a lane's mix, so every state the
 * DOM shows was scripted. Screenshots of the command bar and the FX drawer land in logs/fade-dub/ for
 * the eye.
 * Run: pnpm probe fade-dub
 */
import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { probe } from '../harness/probe.ts';

const RATE = 48000;
const BAR = 2 * RATE; // one 4/4 bar at 120 BPM
const outDir = 'logs/fade-dub';

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
const playing = (extra = {}) => lane('Playing', { length: 2 * BAR, canReverse: true, ...extra });
const laneEvent = (i, info, frame = 0) => ({ Lane: { frame, lane: i, info } });
const transport = (master, locked = true) => ({ Transport: { frame: 0, master, bpm: 120, locked } });
const anchorAt = (frame) => ({ frame, atMs: Date.now(), rate: RATE, grid: 0 });

await probe(async ({ browser, open }) => {
  await mkdir(outDir, { recursive: true });
  const { page, consoleErrors } = await open({
    viewport: { width: 1600, height: 900 },
    init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)),
  });
  let seq = 0;
  const emit = (frame) => page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], ...frame });
  const sent = () => page.evaluate(() => window.__lf.native.sent.slice());
  const clearSent = () => page.evaluate(() => void (window.__lf.native.sent.length = 0));
  const sentAtLeast = async (count) => {
    await page.waitForFunction((n) => window.__lf.native.sent.length >= n, count, { timeout: 5000 });
    return sent();
  };
  const lanes = page.locator('.lp-lane');
  const word = (i) => lanes.nth(i).locator('.lp-lane__state').textContent();
  const well = (i) => lanes.nth(i).locator('.lp-lane__wellmsg');
  const fade = page.locator('.transport__fade .transport__tgl');
  const shorter = page.getByRole('button', { name: 'Shorter fade', exact: true });
  const longer = page.getByRole('button', { name: 'Longer fade', exact: true });
  const fadeBars = () => page.locator('.transport__fade .transport__bars-val').innerText().then((t) => t.replace(/\s+/g, ' ').trim().toLowerCase());
  const runAction = (id) => page.evaluate((a) => import('/src/app/actions.ts').then((m) => m.runAction(a)), id);

  await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });
  await emit({
    reset: true,
    settings: [],
    events: [
      transport(2 * BAR),
      laneEvent(0, playing()),
      laneEvent(1, playing()),
      ...[2, 3, 4].map((i) => laneEvent(i, lane('Empty'))),
      { Selected: { frame: 0, lane: 0 } },
    ],
    anchor: anchorAt(0),
    meter: { peak: 0, clip: false },
  });

  // ── FADE: the pill and its bars ────────────────────────────────────────────────────────────────────
  assert.equal(await fade.count(), 1, 'FADE shows in engine mode');
  const beside = await page.evaluate(() => {
    const kids = [...document.querySelector('.transport__global').children];
    const all = kids.findIndex((el) => /ALL/.test(el.textContent) && el.tagName === 'BUTTON');
    return kids[all + 1]?.classList.contains('transport__fade');
  });
  assert.ok(beside, 'FADE sits beside ■ ALL');
  assert.equal((await fade.textContent()).trim(), 'FADE');
  assert.equal(await fadeBars(), '2 bars', 'two bars by default');
  await page.locator('.cmd').screenshot({ path: `${outDir}/command-bar.png` });
  await clearSent();
  await longer.click();
  await longer.click();
  assert.ok(await longer.isDisabled(), 'at most 8 bars');
  for (let k = 0; k < 3; k++) await shorter.click();
  assert.ok(await shorter.isDisabled(), 'at least 1 bar');
  assert.equal(await fadeBars(), '1 bar');
  assert.deepEqual(await sentAtLeast(5), [4, 8, 4, 2, 1].map((b) => ({ SetFadeBars: b })), 'the stepper walks 1, 2, 4, 8');
  await longer.click();
  await clearSent();
  await fade.click();
  assert.deepEqual(await sentAtLeast(1), [{ Action: 'FadeAll' }], 'FADE sends the engine action');

  // ── The feed says the lanes fade ──────────────────────────────────────────────────────────────────
  const end = 4 * BAR;
  await emit({ events: [laneEvent(0, playing({ stopAt: end, fading: true })), laneEvent(1, playing({ stopAt: end, fading: true }))] });
  assert.equal(await word(0), 'FADING', 'the lane word reads FADING where ENDING shows');
  assert.equal(await word(1), 'FADING');
  assert.match(await well(0).textContent(), /FADING OUT/);
  assert.equal((await fade.textContent()).trim(), 'FADING', 'the pill reads FADING');
  assert.equal(await fade.getAttribute('aria-label'), 'Stop the fade now');
  assert.match(await page.locator('.transport__global .transport__tgl').first().textContent(), /■ NOW/, '■ ALL stops now');
  await page.locator('.cmd').screenshot({ path: `${outDir}/command-bar-fading.png` });
  await page.locator('.lp-lane').first().screenshot({ path: `${outDir}/lane-fading.png` });
  await page.evaluate(() => document.activeElement instanceof HTMLElement && document.activeElement.blur());
  await page.keyboard.press('b');
  await page.locator('.sv').waitFor();
  const stageChips = await page.locator('.sv-chip').evaluateAll((chips) => chips.map((c) => c.getAttribute('aria-label')));
  const stageMsg = await page.locator('.sv-msg').textContent();
  console.log('stage chips', JSON.stringify(stageChips), 'message', JSON.stringify(stageMsg));
  assert.deepEqual(stageChips.slice(0, 3), ['Track 1, fading out', 'Track 2, fading out', 'Track 3, empty'], 'the stage view\'s chips say the lanes fade');
  assert.match(stageMsg, /^[12] · FADING OUT$/, 'the stage view\'s message line reads FADING OUT');
  await page.screenshot({ path: `${outDir}/stage-fading.png` });
  await page.keyboard.press('b');
  await page.locator('.sv').waitFor({ state: 'detached' });
  await emit({ events: [{ Refused: { frame: BAR, lane: 1, reason: 'Fading' } }] });
  assert.match(await well(1).textContent(), /fading out, wait or stop now/, 'a refusal names the fade');
  await clearSent();
  await fade.click();
  assert.deepEqual(await sentAtLeast(1), [{ Action: 'FadeAll' }], 'a second press is the same action: the engine stops the fade');

  // ── The fade ends: the lanes stop, FADE has nothing to fade ──────────────────────────────────────
  const stopped = lane('Stopped', { length: 2 * BAR, canReverse: true });
  await emit({ events: [laneEvent(0, stopped, end), laneEvent(1, stopped, end)] });
  assert.equal(await word(0), 'STOPPED');
  assert.equal((await fade.textContent()).trim(), 'FADE');
  assert.ok(await fade.isDisabled(), 'nothing plays: FADE is disabled');
  assert.match(await fade.getAttribute('title'), /nothing is playing to fade/);
  await emit({ events: [laneEvent(0, playing()), laneEvent(2, lane('Recording', { armed: true }))] });
  assert.ok(await fade.isDisabled(), 'a lane records: FADE is disabled');
  assert.match(await fade.getAttribute('title'), /this track is recording, stop it first/);
  await emit({ events: [laneEvent(2, lane('Empty'))] });
  assert.ok(!(await fade.isDisabled()));

  // ── fadeAll: MIDI learn, and the engine action ───────────────────────────────────────────────────
  await clearSent();
  await runAction('fadeAll');
  assert.deepEqual(await sentAtLeast(1), [{ Action: 'FadeAll' }], 'the pedal action sends the engine action');
  const label = await page.evaluate(() => import('/src/app/actions.ts').then((m) => m.ACTION_LABELS.fadeAll));
  assert.equal(label, 'Fade out all');
  await page.evaluate(() => window.__lf.ui.openSettings());
  const picked = page.locator('[aria-label="Action to learn"] option', { hasText: 'Fade out all' });
  await picked.waitFor({ state: 'attached', timeout: 5000 });
  assert.equal(await picked.getAttribute('value'), 'fadeAll', 'MIDI learn lists it');
  await page.keyboard.press('Escape');

  // ── DUB FEEDBACK in the FX drawer ─────────────────────────────────────────────────────────────────
  const openFx = async (i) => {
    await lanes.nth(i).getByRole('button', { name: `Track ${i + 1} FX`, exact: true }).click();
    await page.locator('.lp-drawer').waitFor();
  };
  const dub = (i) => page.getByRole('slider', { name: `Track ${i + 1} dub feedback`, exact: true });
  const dubText = () => page.locator('.fxp-dub .fxp-param__val').textContent();
  await openFx(0);
  assert.equal(await dub(0).inputValue(), '100', 'DUB FEEDBACK starts at 100 %');
  assert.equal(await dubText(), '100 %');
  await clearSent();
  await dub(0).fill('50');
  assert.deepEqual(await sentAtLeast(1), [{ SetDubFeedback: [0, 0.5] }], 'the slider sends a fraction');
  await dub(0).fill('0');
  assert.deepEqual((await sentAtLeast(2)).at(-1), { SetDubFeedback: [0, 0] });
  assert.equal(await dubText(), 'REPLACE', '0 % reads as replace');
  await page.locator('.lp-drawer').screenshot({ path: `${outDir}/fx-drawer.png` });
  await dub(0).fill('40');
  // COPY took 40 %; the source moves to 75 % before the copy's Copied lands, which carries what it copied.
  await dub(0).fill('75');
  // The copy's DUB FEEDBACK arrives as its lane's `Mix`, which the fake reports as the engine does.
  const dubSettles = (i, pct) =>
    page.waitForFunction(([i, pct]) => document.querySelector(`[aria-label="Track ${i + 1} dub feedback"]`)?.value === pct, [i, pct], { timeout: 5000 }).catch(() => {});
  await emit({ events: [{ Copied: { frame: BAR, from: 0, to: 2, feedback: 0.4 } }, laneEvent(2, playing())] });
  await openFx(2);
  await dubSettles(2, '40');
  assert.equal(await dub(2).inputValue(), '40', "COPY hands the copy the DUB FEEDBACK the engine copied, not the source's later one");
  await emit({ events: [{ Cleared: { frame: BAR, lane: 2 } }, laneEvent(2, playing())] });
  await dubSettles(2, '100');
  assert.equal(await dub(2).inputValue(), '100', 'CLEAR resets it');

  // ── A reload's reset frame: the engine's remembered bars and feedback are adopted ─────────────────
  await clearSent();
  await emit({
    reset: true,
    settings: [{ SetFadeBars: 8 }, { SetDubFeedback: [2, 0.25] }],
    events: [transport(2 * BAR), laneEvent(0, playing()), laneEvent(1, playing()), laneEvent(2, playing()), { Selected: { frame: 0, lane: 0 } }],
    anchor: anchorAt(0),
    meter: { peak: 0, clip: false },
  });
  assert.equal(await fadeBars(), '8 bars', "the engine's fade bars are adopted");
  assert.equal(await dub(2).inputValue(), '25', "the engine's DUB FEEDBACK is adopted");
  const adopted = await sent();
  assert.ok(!adopted.some((c) => c.SetFadeBars !== undefined || c.SetDubFeedback), 'adopted settings are not pushed back');
  assert.deepEqual(consoleErrors, [], 'no console errors');

  // ── FADE's bars outlive a restart, as the master and click levels do ─────────────────────────────
  // One browser profile, launched three times: the bars chosen are sent to a fresh engine (which
  // remembers nothing), and an engine that remembers bars wins and is kept for the next launch.
  const profile = await browser.newContext({ viewport: { width: 1600, height: 900 } });
  const launch = async (settings) => {
    const app = await open({ context: profile, init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)) });
    await app.page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });
    await app.page.evaluate((f) => window.__lf.native.emit(f), {
      seq: 1,
      reset: true,
      settings,
      events: [transport(2 * BAR), laneEvent(0, playing()), { Selected: { frame: 0, lane: 0 } }],
      anchor: anchorAt(0),
      meter: { peak: 0, clip: false },
    });
    const bars = () => app.page.locator('.transport__fade .transport__bars-val').innerText().then((t) => t.replace(/\s+/g, ' ').trim().toLowerCase());
    const sentFade = () => app.page.evaluate(() => window.__lf.native.sent.filter((c) => c.SetFadeBars !== undefined));
    return { ...app, bars, sentFade };
  };
  const first = await launch([]);
  assert.equal(await first.bars(), '2 bars', 'a first launch: the default');
  const longerThere = first.page.getByRole('button', { name: 'Longer fade', exact: true });
  await longerThere.click();
  await longerThere.click();
  assert.equal(await first.bars(), '8 bars');
  await first.page.close();
  const second = await launch([]);
  assert.equal(await second.bars(), '8 bars', 'a fresh launch keeps 8 bars');
  assert.deepEqual(await second.sentFade(), [{ SetFadeBars: 8 }], 'and sends them to the engine, which lacks them');
  await second.page.close();
  const third = await launch([{ SetFadeBars: 4 }]);
  assert.equal(await third.bars(), '4 bars', "an engine's remembered bars win");
  assert.deepEqual(await third.sentFade(), [], 'and are not pushed back');
  await third.page.close();
  const fourth = await launch([]);
  assert.equal(await fourth.bars(), '4 bars', 'the adopted bars are kept for the next launch');
  for (const app of [first, second, third, fourth]) assert.deepEqual(app.consoleErrors, [], 'no console errors across the launches');
  await profile.close();
});
