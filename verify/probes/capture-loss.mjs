/**
 * Drops one actual capture packet during a take/layer and reports the matching producer loss:
 * an overdub rolls back to the pre-dub PCM and undo buffer, and a FIXED-length take is discarded
 * (EMPTY, no active recorder). `verify/guards/capture-packets.mjs` separately executes a true
 * full-ring producer drop.
 * Run: pnpm probe capture-loss [--url=<server>]
 * Drives the real looper and capture ring in Chromium; it does not establish native ring-buffer
 * behaviour under a real device xrun.
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

await probe(async ({ open }) => {
  const { page } = await open();
  const result = await page.evaluate(async () => {
    const lf = window.__lf;
    await lf.looper.init();
    lf.recordLatency.setEnabled(false);
    const { engineState } = await import('/src/audio/looper/state.ts');
    const { CAPTURE_PACKET_SIZE } = await import('/src/audio/capture-packet.ts');
    const { defaultFxStates } = await import('/src/audio/fx/fx.ts');
    const sr = lf.engine.ctx.sampleRate;
    const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
    const until = async (fn) => {
      const deadline = performance.now() + 4000;
      while (!fn() && performance.now() < deadline) await pause(5);
      if (!fn()) throw new Error('Capture loss test timed out');
    };
    const inject = () => {
      const ring = engineState.ring;
      const original = ring.pop;
      const discarded = new Float64Array(CAPTURE_PACKET_SIZE);
      let injected = false;
      ring.pop = function (target, count) {
        if (this.available_read() >= CAPTURE_PACKET_SIZE) {
          original.call(this, discarded, CAPTURE_PACKET_SIZE);
          Atomics.add(engineState.heartbeat, 1, 128);
          injected = true;
          ring.pop = original;
        }
        return original.call(this, target, count);
      };
      return () => injected;
    };
    lf.looper.clearAll();
    const original = Float32Array.from({ length: sr }, (_, k) => Math.sin(k / 67) * 0.125);
    const olderUndo = Float32Array.from(original, (value) => value * 0.5);
    await lf.looper.loadSession({ bpm: 240, bars: 1, masterLengthFrames: sr,
      tracks: [{ index: 0, pcm: original, volume: 0, muted: false, reversed: false, fx: defaultFxStates() }] });
    engineState.tracks[0].undoBuf = olderUndo.slice();
    await lf.looper.recDub(0);
    const dubInjected = inject();
    await until(dubInjected);
    lf.looper.playStop(0);
    await until(() => lf.looper.stateOf(0) !== 'OVERDUBBING');
    const track = engineState.tracks[0];
    const dub = { state: track.state, exact: track.record.subarray(0, sr).every((v, k) => v === original[k]),
      undoExact: track.undoBuf?.every((v, k) => v === olderUndo[k]), silent: track.source === null && track.retiringSources.size === 0 };

    lf.looper.clearAll();
    lf.clock.setBpm(240);
    lf.looper.setFixedLengthEnabled(true);
    lf.looper.setFixedLengthBars(1);
    await lf.looper.recDub(0);
    await until(() => !engineState.tracks[0].armed);
    const takeInjected = inject();
    await until(takeInjected);
    await until(() => lf.looper.stateOf(0) !== 'RECORDING');
    const take = { state: lf.looper.stateOf(0), frames: lf.looper.exportSnapshot().masterLengthFrames,
      active: engineState.activeRecordIndex };
    lf.looper.clearAll();
    lf.looper.setFixedLengthEnabled(false);
    return { dub, take, dropped: lf.looper.captureOverruns() };
  });
  console.log(JSON.stringify(result));
  assert.deepEqual(result.dub, { state: 'STOPPED', exact: true, undoExact: true, silent: true });
  assert.deepEqual(result.take, { state: 'EMPTY', frames: 0, active: -1 });
  assert.equal(result.dropped, 256);
});
