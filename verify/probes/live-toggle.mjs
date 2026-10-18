/**
 * GO LIVE as the engine's toggle (`src/ui/instrument/live.ts`, `src/ui/state/native-io.ts`), on the web
 * engine fake (`src/platform/host.web.ts`), which applies `{"Toggle":{"Live":slot}}` against its own
 * flag as the engine does and shows it on the feed's `live`:
 *
 * - a press sends the toggle, never a `SetSlotLive` the UI worked out;
 * - a press made while the engine's flag differs from the feed's (a live-on the fake has not applied,
 *   held by its `holdApply` seam) turns the slot off: it flips the engine's value, not the feed's;
 * - two quick presses, the first one's batch still unanswered (`sendHold`), both reach the engine and
 *   end where they started, from the rendered cap (clicked twice: it stays pressable and shows busy)
 *   and from the pedal's named action alike;
 * - presses made while a source op holds the slot past two seconds (the feed stale meanwhile) apply
 *   once it settles: one press as one toggle, two as none;
 * - the page's first reset frame disarms both slots (a reload's disarm), and a later one (a new engine,
 *   a held feed's resync) sends no live flag: the engine's settings replay is the authority, and the
 *   feed's flags may be older than an unload's live-off.
 *
 * A tone reload's resume yielding to a press: `slot-sources`. Cannot see the native engine (its command
 * order is the rtrb ring's; `src-tauri/crates/lf-engine/tests/toggles.rs`) or Tauri IPC.
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
    const { runAction } = await import('/src/app/actions.ts');
    const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
    const idle = async () => { while (slots.slotPendingCounts().some((n) => n > 0)) await pause(10); };
    const liveFlags = (from) => lf.native.sent.slice(from).filter((c) => c.SetSlotLive).map((c) => c.SetSlotLive);
    const out = {};
    // 0. Reset frames: the page's first, then a later one whose feed shows slot A live.
    let from = lf.native.sent.length;
    lf.native.emit({ seq: 1, reset: true, events: [], settings: [], live: [false, false] });
    await pause(30);
    out.firstReset = liveFlags(from);
    from = lf.native.sent.length;
    lf.native.emit({ seq: 2, reset: true, events: [], settings: [], live: [true, false] });
    await pause(30);
    out.laterReset = liveFlags(from);
    lf.native.emit({ seq: 3, reset: false, events: [], live: [false, false] });
    await pause(30);
    instrument.selectOff(0);
    instrument.setActiveSlot(0);
    await idle();
    await pause(50);
    // What the UI sent slot A since `from`: `toggle` for a press, else the flag it set.
    const sentA = (from) =>
      lf.native.sent.slice(from).flatMap((c) =>
        c.Action?.Toggle?.Live === 0 ? ['toggle'] : c.SetSlotLive?.[0] === 0 ? [c.SetSlotLive[1]] : []);

    // 1. A press sends the toggle.
    from = lf.native.sent.length;
    await live.toggleLive(0);
    await pause(30);
    out.press = { sent: sentA(from), live: inputArmed()[0] };

    // 2. The engine's flag ahead of the feed: off on the feed, a live-on queued unapplied, then a press.
    await live.toggleLive(0);
    await pause(30);
    out.offFirst = inputArmed()[0];
    lf.native.holdApply = true;
    from = lf.native.sent.length;
    await live.startLive(0);
    await live.toggleLive(0);
    out.raceWhileHeld = inputArmed()[0];
    lf.native.holdApply = false;
    await pause(30);
    out.race = { sent: sentA(from), live: inputArmed()[0] };

    // 3. Two quick presses, the first one's batch unanswered when the second comes: from the rendered
    // cap (a click on a disabled button does nothing, as a real one)...
    const quick = async (press) => {
      let answer;
      lf.native.sendHold = new Promise((resolve) => (answer = resolve));
      const at = lf.native.sent.length;
      press();
      await pause(20);
      const busy = cap().getAttribute('aria-busy');
      press();
      await pause(20);
      const whileHeld = sentA(at);
      lf.native.sendHold = null;
      answer();
      while (live.liveBusy(0)) await pause(10);
      await idle();
      await pause(30);
      return { busy, whileHeld, sent: sentA(at), live: inputArmed()[0] };
    };
    const cap = () => document.querySelector('button[aria-label="Live input for slot A"]');
    out.capTwice = await quick(() => cap().click());
    // ...and from the pedal's named action (its `Press` first, then GO LIVE).
    out.pedalTwice = await quick(() => runAction('goLive'));

    // 4. A source op holds slot A past two seconds; the feed shows nothing new meanwhile.
    const held = async (presses) => {
      let release;
      void slots.serializeSlot(0, () => new Promise((resolve) => (release = resolve)));
      const at = lf.native.sent.length;
      const before = inputArmed()[0];
      for (let i = 0; i < presses; i++) runAction('goLive');
      await pause(2100);
      const whileHeld = sentA(at);
      release();
      await idle();
      await pause(30);
      return { before, whileHeld, sent: sentA(at), live: inputArmed()[0] };
    };
    out.heldOnce = await held(1);
    out.heldTwice = await held(2);
    return out;
  });
  console.log(JSON.stringify(result));
  assert.deepEqual(result.firstReset, [[0, false], [1, false]], "the page's first reset disarms both slots");
  assert.deepEqual(result.laterReset, [], 'a later reset sends no live flag');
  assert.deepEqual(result.press, { sent: ['toggle'], live: true }, 'a press sends the toggle, and the engine turns the slot live');
  assert.equal(result.offFirst, false, 'a second press turns it off');
  assert.equal(result.raceWhileHeld, false, 'the feed still shows slot A off');
  assert.deepEqual(result.race, { sent: [true, 'toggle'], live: false }, "a press after a live-on the feed does not show yet turns it off: the engine's value, flipped");
  assert.deepEqual(result.capTwice, { busy: 'true', whileHeld: ['toggle'], sent: ['toggle', 'toggle'], live: false }, 'two clicks on the cap: it shows busy and stays pressable, the second waits for the first, both reach the engine, back off');
  assert.deepEqual(result.pedalTwice, { busy: 'true', whileHeld: ['toggle'], sent: ['toggle', 'toggle'], live: false }, 'two pedal presses alike: none dropped');
  assert.deepEqual(result.heldOnce, { before: false, whileHeld: [], sent: ['toggle'], live: true }, 'one press during a held op: one toggle once it settles');
  assert.deepEqual(result.heldTwice, { before: true, whileHeld: [], sent: [], live: true }, 'two presses during a held op: none');
});
