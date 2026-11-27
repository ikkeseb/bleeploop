// Stereo float32 monitor recording at the engine's own rate. Find six distinct note bursts,
// then compare whole bars without gain normalization. A silent, mistimed or extra repeat fails.
import assert from 'node:assert/strict';

export function measure(samples, device, take) {
  const { sampleRate: rate, alignFrames, inputFrames, block } = device;
  const bar = take.frames;
  const length = samples.length / 2;
  assert(Number.isInteger(length));
  let peak = 0;
  for (const s of samples) {
    assert(Number.isFinite(s), 'Invalid monitor sample');
    peak = Math.max(peak, Math.abs(s));
  }
  assert(peak > 0.0001 && peak < 0.05, `Monitor peak ${peak}, expected a quiet nonzero signal`);
  const threshold = Math.max(0.00003, peak * 0.03);
  const bursts = [];
  let last = -rate;
  for (let n = 0; n < length; n++) {
    if (Math.max(Math.abs(samples[n * 2]), Math.abs(samples[n * 2 + 1])) < threshold) continue;
    if (n - last > rate * 0.08) bursts.push(n);
    last = n;
  }
  assert.equal(bursts.length, 6, `Expected two notes three times, got onsets at frames ${bursts}`);
  assert(last + bar < length, 'Less than one silent bar follows stop');
  const start = bursts[0] - Math.round(rate * 0.1);
  assert(start >= 0 && start + 3 * bar <= length, 'Incomplete three-bar recording');
  // instruments.rs subtracts its 128-frame scheduling lead from the record tap's input delay.
  // The take compensates the full round trip plus compressor.rs's six-millisecond pre-delay.
  const recordDelay = Math.max(0, inputFrames - 128);
  const limiterDelay = Math.min(1023, Math.floor(Math.fround(Math.fround(0.006) * rate)));
  const expectedFirst = bar - alignFrames + recordDelay - limiterDelay;
  const errors = [];
  const lags = [];
  for (let cycle = 1; cycle <= 2; cycle++) {
    const rough = bursts[cycle * 2] - bursts[0];
    let lag = rough;
    let best = Infinity;
    const radius = Math.round(rate * 0.001);
    const earliest = Math.max(rough - radius, -start);
    const latest = Math.min(rough + radius, length - start - bar);
    assert(earliest <= latest, `Replay ${cycle} does not contain a full bar`);
    for (let shift = earliest; shift <= latest; shift++) {
      let diff = 0;
      for (let n = start; n < start + bar; n += 32) {
        const i = 2 * n;
        diff += (samples[i] - samples[i + 2 * shift]) ** 2;
      }
      if (diff < best) { best = diff; lag = shift; }
    }
    lags.push(lag);
    const channelErrors = [];
    for (let ch = 0; ch < 2; ch++) {
      let energy = 0;
      let difference = 0;
      for (let n = start; n < start + bar; n++) {
        const i = 2 * n + ch;
        energy += samples[i] ** 2;
        difference += (samples[i] - samples[i + 2 * lag]) ** 2;
      }
      const error = Math.sqrt(difference / energy);
      channelErrors.push(error);
      assert(error <= 0.05, `Replay ${cycle}, channel ${ch + 1}: relative RMS waveform error ${error}, lag ${lag}`);
    }
    errors.push(channelErrors);
  }
  assert(Math.abs(lags[0] - expectedFirst) <= Math.max(2, Math.round(rate * 0.001)),
    `First replay ${lags[0]} frames after live, expected ${expectedFirst} (block ${block})`);
  assert(Math.abs(lags[1] - lags[0] - bar) <= 2, `Repeat period ${lags[1] - lags[0]} frames, expected ${bar}`);
  return { peak, onsets: bursts, liveToReplayFrames: lags[0], expectedLiveToReplayFrames: expectedFirst,
    repeatFrames: lags[1] - lags[0], relativeRmsByReplayAndChannel: errors, silentTailFrames: length - last - 1 };
}
