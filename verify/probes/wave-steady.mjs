/**
 * A recording lane's waveform holds still while its bins arrive. Engine mode on the web engine fake
 * (`src/platform/host.web.ts`), booted as `engine-seam.mjs` boots it; the probe scripts two takes through
 * `__lf.native`: a first free take (no loop yet), then a later free take (E10) over a 1-bar loop. Bins
 * arrive at irregular intervals (5–70 ms), each update a little behind the clock (the capture's lag) and
 * with a clock anchor taken up to 8 ms late, so the extrapolated record head jitters as the real feed's
 * does. The clock runs 4× (first take) and 2× (later take) real time through the anchor's rate, so the
 * spans grow within seconds. Every take is quiet but for one loud burst early on; after each update the
 * probe reads, in the next drawn frame, the burst's left edge on the lane's canvas (the first rec-red
 * column of a row the quiet bins never reach) and the span that frame was drawn over (`__lf.drawnSpan`).
 *
 * Proves, in each of the wave's four looks (`src/ui/looper/wave-looks.ts`): while the span holds, the
 * burst stays within 1 device px across the updates, wherever the head is and however many bins arrived
 * (before, the bins stretched to the head, or across the lane on a first take, and the burst moved with
 * every update), and it sits where its frame falls in the span, within the look's own grain (`LOOKS`):
 * the plain body places each bin to the device px; the stripes and the light bars light the whole
 * element their slice falls in, so its edge sits up to one pitch from the frame; the dense look widens a
 * peak by a slice each side and smooths it, so up to two pitches. It moves only when the span grows: a
 * first take's window doubles (4 bars at the tempo: 8 s at 120 BPM), a free later take spans the loops it
 * has reached. And nothing is drawn past the recorded boundary (the bins' end over the span), but the
 * record head and the tape's dashes on the centre line. Every span needs 4 samples; a run too slow to take
 * them fails.
 *
 * Cannot see the native engine's real feed cadence, the web looper's own takes, WebView2 or the eye.
 * Run: pnpm probe wave-steady
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

const RATE = 48000;
const BAR = 2 * RATE; // one 4/4 bar at 120 BPM
const PEAK_FRAMES = 1024; // the engine's bin (`lf-engine/src/overview.rs`), the web looper's `PEAK_FRAMES`
const BURST = 3; // bins

const lane = (state, extra = {}) => ({
  state,
  length: 0,
  armed: false,
  autoArmed: false,
  canUndo: false,
  canReverse: false,
  reversed: false,
  stopAt: null,
  fading: false,
  retakePass: 0,
  ...extra,
});
const laneEvent = (i, info, frame = 0) => ({ Lane: { frame, lane: i, info } });
const transport = (master, locked, bpm = 120) => ({ Transport: { frame: 0, master, bpm, locked } });

/**
 * The looks, each with how far (CSS px) its burst's left edge may sit from the burst's frame: the plain
 * body to the device px; the stripes one stripe pitch (aimed at 8.5 px, at most 12 when a bar's count
 * snaps to the meter) since the whole stripe lights; the light bars one 5 px pitch; the dense look's
 * neighbour max and smoothing two 3 px pitches.
 */
const LOOKS = { plain: 0, stripes: 12, bars: 5, dense: 6 };

// A fixed-seed Park–Miller generator: a red run replays its intervals.
let seed = 20260928;
const rand = () => (seed = (seed * 16807) % 2147483647) / 2147483647;

await probe(async ({ open }) => {
  for (const [look, grain] of Object.entries(LOOKS)) await run(open, look, grain);
});

