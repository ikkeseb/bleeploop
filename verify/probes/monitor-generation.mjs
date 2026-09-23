/**
 * Actual monitor lifecycle with delayed host replies: a rearmed monitor is not overwritten by an old
 * reply; a stale configuration reply neither reopens nor overwrites a take's frozen compensation, and
 * starts no new query; a reply inside the current generation respects its first-use freeze; an
 * in-flight monitor arm is not overwritten by a newer buffer configuration; and when the most
 * recently armed slot disarms, a surviving native monitor reopens compensation, freezes its first
 * take immediately and ignores its own late latency reply after that. No hardware latency claims.
 * Run: pnpm probe monitor-generation
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

await probe(async ({ open }) => {
  const { page } = await open();
  const result = await page.evaluate(async () => {
    const { platform } = await import('/src/platform/index.ts');
    const io = await import('/src/audio/native-io.ts');
    const slots = await import('/src/audio/instrument-slots.ts');
    const { recordLatency: latency } = await import('/src/audio/record-latency.ts');
    await window.__lf.engine.start();
    slots.setSlotPlugins([
      { id: 'probe-0', name: 'Probe 0', path: 'probe-0', format: 'vst3', isEffect: true },
      { id: 'probe-1', name: 'Probe 1', path: 'probe-1', format: 'vst3', isEffect: true },
    ]);
    const host = platform.pluginHost;
    const timers = [];
    const originalTimeout = window.setTimeout;
    window.setTimeout = (fn, delay, ...args) => {
      if (delay === io.MONITOR_LATENCY_SETTLE_MS) timers.push(() => fn(...args));
      return originalTimeout(fn, delay, ...args);
    };
    host.armMonitor = async () => {};
    host.disarmMonitor = async () => {};
    host.monitorLatencySeconds = async () => 0.01;
    await io.armMonitor(0);
    const oldTimer = timers.at(-1);
    let resolveOld;
    host.monitorLatencySeconds = () => new Promise(resolve => { resolveOld = resolve; });
    const oldSettle = io.refreshMonitorLatency(0, 'settle');
    await io.disarmMonitor(0);
    host.monitorLatencySeconds = async () => 0.03;
    await io.armMonitor(0);
    resolveOld(0.09);
    await oldSettle;
    const afterRearm = latency.cpalOutSeconds();
    let staleTimerReads = 0;
    host.monitorLatencySeconds = async () => { staleTimerReads++; return 0.03; };
    oldTimer();
    await Promise.resolve();
    host.monitorLatencySeconds = () => new Promise(resolve => { resolveOld = resolve; });
    const oldGeneration = io.refreshMonitorLatency(0, 'generation');
    host.monitorLatencySeconds = async () => 0.04;
    await io.refreshMonitorLatency(0, 'generation');
    latency.recordCompensationFrames();
    resolveOld(0.08);
    await oldGeneration;
    const afterNewerTake = { seconds: latency.cpalOutSeconds(), frozen: latency.snapshot().frozen };
    host.monitorLatencySeconds = () => new Promise(resolve => { resolveOld = resolve; });
    const pendingGeneration = io.refreshMonitorLatency(0, 'generation');
    latency.recordCompensationFrames();
    resolveOld(0.12);
    await pendingGeneration;
    const afterFastTake = { seconds: latency.cpalOutSeconds(), frozen: latency.snapshot().frozen };
    await io.disarmMonitor(0);
    // Real callers can overlap: per-slot arm awaits its reply while the global buffer setter runs.
    const devices = await import('/src/audio/audio-devices.ts');
    resolveOld = null;
    host.monitorLatencySeconds = () => new Promise(resolve => { resolveOld = resolve; });
    const pendingArm = io.armMonitor(0);
    while (!resolveOld) await new Promise(resolve => originalTimeout(resolve, 0));
    host.setBufferSize = async () => {};
    host.monitorLatencySeconds = async () => 0.05;
    await devices.setBufferSize(128);
    latency.recordCompensationFrames();
    resolveOld(0.01);
    await pendingArm;
    const armVersusBuffer = { seconds: latency.cpalOutSeconds(), frozen: latency.snapshot().frozen };
    await io.disarmMonitor(0);

    // Both slots may monitor under WASAPI. When the most recently armed slot disappears, the other
    // remains native-monitored and must become a fresh compensation generation rather than falling
    // back to the zero-compensation synth/mic baseline.
    host.monitorLatencySeconds = async (slot) => slot === 0 ? 0.011 : 0.022;
    await io.armMonitor(0);
    await io.armMonitor(1);
    latency.recordCompensationFrames(); // freeze slot 1's generation
    let resolveSurvivor;
    host.monitorLatencySeconds = (slot) => slot === 0
      ? new Promise(resolve => { resolveSurvivor = resolve; })
      : Promise.resolve(0.022);
    await io.disarmMonitor(1);
    const survivorBeforeTake = {
      monitors: io.monitorArmed(),
      slot: latency.armedSlot(),
      seconds: latency.cpalOutSeconds(),
      frozen: latency.snapshot().frozen,
    };
    latency.recordCompensationFrames();
    const survivorAfterTake = {
      slot: latency.armedSlot(),
      seconds: latency.cpalOutSeconds(),
      frozen: latency.snapshot().frozen,
    };
    resolveSurvivor(0.099);
    await Promise.resolve();
    await Promise.resolve();
    const survivorAfterLateReply = {
      slot: latency.armedSlot(),
      seconds: latency.cpalOutSeconds(),
      frozen: latency.snapshot().frozen,
    };
    await io.disarmMonitor(0);
    window.setTimeout = originalTimeout;
    return {
      afterRearm, afterNewerTake, staleTimerReads, afterFastTake, armVersusBuffer,
      survivorBeforeTake, survivorAfterTake, survivorAfterLateReply,
    };
  });
  console.log(JSON.stringify(result));
  assert.equal(result.afterRearm, 0.03, 'an old monitor reply must not overwrite a rearmed monitor');
  assert.deepEqual(result.afterNewerTake, { seconds: 0.04, frozen: true }, 'an old configuration reply must not reopen or overwrite a take\'s frozen compensation');
  assert.equal(result.staleTimerReads, 0, 'an old configuration timer must not start a new monitor query');
  assert.deepEqual(result.afterFastTake, { seconds: 0.04, frozen: true }, 'a reply within the current generation must respect its first-use freeze');
  assert.deepEqual(result.armVersusBuffer, { seconds: 0.05, frozen: true }, 'an in-flight monitor arm must not overwrite a newer buffer configuration');
  assert.deepEqual(
    result.survivorBeforeTake,
    { monitors: [true, false], slot: 0, seconds: 0.011, frozen: false },
    'disarming the registered slot must reopen compensation for a surviving native monitor',
  );
  assert.deepEqual(
    result.survivorAfterTake,
    { slot: 0, seconds: 0.011, frozen: true },
    'an immediate first take must freeze the survivor\'s last accepted latency, not zero',
  );
  assert.deepEqual(
    result.survivorAfterLateReply,
    { slot: 0, seconds: 0.011, frozen: true },
    'the promoted monitor\'s late latency reply must not overwrite its frozen first take',
  );
});
