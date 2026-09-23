/** Measures the real per-track FX graph's pitch stage offline: unused pitch allocates no delay lines,
 * bypassed/reset output preserves the dry signal, enabling it mid-render costs no discontinuity and
 * reuses its delay lines across repeated enable/bypass/reset, and a live chain in the running graph
 * applies its stored pitch on first enable without dropping an audio block. `--baseline` reads a
 * reviewer-saved copy at logs/fx-baseline.ts and reports its resource cost instead, for comparison.
 * Timings are offline DSP render work, not whole-app CPU or physical output latency.
 * Run: pnpm probe fx-pitch-cost [--baseline]
 */
import assert from 'node:assert/strict';
import { flag, probe } from '../harness/probe.ts';

const baseline = flag('baseline');

await probe(async ({ open }) => {
  const { page } = await open();
  const results = await page.evaluate(async (baseline) => {
    await window.__lf.engine.start();
    const path = baseline ? '/logs/fx-baseline.ts' : '/src/audio/fx/fx.ts';
    const { FxChain, defaultFxStates } = await import(path);
    const transformed = await (await fetch('/src/audio/fx/fx.ts')).text();
    const tonePath = transformed.match(/from\s+["']([^"']*\/tone[^"']*)["']/)?.[1];
    if (!tonePath) throw new Error('Could not resolve application Tone module');
    const { OfflineContext, Gain } = await import(tonePath);
    const sr = 44100;
    const render = async (count, dynamic = false, enabled = false) => {
      const seconds = dynamic ? 2.4 : 10;
      const offline = new OfflineContext(1, seconds, sr);
      const createDelay = offline.createDelay;
      let delayNodes = 0;
      offline.createDelay = function (...args) { delayNodes++; return createDelay.apply(this, args); };
      const dest = new Gain({ gain: 1 / count, context: offline });
      dest.toDestination();
      const reverbBus = new Gain({ gain: 0, context: offline });
      const states = defaultFxStates();
      states[1].params.semitones = 12;
      states[1].bypassed = !enabled;
      const chains = Array.from({ length: count }, () => new FxChain(states, { context: offline, dest, reverbBus }));
      const beforeEnable = delayNodes;
      const buffer = offline.createBuffer(1, Math.ceil(seconds * sr), sr);
      const input = buffer.getChannelData(0);
      for (let k = 0; k < input.length; k++) input[k] = 0.1 * Math.sin(2 * Math.PI * 220 * k / sr);
      const source = offline.createBufferSource();
      source.buffer = buffer;
      const rawInput = (node) => node.input ? rawInput(node.input) : node;
      for (const chain of chains) source.connect(rawInput(chain.input));
      source.start();
      const timing = { anchor: 0, beatPeriod: 0.5 };
      for (const chain of chains) chain.setTiming(timing);
      if (dynamic) {
        offline.setTimeout(() => chains[0].nodes[1].setBypass(false), 0.4);
        offline.setTimeout(() => chains[0].nodes[1].setBypass(true), 0.95);
        offline.setTimeout(() => chains[0].nodes[1].setBypass(false), 1.4);
        offline.setTimeout(() => chains[0].setState(defaultFxStates()), 1.9);
      }
      const start = performance.now();
      const rendered = await offline.render();
      const renderMs = performance.now() - start;
      const pcm = rendered.getChannelData(0);
      const pcmHash = Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', pcm.buffer)))
        .map((byte) => byte.toString(16).padStart(2, '0')).join('');
      const error = (a, b) => {
        let max = 0;
        for (let k = Math.ceil(a * sr); k < Math.floor(b * sr); k++) max = Math.max(max, Math.abs(pcm[k] - input[k]));
        return max;
      };
      const frequency = (a, b) => {
        let crossings = 0;
        let energy = 0;
        for (let k = Math.ceil(a * sr); k < Math.floor(b * sr); k++) {
          if (pcm[k] <= 0 && pcm[k + 1] > 0) crossings++;
          energy += pcm[k] * pcm[k];
        }
        return { hz: crossings / (b - a), rms: Math.sqrt(energy / ((b - a) * sr)) };
      };
      let maxJump = 0;
      for (let k = 1; k < pcm.length; k++) maxJump = Math.max(maxJump, Math.abs(pcm[k] - pcm[k - 1]));
      const result = { count, dynamic, enabled, beforeEnable, delayNodes, renderMs, maxJump, pcmHash,
        dryError: dynamic ? Math.max(error(0.1, 0.35), error(1.15, 1.35), error(2.15, 2.35)) : error(0.1, 9.9),
        shifted: dynamic ? [frequency(0.7, 0.9), frequency(1.65, 1.85)] : [],
        resetBypassed: chains[0].nodes[1].isBypassed(), resetPitch: chains[0].nodes[1].getParam('semitones') };
      source.disconnect();
      for (const chain of chains) chain.dispose();
      dest.dispose(); reverbBus.dispose(); offline.dispose();
      return result;
    };
    const results = [];
    for (let run = 0; run < 3; run++) results.push(await render(5));
    results.push(await render(5, false, true));
    results.push(await render(1, true));
    return results;
  }, baseline);
  console.log(JSON.stringify({ baseline, results }));
  for (const result of results) {
    // Tone's CrossFade uses a 1024-point abs() waveshaper that misses the exact dry endpoint.
    // The original chain already deviates by 0.000614 at input peak 0.1. Preserve it within 1%.
    if (!result.enabled) assert.ok(result.dryError < 0.001, 'bypassed output and reset must preserve the dry signal within 1%');
    if (!baseline && !result.enabled) assert.equal(result.beforeEnable, result.count, 'unused pitch must not allocate its delay lines');
    assert.ok(result.maxJump < 0.03, 'activating pitch must not introduce a discontinuity');
    if (result.dynamic) {
      assert.equal(result.delayNodes, result.count + 3, 'repeated enables and reset must reuse the pitch delay lines');
      for (const sample of result.shifted) {
        assert.ok(Math.abs(sample.hz - 440) < 10, 'saved +12 semitones must sound on each activation');
        assert.ok(sample.rms > 0.02, 'enabled pitch must produce audible PCM');
      }
      assert.equal(result.resetBypassed, true);
      assert.equal(result.resetPitch, 0, 'CLEAR/import reset must retain the default pitch parameter');
    }
  }
  const live = await page.evaluate(async (baseline) => {
    const lf = window.__lf;
    const ctx = lf.engine.ctx;
    const { FxChain, defaultFxStates } = await import(baseline ? '/logs/fx-baseline.ts' : '/src/audio/fx/fx.ts');
    const states = defaultFxStates();
    states[1].params.semitones = 12;
    lf.master.setMuted(true);
    const chain = new FxChain(states);
    const raw = (node) => node.output ? raw(node.output) : node;
    const input = (node) => node.input ? input(node.input) : node;
    const source = ctx.createOscillator();
    source.frequency.value = 220;
    const gain = ctx.createGain(); gain.gain.value = 0.1;
    source.connect(gain); gain.connect(input(chain.input)); source.start();
    const moduleUrl = URL.createObjectURL(new Blob([`
      class Capture extends AudioWorkletProcessor {
        next = -1; // Chromium can repeat a quantum's currentFrame (capture-processor.ts); a real input never repeats
        constructor(options) {
          super(); this.first = options.processorOptions.first;
          this.samples = new Float32Array(options.processorOptions.frames); this.sent = false;
        }
        process(inputs) {
          if (this.sent) return false;
          const base = Math.max(currentFrame, this.next);
          this.next = base + 128;
          const channel = inputs[0]?.[0];
          for (let k = 0; k < 128; k++) {
            const index = base + k - this.first;
            if (index >= 0 && index < this.samples.length) this.samples[index] = channel?.[k] ?? 0;
          }
          if (base + 128 >= this.first + this.samples.length) {
            this.sent = true; this.port.postMessage(this.samples, [this.samples.buffer]);
          }
          return true;
        }
      }
      registerProcessor('pitch-live-capture', Capture);
    `], { type: 'text/javascript' }));
    await ctx.audioWorklet.addModule(moduleUrl); URL.revokeObjectURL(moduleUrl);
    const first = Math.ceil((ctx.currentTime + 0.15) * ctx.sampleRate);
    const meter = new AudioWorkletNode(ctx, 'pitch-live-capture', {
      processorOptions: { first, frames: Math.ceil(1.8 * ctx.sampleRate) },
    });
    raw(chain.nodes[1].output).connect(meter); meter.connect(ctx.destination); // Recorder output is silence.
    const captured = new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error('Live pitch capture timed out')), 5000);
      meter.port.onmessage = ({ data }) => { clearTimeout(timer); resolve(data); };
    });
    const until = async (seconds) => {
      while (ctx.currentTime < first / ctx.sampleRate + seconds) await new Promise((resolve) => setTimeout(resolve, 2));
    };
    await until(0.4);
    const start = performance.now(); chain.nodes[1].setBypass(false);
    const enableMs = performance.now() - start;
    await until(1.05); chain.setState(defaultFxStates());
    const pcm = await captured;
    let silentRun = 0, maxSilentRun = 0, maxJump = 0;
    for (let k = 1; k < pcm.length; k++) {
      silentRun = Math.abs(pcm[k]) < 1e-7 ? silentRun + 1 : 0;
      maxSilentRun = Math.max(maxSilentRun, silentRun);
      maxJump = Math.max(maxJump, Math.abs(pcm[k] - pcm[k - 1]));
    }
    const frequency = (a, b) => {
      let crossings = 0;
      for (let k = Math.ceil(a * ctx.sampleRate); k < Math.floor(b * ctx.sampleRate); k++) {
        if (pcm[k] <= 0 && pcm[k + 1] > 0) crossings++;
      }
      return crossings / (b - a);
    };
    const result = { enableMs, maxSilentRun, maxJump,
      beforeHz: frequency(0.1, 0.3), pitchHz: frequency(0.75, 0.95), resetHz: frequency(1.4, 1.7) };
    source.stop(); source.disconnect(); gain.disconnect(); meter.disconnect(); chain.dispose();
    return result;
  }, baseline);
  console.log(JSON.stringify({ live }));
  assert.ok(live.maxSilentRun < 8, 'live first-enable and reset must not drop an audio block');
  assert.ok(live.maxJump < 0.03, 'live switching must retain smooth ramps');
  assert.ok(Math.abs(live.beforeHz - 220) < 10 && Math.abs(live.resetHz - 220) < 10);
  assert.ok(Math.abs(live.pitchHz - 440) < 10, 'first enable must apply the stored pitch');
});