async function run(open, look, grain) {
  // Every look replays the same intervals.
  seed = 20260928;
  const { page } = await open({
    viewport: { width: 1600, height: 900 },
    init: (p) =>
      p.addInitScript((l) => {
        window.__lfEngineFake = true;
        localStorage.setItem('lf.waveLook', l);
      }, look),
  });
  const slack = 1 + Math.ceil(grain * (await page.evaluate(() => devicePixelRatio)));
  let seq = 0;
  const emit = (frame) => page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], ...frame });

  await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });
  await emit({
    reset: true,
    settings: [],
    events: [...[0, 1, 2, 3, 4].map((i) => laneEvent(i, lane('Empty'))), transport(0, false), { Selected: { frame: 0, lane: 0 } }],
    anchor: { frame: 0, atMs: Date.now(), rate: RATE, grid: 0 },
    meter: { peak: [0, 0], clip: [false, false] },
  });

  /**
   * The burst's left edge on lane `i`'s canvas in the next drawn frame, the span it was drawn over, and the
   * rec-red pixels past the recorded boundary (the bins held over the span), off the centre line's tape
   * and the record head's columns (the ones red at the row's top, which no wave reaches).
   */
  const sample = (i) =>
    page.evaluate(
      async ({ k, peakFrames }) => {
        const { looper } = await import('/src/ui/state/audio.ts');
        await new Promise((r) => requestAnimationFrame(r));
        const c = document.querySelectorAll('.lp-lane')[k].querySelector('canvas');
        const px = c.getContext('2d').getImageData(0, 0, c.width, c.height).data;
        const red = (x, y) => {
          const o = 4 * (y * c.width + x);
          return px[o + 3] > 40 && px[o] > 150 && px[o] - px[o + 1] > 80;
        };
        // 30 % down: the burst's outer envelope (±0.9) crosses it, the quiet bins (±0.05) and the tape don't.
        const y30 = Math.round(c.height * 0.3);
        let edge = -1;
        for (let x = 0; x < c.width; x++) {
          if (red(x, y30)) {
            edge = x;
            break;
          }
        }
        // The span the frame we just read was rasterised over (`waveform.ts` `lane.lastSpan`), not a fresh
        // `looper.recSpanFrames` call: `recSpanFrames` reads `Date.now()`, which can cross a span boundary
        // during `frame()` or `getImageData` and grade an old-span bitmap against the new span.
        const span = window.__lf.drawnSpan(k);
        const view = { min: null, max: null, count: 0, version: 0 };
        looper.peaksInto(k, view);
        const end = Math.ceil(((view.count * peakFrames) / span) * c.width);
        const mid = c.height / 2;
        const tape = Math.max(2, Math.round(2 * devicePixelRatio));
        let past = 0;
        for (let x = end; x < c.width; x++) {
          if (red(x, 1) || (x > 0 && red(x - 1, 1)) || (x + 1 < c.width && red(x + 1, 1))) continue;
          for (let y = 2; y < c.height - 2; y++) if (Math.abs(y - mid) > tape && red(x, y)) past++;
        }
        return { edge, span, width: c.width, end, past };
      },
      { k: i, peakFrames: PEAK_FRAMES },
    );

  /**
   * Record lane `i` from device frame `start` (grid `grid`) for `ms` of real time on a clock `speed`×
   * real time, its burst at bin `burstAt`; one sample per peak update, once the head is well past the burst.
   */
  async function take(i, start, grid, speed, ms, burstAt) {
    const rate = RATE * speed;
    const t0 = Date.now();
    const frameAt = (t) => start + Math.floor(((t - t0) * rate) / 1000);
    await emit({ events: [laneEvent(i, lane('Recording'), start)], anchor: { frame: start, atMs: t0, rate, grid } });
    let count = 0;
    const samples = [];
    while (Date.now() - t0 < ms) {
      await page.waitForTimeout(5 + Math.floor(rand() * 65));
      const now = Date.now();
      const next = Math.max(count, Math.floor((frameAt(now - rand() * 30) - start) / PEAK_FRAMES));
      const from = Math.max(0, count - 1); // the last bin again: it was partial
      const amp = (b) => (b >= burstAt && b < burstAt + BURST ? 0.9 : 0.05);
      const bins = Array.from({ length: next - from }, (_, k) => amp(from + k));
      const frame = { peaks: next > count ? [{ lane: i, start: from, count: next, min: bins.map((v) => -v), max: bins }] : [] };
      if (rand() < 0.6) frame.anchor = { frame: frameAt(now), atMs: now - Math.floor(rand() * 8), rate, grid };
      await emit(frame);
      count = next;
      if (count > burstAt + 12) samples.push({ count, ...(await sample(i)) });
    }
    return samples;
  }

  /** Split `samples` into runs of one span; within each run the burst holds (±1 px) where its frame falls. */
  function assertSteady(name, samples, burstAt) {
    const runs = [];
    for (const s of samples) {
      if (runs.length && runs.at(-1)[0].span === s.span) runs.at(-1).push(s);
      else runs.push([s]);
    }
    for (const run of runs) {
      const edges = run.map((s) => s.edge);
      const counts = new Set(run.map((s) => s.count));
      const expected = Math.ceil(((burstAt * PEAK_FRAMES) / run[0].span) * run[0].width);
      console.log(`${look} ${name}: span ${run[0].span} frames, ${run.length} samples over ${counts.size} bin counts, burst at ${[...new Set(edges)].join('/')} px (its frame: ${expected})`);
      // A span sampled too thinly proves nothing about it: a slow run fails here, it never passes unchecked.
      assert.ok(run.length >= 4, `${look} ${name}: only ${run.length} samples over the span ${run[0].span} (needs 4)`);
      assert.ok(counts.size >= 4, `${look} ${name}: the bins kept arriving within the span ${run[0].span}`);
      assert.ok(Math.max(...edges) - Math.min(...edges) <= 1, `${look} ${name}: the burst holds within 1 px over the span ${run[0].span} (${edges.join(' ')})`);
      assert.ok(
        edges.every((e) => e >= 0 && Math.abs(e - expected) <= slack),
        `${look} ${name}: the burst sits at its frame in the span ${run[0].span}, within ${slack} px (${edges.join(' ')})`,
      );
    }
    const past = samples.filter((s) => s.past > 0);
    assert.equal(
      past.length,
      0,
      `${look} ${name}: nothing drawn past the recorded boundary (${past.map((s) => `${s.past} px past ${s.end} (count ${s.count}, span ${s.span})`).join(', ')})`,
    );
    return runs.map((run) => run[0].span);
  }

  // ── A first free take: the lane's window opens at 4 bars and doubles as the take passes it ─────────
  const first = await take(0, 0, 0, 4, 3200, 40);
  const firstSpans = assertSteady('first take', first, 40);
  assert.deepEqual(firstSpans, [4 * BAR, 8 * BAR], 'a first take opens 4 bars wide and doubles once in 12.8 s');

  // Commit it as a 1-bar loop.
  const loopEnd = 7 * BAR; // the grid: loops start on multiples of BAR
  await emit({
    events: [laneEvent(0, lane('Playing', { length: BAR, canReverse: true })), transport(BAR, true)],
    anchor: { frame: loopEnd, atMs: Date.now(), rate: RATE, grid: 0 },
  });

  // ── A later free take (E10): the lane spans the loops it has reached ─────────────────────────────
  const later = await take(1, 8 * BAR, 0, 2, 2600, 20);
  const laterSpans = assertSteady('later take', later, 20);
  assert.deepEqual(laterSpans, [BAR, 2 * BAR, 3 * BAR], 'a free later take spans the loops it reached');
  await page.close();
}
