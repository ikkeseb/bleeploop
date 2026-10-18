/**
 * GO LIVE pressed twice before the feed shows the first press (`src/ui/instrument/live.ts`,
 * `src/ui/state/native-io.ts` `liveToggleBase`), on the web engine fake (`src/platform/host.web.ts`) with
 * its `holdApply` seam holding the engine's answer: the second press toggles from the flag the first
 * sent, not from the feed's older one.
 *
 * - two presses on an Off slot send `SetSlotLive` on, then off (not on twice);
 * - the first press's answer (live) landing on the feed after the second press: a third press sends on;
 * - once the feed shows them, the slot is not live, and the next press sends on again.
 *
 * Cannot see the native engine (its command order is the rtrb ring's) or Tauri IPC.
 * Run: pnpm probe live-toggle
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

await probe(async ({ open }) => {
  const { page } = await open({ init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)) });
  await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 10000 });
  const result = await page.evaluate(async () => {
    const lf = window.__lf;
    const instrument = await import('/src/ui/state/instrument.ts');
    const slots = await import('/src/ui/state/instrument-slots.ts');
    const live = await import('/src/ui/instrument/live.ts');
    const { inputArmed } = await import('/src/ui/state/native-io.ts');
    const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
    const idle = async () => { while (slots.slotPendingCounts().some((n) => n > 0)) await pause(10); };
    instrument.selectOff(0);
    await idle();
    await pause(50);
    const sentLive = (from) => lf.native.sent.slice(from).filter((c) => c.SetSlotLive?.[0] === 0).map((c) => c.SetSlotLive[1]);

    lf.native.holdApply = true; // the engine has not applied them: the feed still shows slot A off
    const from = lf.native.sent.length;
    await live.toggleLive(0);
    await live.toggleLive(0);
    const twoPresses = sentLive(from);
    const armedWhileHeld = inputArmed()[0];
    // The first press's answer lands late: the feed shows slot A live.
    lf.native.emit({ seq: 0, reset: false, events: [], live: [true, false] });
    await pause(20);
    const lateAnswer = inputArmed()[0];
    await live.toggleLive(0);
    const threePresses = sentLive(from);
    lf.native.holdApply = false;
    await pause(50);
    const settled = inputArmed()[0];
    const next = lf.native.sent.length;
    await live.toggleLive(0);
    await pause(50);
    return { twoPresses, armedWhileHeld, lateAnswer, threePresses, settled, third: sentLive(next), after: inputArmed()[0] };
  });
  console.log(JSON.stringify(result));
  assert.equal(result.armedWhileHeld, false, 'the feed has not shown the first press');
  assert.deepEqual(result.twoPresses, [true, false], 'the second press toggles from the flag the first sent');
  assert.equal(result.lateAnswer, true, "the feed shows the first press's late answer");
  assert.deepEqual(result.threePresses, [true, false, true], 'a third press toggles from the flag the second sent, not the late answer');
  assert.equal(result.settled, true, 'once applied, the slot is live');
  assert.deepEqual({ third: result.third, after: result.after }, { third: [false], after: false }, 'the next press stops it');
});
