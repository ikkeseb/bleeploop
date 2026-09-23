/** Measures the real per-track rhythmic FX (tremolo/gate-style rate division) at AudioWorklet frame
 * timestamps: two chains driven off the same timing agree with the expected on/off grid at two BPMs,
 * and a delay chain reused across CLEAR picks up its next-take beat period. Without `--baseline`, also
 * renders the production Tone module offline for the same grid and drives the real lane dispatcher
 * (loadSession, clear, record at a new BPM) to check a retained FX chain's delay time refreshes to the
 * new tempo. `--baseline`/`LF_FX_BASELINE=1` selects a temporary fx-baseline.ts copied from HEAD by
 * the reviewer, to compare cost/behaviour against a change. Measures grid timing and PCM only; it
 * cannot see native audio latency or anything below the Web Audio graph.
 * Run: pnpm probe fx-grid [--baseline]
 */
import assert from 'node:assert/strict';
import { flag, probe } from '../harness/probe.ts';

const baseline = flag('baseline') || process.env.LF_FX_BASELINE === '1';

await probe(async ({ open }) => {
  const { page } = await open();
  const results = await page.evaluate(async (baseline) => {
    const lf = window.__lf;
    await lf.engine.start();
    const ctx = lf.engine.ctx;
    const { FxChain, defaultFxStates } = await import(baseline
      ? '/src/audio/fx/fx-baseline.ts' : '/src/audio/fx/fx.ts');
    const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
    // Tone's offline context wraps native nodes; structural endpoints work in both contexts.
    const input = (node) => node.input ? input(node.input) : node;
    const output = (node) => node.output ? output(node.output) : node;
    const moduleUrl = URL.createObjectURL(new Blob([`
      class Capture extends AudioWorkletProcessor {
        next = -1; // Chromium can repeat a quantum's currentFrame (capture-processor.ts); a real input never repeats
        constructor(options) {
          super();
          this.first = options.processorOptions.first;
          this.frames = options.processorOptions.frames;
          this.a = new Float32Array(this.frames);
          this.b = new Float32Array(this.frames);
          this.sent = false;
        }
        process(inputs) {
          if (this.sent) return false;
          const base = Math.max(currentFrame, this.next);
          this.next = base + 128;
          const channels = inputs[0];
          for (let k = 0; k < 128; k++) {
            const index = base + k - this.first;
            if (index >= 0 && index < this.frames) {
              this.a[index] = channels[0]?.[k] ?? 0;
              this.b[index] = channels[1]?.[k] ?? 0;
            }
          }
          if (base + 128 >= this.first + this.frames) {
            this.sent = true;
            this.port.postMessage({a: this.a, b: this.b, first: this.first});
          }
          return true;
        }
      }
      registerProcessor('fx-grid-capture', Capture);
    `], { type: 'text/javascript' }));
    await ctx.audioWorklet.addModule(moduleUrl);
    URL.revokeObjectURL(moduleUrl);
    const capture = async (a, b, seconds) => {
      const merger = ctx.createChannelMerger(2);
      output(a).connect(merger, 0, 0);
      output(b).connect(merger, 0, 1);
      const node = new AudioWorkletNode(ctx, 'fx-grid-capture', {
        processorOptions: { first: Math.ceil((ctx.currentTime + 0.15) * ctx.sampleRate), frames: Math.ceil(seconds * ctx.sampleRate) },
      });
      merger.connect(node);
      node.connect(ctx.destination);
      const data = await new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error('FX capture timed out')), (seconds + 3) * 1000);
        node.port.onmessage = ({ data }) => { clearTimeout(timer); resolve(data); };
      });
      output(a).disconnect(merger);
      output(b).disconnect(merger);
      merger.disconnect();
      node.disconnect();
      return data;
    };
    const rows = [];
    for (const bpm of [120, 137]) {
      lf.clock.setBpm(bpm);
      const timing = { anchor: ctx.currentTime + 0.04, beatPeriod: Math.round(240 / bpm * ctx.sampleRate) / ctx.sampleRate / 4 };
      const states = defaultFxStates();
      states[2].bypassed = false;
      const a = new FxChain(states);
      a.setTiming?.(timing);
      await pause(93);
      const b = new FxChain(states);
      b.setTiming?.(timing);
      const source = ctx.createConstantSource();
      source.offset.value = 0.2;
      source.connect(input(a.input));
      source.connect(input(b.input));
      source.start();
      for (const division of [0, 1, 2, 3]) {
        a.nodes[2].setParam('rate', division);
        b.nodes[2].setParam('rate', division);
        const period = timing.beatPeriod * [1, 0.5, 0.75, 0.25][division];
        const recorded = await capture(a.nodes[2].output, b.nodes[2].output, Math.max(0.6, period * 2));
        let checked = 0, wrongA = 0, wrongB = 0, disagreement = 0;
        for (let k = 0; k < recorded.a.length; k++) {
          const time = (recorded.first + k) / ctx.sampleRate;
          const phase = (((time - timing.anchor) % period) + period) % period;
          // Exclude interpolation at the two square-wave edges, not entire render quanta.
          if (Math.min(phase, Math.abs(phase - period / 2), period - phase) < 0.003) continue;
          const expected = phase < period / 2;
          const onA = recorded.a[k] > 0.1;
          const onB = recorded.b[k] > 0.1;
          checked++;
          wrongA += Number(onA !== expected);
          wrongB += Number(onB !== expected);
          disagreement += Number(onA !== onB);
        }
        rows.push({ bpm, division, checked, wrongA, wrongB, disagreement });
      }
      // Simulates an existing chain retained across clear, then refreshed on the next first take.
      const changed = { anchor: ctx.currentTime, beatPeriod: 1 };
      a.setTiming?.(changed);
      await pause(160);
      rows.push({ reusedDelaySeconds: a.nodes[3].delay.delayTime.value, expected: 0.5 });
      source.stop();
      source.disconnect();
      a.dispose();
      b.dispose();
    }
    if (!baseline) {
      // Use the same installed Tone module as production, including Vite's dependency version key.
      const transformed = await (await fetch('/src/audio/fx/fx.ts')).text();
      const tonePath = transformed.match(/from\s+["']([^"']*\/tone[^"']*)["']/)?.[1];
      if (!tonePath) throw new Error('Could not resolve the production Tone module');
      const { Offline, Gain } = await import(tonePath);
      const beatPeriod = Math.round(240 / 137 * ctx.sampleRate) / ctx.sampleRate / 4;
      for (const division of [0, 1, 2, 3]) {
        const period = beatPeriod * [1, 0.5, 0.75, 0.25][division];
        const anchor = -0.087;
        const rendered = await Offline((offline) => {
          const raw = offline.rawContext;
          const fx = defaultFxStates();
          fx[2] = { bypassed: false, params: { rate: division } };
          const silentSend = new Gain(0);
          const chain = new FxChain(fx, { dest: raw.destination, reverbBus: silentSend });
          chain.setTiming({ anchor, beatPeriod });
          const source = raw.createConstantSource();
          source.offset.value = 0.2;
          source.connect(input(chain.input));
          source.start(0);
        }, 1, 1, ctx.sampleRate);
        const samples = rendered.getChannelData(0);
        let checked = 0, wrong = 0;
        for (let k = Math.ceil(ctx.sampleRate * 0.15); k < samples.length; k++) {
          const phase = (((k / ctx.sampleRate - anchor) % period) + period) % period;
          if (Math.min(phase, Math.abs(phase - period / 2), period - phase) < 0.003) continue;
          checked++;
          wrong += Number((samples[k] > 0.1) !== (phase < period / 2));
        }
        rows.push({ offline: true, division, checked, wrongA: wrong, wrongB: wrong, disagreement: 0 });
      }

      // Actual lane dispatcher: construct FX at 120, clear, record a new master at 240, enable
      // delay without touching its time selector. Its default eighth note must now be 125 ms.
      await lf.looper.init();
      lf.looper.clearAll();
      const sr = ctx.sampleRate;
      await lf.looper.loadSession({
        bpm: 120, bars: 1, masterLengthFrames: sr * 2,
        tracks: [{ index: 0, pcm: new Float32Array(sr * 2), volume: 1, muted: false, reversed: false, fx: defaultFxStates() }],
      });
      const { engineState } = await import('/src/audio/looper/state.ts');
      const retained = engineState.tracks[0].fx;
      lf.looper.clearAll();
      lf.clock.setBpm(240);
      lf.looper.setAutoRecordEnabled(false);
      lf.looper.setFixedLengthEnabled(true);
      lf.looper.setFixedLengthBars(1);
      await lf.looper.recDub(0);
      const deadline = performance.now() + 7000;
      while (lf.looper.stateOf(0) !== 'PLAYING' && performance.now() < deadline) await pause(25);
      if (lf.looper.stateOf(0) !== 'PLAYING') throw new Error('Reused lane failed to finish recording');
      lf.looper.setFxBypass(0, 3, false);
      await pause(160);
      if (engineState.tracks[0].fx !== retained) throw new Error('Probe did not exercise a retained FX chain');
      rows.push({ realReusedLane: true, reusedDelaySeconds: retained.nodes[3].delay.delayTime.value, expected: 0.125 });
      lf.looper.clearAll();
    }
    return rows;
  }, baseline);
  console.log(JSON.stringify({ baseline, results }));
  for (const result of results) {
    if ('checked' in result) {
      assert.ok(result.checked > 1000);
      assert.ok(result.wrongA / result.checked < 0.005, `lane A off-grid: ${JSON.stringify(result)}`);
      assert.ok(result.wrongB / result.checked < 0.005, `lane B off-grid: ${JSON.stringify(result)}`);
      assert.ok(result.disagreement / result.checked < 0.005, `lanes disagree: ${JSON.stringify(result)}`);
    } else {
      assert.ok(Math.abs(result.reusedDelaySeconds - result.expected) < 1e-6, `stale delay: ${JSON.stringify(result)}`);
    }
  }
});
