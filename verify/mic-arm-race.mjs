/**
 * Arm/disarm race on the real capture module, with only `platform.audioInput.open` substituted for a
 * deferred promise so the gesture order is controlled exactly:
 *   node verify/mic-arm-race.mjs --url=http://localhost:1420
 *
 * Proves that a disarm landing while getUserMedia is still open cancels that open (the late stream is
 * closed, its tracks stopped, inputArmed stays false) and that a burst of toggles ends in the state of
 * the LAST gesture. It substitutes the device open, so it says nothing about real microphone hardware,
 * permission prompts or native ASIO input.
 */
import assert from 'node:assert/strict';
import { chromium } from 'playwright';

const url = process.argv.find((arg) => arg.startsWith('--url='))?.slice(6) ?? 'http://localhost:1420';
const browser = await chromium.launch({ headless: true, args: ['--autoplay-policy=no-user-gesture-required'] });

try {
  const page = await browser.newPage();
  const pageErrors = [];
  page.on('pageerror', (error) => pageErrors.push(String(error)));
  await page.goto(url);
  await page.waitForFunction(() => !!window.__lf);

  const result = await page.evaluate(async () => {
    const lf = window.__lf;
    await lf.looper.init();
    const { engineState } = await import('/src/audio/looper/state.ts');
    if (!engineState.initialized) {
      throw new Error('capture engine is unavailable (crossOriginIsolated=false) — run this probe against the Vite server');
    }

    const realOpen = lf.platform.audioInput.open;
    const opens = [];
    // One fake device per open: `stoppedTracks` records the MediaStream tracks the production disarm
    // path must stop. `close()` is what capture.ts calls on both the adopt and the cancel path.
    const makeFakeInput = () => {
      const node = lf.engine.ctx.createGain();
      const fake = { closes: 0, tracks: [{ stopped: false }] };
      fake.handle = {
        node,
        sampleRate: lf.engine.ctx.sampleRate,
        close() {
          fake.closes++;
          for (const track of fake.tracks) track.stopped = true;
          node.disconnect();
        },
      };
      return fake;
    };
    lf.platform.audioInput.open = () => new Promise((resolve) => {
      const fake = makeFakeInput();
      opens.push({ fake, resolve: () => resolve(fake.handle) });
    });

    const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
    const waitForOpen = async (index, label) => {
      const deadline = performance.now() + 4000;
      while (opens.length <= index) {
        if (performance.now() >= deadline) throw new Error(`${label} timed out waiting for platform open`);
        await wait(5);
      }
      return opens[index];
    };

    try {
      // ── Case 1: disarm during a pending open must cancel it ──────────────────────────────
      const cancelled = lf.looper.armInput();
      const first = await waitForOpen(0, 'cancelled arm');
      lf.looper.disarmInput();
      const armedDuringPending = lf.looper.inputArmed();
      first.resolve();
      const cancelledResult = await cancelled;
      await wait(20);
      const cancelledCase = {
        armedDuringPending,
        armInputResolved: cancelledResult,
        armedAfter: lf.looper.inputArmed(),
        closes: first.fake.closes,
        tracksStopped: first.fake.tracks.every((track) => track.stopped),
        opens: opens.length,
      };

      // ── Case 2: arm → disarm → arm must end ARMED (the last gesture wins) ────────────────
      const rearmFirst = lf.looper.armInput();
      const second = await waitForOpen(1, 'rearmed arm');
      lf.looper.disarmInput();
      const rearmSecond = lf.looper.armInput();
      second.resolve();
      const rearmResults = [await rearmFirst, await rearmSecond];
      await wait(20);
      const rearmCase = {
        armInputResolved: rearmResults,
        armedAfter: lf.looper.inputArmed(),
        closes: second.fake.closes,
        tracksStopped: second.fake.tracks.some((track) => track.stopped),
        opens: opens.length,
      };

      // ── Teardown: the adopted input must still disarm normally ──────────────────────────
      lf.looper.disarmInput();
      const finalState = {
        armed: lf.looper.inputArmed(),
        closes: second.fake.closes,
        tracksStopped: second.fake.tracks.every((track) => track.stopped),
      };

      // ── Case 3: the UI route. Two taps while the open is pending = arm, then DISARM ──────
      // `toggleInput` is what the MIC button calls; `inputArmed()` is still false during the open, so
      // without the pending-open check the second tap would be a second arm, not a cancel.
      const tapOn = lf.looper.toggleInput();
      const third = await waitForOpen(2, 'toggled arm');
      const tapOff = lf.looper.toggleInput();
      const requestedAfterTapOff = lf.looper.inputArmRequested();
      third.resolve();
      const tapResults = [await tapOn, await tapOff];
      await wait(20);
      const toggleOffCase = {
        toggleResolved: tapResults,
        requestedAfterTapOff,
        armedAfter: lf.looper.inputArmed(),
        closes: third.fake.closes,
        tracksStopped: third.fake.tracks.every((track) => track.stopped),
        opens: opens.length,
      };

      // ── Case 4: a burst of taps (on, off, on) on ONE pending open ends in the last gesture: ON ──
      const burst = [lf.looper.toggleInput()];
      const fourth = await waitForOpen(3, 'burst arm');
      burst.push(lf.looper.toggleInput(), lf.looper.toggleInput());
      fourth.resolve();
      const burstResults = await Promise.all(burst);
      await wait(20);
      const burstOnCase = {
        toggleResolved: burstResults,
        armedAfter: lf.looper.inputArmed(),
        closes: fourth.fake.closes,
        opens: opens.length,
      };
      lf.looper.disarmInput();
      const burstFinal = { armed: lf.looper.inputArmed(), closes: fourth.fake.closes };
      return { cancelledCase, rearmCase, finalState, toggleOffCase, burstOnCase, burstFinal };
    } finally {
      lf.platform.audioInput.open = realOpen;
      lf.looper.disarmInput();
    }
  });

  console.log(JSON.stringify({ url, ...result }, null, 2));

  const { cancelledCase, rearmCase, finalState, toggleOffCase, burstOnCase, burstFinal } = result;
  assert.equal(cancelledCase.armedDuringPending, false, 'a pending open must not report ARMED');
  assert.equal(cancelledCase.armInputResolved, false, 'a cancelled arm must resolve false');
  assert.equal(cancelledCase.armedAfter, false, 'a cancelled open must leave inputArmed false');
  assert.equal(cancelledCase.closes, 1, 'the late open must close its own stream exactly once');
  assert.equal(cancelledCase.tracksStopped, true, 'the cancelled stream tracks must be stopped');

  assert.equal(rearmCase.opens, 2, 'the re-arm must share the pending open, not start a second one');
  assert.deepEqual(rearmCase.armInputResolved, [true, true], 'the shared arm must resolve true for both callers');
  assert.equal(rearmCase.armedAfter, true, 'arm → disarm → arm must end ARMED');
  assert.equal(rearmCase.closes, 0, 'the adopted stream must not be closed by the superseded disarm');
  assert.equal(rearmCase.tracksStopped, false, 'the adopted stream tracks must stay live');

  assert.equal(finalState.armed, false, 'the adopted input must still disarm normally');
  assert.equal(finalState.closes, 1, 'the ordinary disarm closes the adopted stream once');
  assert.equal(finalState.tracksStopped, true, 'the ordinary disarm stops the adopted tracks');

  assert.equal(toggleOffCase.opens, 3, 'the second tap must cancel the pending open, not start another');
  assert.deepEqual(toggleOffCase.toggleResolved, [false, false], 'both taps resolve false: a cancelled arm and a disarm');
  assert.equal(toggleOffCase.requestedAfterTapOff, false, 'the second tap must withdraw the arm request');
  assert.equal(toggleOffCase.armedAfter, false, 'tap, tap during the open must end DISARMED');
  assert.equal(toggleOffCase.closes, 1, 'the late open must close its own stream once');
  assert.equal(toggleOffCase.tracksStopped, true, 'the cancelled stream tracks must be stopped');

  assert.equal(burstOnCase.opens, 4, 'the burst must share one pending open');
  assert.deepEqual(burstOnCase.toggleResolved, [true, false, true], 'arm, disarm, arm through the UI route');
  assert.equal(burstOnCase.armedAfter, true, 'a burst ending in a tap ON must end ARMED');
  assert.equal(burstOnCase.closes, 0, 'the adopted stream must survive the superseded disarm');
  assert.equal(burstFinal.armed, false, 'the adopted input still disarms normally');
  assert.equal(burstFinal.closes, 1, 'the ordinary disarm closes the adopted stream once');

  assert.deepEqual(pageErrors, [], 'unexpected browser errors');
  console.log('=== RESULT: mic-arm-race passed ===');
} finally {
  await browser.close();
}
