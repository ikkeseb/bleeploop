/**
 * The cue of a lane waiting for its END STOP (the loop-end stop), on the web engine fake
 * (`src/platform/host.web.ts`, the `engine-seam` pattern: an init script sets `window.__lfEngineFake`,
 * the probe scripts the feed through `__lf.native` and reads what the UI sends). The two UI rows of the
 * Web Audio probe loop-end-stop.mjs, in engine mode:
 *
 * - a lane the feed reports PLAYING with a pending stop (`stopAt`) cues it (`src/ui/looper/Looper.tsx`,
 *   `src/ui/looper/gates.ts`): its REC/DUB core is disabled and names the refusal in its aria-label and
 *   title, its transforms (↺ REV, ↶ UNDO, ✂ TRIM) are disabled, and its PLAY/STOP cap turns into a
 *   visible stop-now control; the same lane without the pending stop has all of them enabled;
 * - a pointer click on that stop-now control sends the engine's PLAY/STOP for the lane (the engine's
 *   second press stops at once: lf-engine `tests/copy.rs`); the feed's STOPPED ends the cue.
 *
 * Asserted at 1280×720, as the Web Audio probe did. Cannot see the native engine (lf-engine
 * `tests/loop_end_stop.rs` holds the stop itself), Tauri IPC or any timing: the fake answers no command
 * by itself, so every state the DOM shows was scripted.
 * Run: pnpm probe end-stop-cue
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

const RATE = 48000;
const BAR = 2 * RATE; // one 4/4 bar at 120 BPM
const MASTER = 8 * BAR;

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
const committed = (state, extra = {}) => lane(state, { length: MASTER, canUndo: true, canReverse: true, ...extra });
const laneEvent = (i, info, frame = 0) => ({ Lane: { frame, lane: i, info } });
const anchorAt = (frame) => ({ frame, atMs: Date.now(), rate: RATE, grid: 0 });

await probe(async ({ open }) => {
  const { page, consoleErrors } = await open({
    viewport: { width: 1280, height: 720 },
    init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)),
  });
  let seq = 0;
  const emit = (frame) => page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], ...frame });
  const sent = () => page.evaluate(() => window.__lf.native.sent.slice());
  const clearSent = () => page.evaluate(() => void (window.__lf.native.sent.length = 0));
  const button = (name) => page.getByRole('button', { name, exact: true });
  const core = page.locator('.lp-lane').first().locator('.lp-core');
  const playStop = page.locator('.lp-lane').first().locator('.lp-pb--play');
  const transforms = ['Track 1 reverse', 'Track 1 undo or redo the last overdub or trim', 'Trim track 1'];
  const cue = async () => ({
    word: await page.locator('.lp-lane').first().locator('.lp-lane__state').textContent(),
    core: { label: await core.getAttribute('aria-label'), title: await core.getAttribute('title'), disabled: await core.isDisabled() },
    playStop: { label: await playStop.getAttribute('aria-label'), text: (await playStop.textContent()).trim(), visible: await playStop.isVisible() },
    transforms: Object.fromEntries(await Promise.all(transforms.map(async (name) => [name, await button(name).isDisabled()]))),
  });

  await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });
  await emit({
    reset: true,
    settings: [{ SetLoopEndStop: true }],
    events: [
      { Transport: { frame: 0, master: MASTER, bpm: 120, locked: true } },
      laneEvent(0, committed('Playing')),
      ...[1, 2, 3, 4].map((i) => laneEvent(i, lane('Empty'))),
      { Selected: { frame: 0, lane: 0 } },
    ],
    anchor: anchorAt(0),
    meter: { peak: 0, clip: false },
  });

  // Playing, nothing pending: the core overdubs, the cap stops, the transforms are live.
  const before = await cue();
  console.log('playing', JSON.stringify(before));
  assert.equal(before.core.label, 'Track 1 overdub');
  assert.ok(!before.core.disabled, 'a playing lane overdubs');
  assert.equal(before.playStop.label, 'Track 1 stop');
  for (const name of transforms) assert.equal(before.transforms[name], false, `${name} is enabled while nothing is pending`);

  // The feed says the lane stops at the loop end.
  await emit({ events: [laneEvent(0, committed('Playing', { stopAt: 4 * BAR }), BAR)], anchor: anchorAt(BAR) });
  const pending = await cue();
  console.log('pending', JSON.stringify(pending));
  const pendingCore = button('Track 1 stopping at loop end, wait or stop now');
  assert.equal(await pendingCore.count(), 1, 'the core names the pending stop in its aria-label');
  assert.ok(await pendingCore.isDisabled(), 'the core is disabled');
  assert.equal(await pendingCore.getAttribute('title'), 'stopping at loop end, wait or stop now', 'and in its title');
  for (const name of transforms) assert.ok(await button(name).isDisabled(), `${name} is disabled while the stop is pending`);
  const stopNow = button('Track 1 stop now');
  assert.ok(await stopNow.isVisible(), 'a visible stop-now control');
  assert.equal(pending.playStop.text, '■ NOW');

  // A pointer click on it sends the lane's PLAY/STOP.
  await clearSent();
  await stopNow.click();
  await page.waitForFunction(() => window.__lf.native.sent.some((c) => c.PlayStop !== undefined), undefined, { timeout: 5000 });
  const clicked = await sent();
  console.log('stop now sent', JSON.stringify(clicked));
  assert.deepEqual(clicked.filter((c) => c.SelectTrack === undefined), [{ PlayStop: 0 }], 'stop now sends PLAY/STOP for the lane, nothing else');

  // The engine stops the lane: the cue is gone.
  await emit({ events: [laneEvent(0, committed('Stopped'), 2 * BAR)], anchor: anchorAt(2 * BAR) });
  const stopped = await cue();
  console.log('stopped', JSON.stringify(stopped));
  assert.equal(stopped.playStop.label, 'Track 1 play');
  assert.equal(await button('Track 1 stop now').count(), 0, 'no stop-now control once stopped');
  assert.ok(!(await button('Track 1 reverse').isDisabled()), 'a stopped lane reverses again');
  assert.deepEqual(consoleErrors, [], 'no console errors');
});
