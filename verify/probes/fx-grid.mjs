/** The export's offline wet render keeps the stutter (the tempo-synced gate) on the loop's grid: the real
 * `FxChain` from `src/session/offline-fx.ts`, rendered on Tone's OfflineContext with the timing
 * `src/session/render.ts` hands it (anchor −87 ms, 137 BPM), gates open for the first half of every
 * division at each of the four divisions. `--baseline` imports a reviewer-saved copy at
 * `src/session/offline-fx-baseline.ts` instead, to compare a change against it.
 *
 * Measures offline PCM only. The live lanes' FX are the engine's (lf-engine `tests/sound.rs`,
 * `tests/fx_filter_stutter.rs`); this cannot see them, device latency or anything audible.
 * Run: pnpm probe fx-grid [--baseline]
 */
import assert from 'node:assert/strict';
import { flag, probe } from '../harness/probe.ts';

const baseline = flag('baseline') || process.env.LF_FX_BASELINE === '1';

await probe(async ({ open }) => {
  const { page } = await open();
  const results = await page.evaluate(async (baseline) => {
    const { FxChain } = await import(baseline ? '/src/session/offline-fx-baseline.ts' : '/src/session/offline-fx.ts');
    const { defaultFxStates } = await import('/src/ui/state/fx-metadata.ts');
    // Use the same installed Tone module as production, including Vite's dependency version key.
    const transformed = await (await fetch('/src/session/offline-fx.ts')).text();
    const tonePath = transformed.match(/from\s+["']([^"']*\/tone[^"']*)["']/)?.[1];
    if (!tonePath) throw new Error('Could not resolve the production Tone module');
    const { Offline, Gain } = await import(tonePath);
    // Tone's offline context wraps native nodes; structural endpoints work in both contexts.
    const input = (node) => node.input ? input(node.input) : node;
    const sampleRate = 48000;
    const rows = [];
    const beatPeriod = Math.round(240 / 137 * sampleRate) / sampleRate / 4;
    for (const division of [0, 1, 2, 3]) {
      const period = beatPeriod * [1, 0.5, 0.75, 0.25][division];
      const anchor = -0.087;
      const rendered = await Offline((offline) => {
        const raw = offline.rawContext;
        const fx = defaultFxStates();
        fx[2] = { bypassed: false, params: { rate: division } };
        const silentSend = new Gain({ gain: 0, context: offline });
        const chain = new FxChain(fx, { context: offline, dest: raw.destination, reverbBus: silentSend });
        chain.setTiming({ anchor, beatPeriod });
        const source = raw.createConstantSource();
        source.offset.value = 0.2;
        source.connect(input(chain.input));
        source.start(0);
      }, 1, 1, sampleRate);
      const samples = rendered.getChannelData(0);
      let checked = 0, wrong = 0;
      for (let k = Math.ceil(sampleRate * 0.15); k < samples.length; k++) {
        const phase = (((k / sampleRate - anchor) % period) + period) % period;
        // Exclude interpolation at the two square-wave edges.
        if (Math.min(phase, Math.abs(phase - period / 2), period - phase) < 0.003) continue;
        checked++;
        wrong += Number((samples[k] > 0.1) !== (phase < period / 2));
      }
      rows.push({ offline: true, bpm: 137, division, checked, wrong });
    }
    return rows;
  }, baseline);
  console.log(JSON.stringify({ baseline, results }));
  assert.equal(results.length, 4, 'four divisions');
  for (const result of results) {
    assert.ok(result.checked > 1000);
    assert.ok(result.wrong / result.checked < 0.005, `off-grid: ${JSON.stringify(result)}`);
  }
});
