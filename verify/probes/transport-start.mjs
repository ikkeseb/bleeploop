/**
 * Transport bring-up after a REJECTED engine.start, on the real clock module with only
 * `engine.start` substituted for one call. Run: pnpm probe transport-start
 *
 * Proves that `clock.ensureRunning()` does not stay marked running when `engine.start` rejects
 * (Tone adoption, toneStart, the master-limiter latency measurement): the error is logged once,
 * `running()` falls back to false, and the next gesture retries the start, which then leaves the
 * clock running with the free-run pulse and the Tone transport started exactly once. It injects the
 * rejection, so it says nothing about which real failures occur on which machines.
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

await probe(async ({ open, url }) => {
  const { page, consoleErrors } = await open();

  const result = await page.evaluate(async () => {
    const lf = window.__lf;
    const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
    const runningAtBoot = lf.clock.running();

    const realStart = lf.engine.start;
    let startCalls = 0;
    lf.engine.start = function () {
      startCalls++;
      if (startCalls === 1) return Promise.reject(new Error('Injected engine.start rejection'));
      return realStart.call(this);
    };
    const transport = lf.transport();
    const realTransportStart = transport.start;
    let transportStarts = 0;
    transport.start = function (...args) {
      transportStarts++;
      return realTransportStart.apply(this, args);
    };
    // startFreeRunPulse is the only setInterval ensureRunning makes: count the pulses it starts.
    const realSetInterval = window.setInterval;
    let pulseStarts = 0;
    let counting = false;
    window.setInterval = function (...args) {
      if (counting) pulseStarts++;
      return realSetInterval.apply(this, args);
    };
    const gesture = () => {
      counting = true;
      try { lf.clock.ensureRunning(); } finally { counting = false; }
    };

    try {
      gesture();
      const runningWhilePending = lf.clock.running();
      gesture(); // a second call while the first start is still pending must not start again
      const callsWhilePending = startCalls;
      await wait(50);
      const first = { runningWhilePending, callsWhilePending, running: lf.clock.running(), startCalls, transportStarts, pulseStarts };

      gesture();
      await wait(500);
      const second = {
        running: lf.clock.running(),
        startCalls,
        transportStarts,
        pulseStarts,
        ctxState: lf.engine.ctx.state,
        transportState: transport.state,
      };
      gesture(); // idempotent once running
      const third = { startCalls, transportStarts, pulseStarts };
      return { runningAtBoot, first, second, third };
    } finally {
      lf.engine.start = realStart;
      transport.start = realTransportStart;
      window.setInterval = realSetInterval;
    }
  });

  console.log(JSON.stringify({ url, ...result, consoleErrors }, null, 2));
  const { runningAtBoot, first, second, third } = result;
  assert.equal(runningAtBoot, false, 'the clock must be idle before the first gesture');
  assert.equal(first.callsWhilePending, 1, 'a pending start must not start twice');
  assert.equal(first.running, false, 'a rejected engine.start must leave running false');
  assert.equal(first.startCalls, 1);
  const logged = consoleErrors.filter((text) => text.includes('Injected engine.start rejection'));
  assert.equal(logged.length, 1, 'the rejected start must be logged exactly once');
  assert.equal(second.startCalls, 2, 'the next gesture must retry engine.start');
  assert.equal(second.running, true, 'the retried start must leave the clock running');
  assert.equal(second.ctxState, 'running');
  assert.equal(second.transportState, 'started');
  assert.equal(second.transportStarts, 1, 'the Tone transport must start exactly once');
  assert.equal(second.pulseStarts, 1, 'the free-run pulse must start exactly once');
  assert.deepEqual(third, { startCalls: 2, transportStarts: 1, pulseStarts: 1 }, 'ensureRunning is idempotent once running');
});
