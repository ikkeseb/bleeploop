/**
 * A dropout mark on a lane: where the engine kept a dropout in a take or a layer, the lane shows it. Engine
 * mode on the web engine fake (`src/platform/host.web.ts`), booted as `engine-seam.mjs` boots it; the probe
 * scripts a committed 4-bar loop through `__lf.native` and its peaks, one run of them carrying `marked` (a
 * bin in bar 3, as the feed sends it: `src-tauri/src/engine_io/feed.rs`), and reads the lane's canvas in
 * the next drawn frame.
 *
 * Proves: the marked bin draws a rec-red notch at the lane's top edge inside bar 3's x range and nowhere
 * else, and bar 3 alone takes the faint rec-red wash; the same bin sent again with no `marked` (an UNDO,
 * a STOP's restore) takes both away; a reversed lane draws the bin the feed sends at its own frame (bar 2
 * of the mirrored loop); a reset frame, which re-sends every bin with its marks, keeps the mark.
 *
 * Cannot see the native engine or the feed's mapping (lf-engine `tests/dropout_marks.rs` and the feed's
 * own tests hold those), WebView2 or the eye: whether the mark reads from where the player stands is the
 * owner's (`STATUS.md` § Not heard yet).
 * Run: pnpm probe lane-marks
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

const RATE = 48000;
const BAR = 2 * RATE; // one 4/4 bar at 120 BPM
const MASTER = 4 * BAR;
const PEAK_FRAMES = 1024; // the engine's bin (`lf-engine/src/overview.rs`)
const COUNT = Math.ceil(MASTER / PEAK_FRAMES);
/** The bin of the frame 2.5 bars in: bar 3. */
const MARKED = Math.floor((2.5 * BAR) / PEAK_FRAMES);

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
/** Every bin of lane 0, quiet, `marked` as given. */
const peaks = (marked) => [{ lane: 0, start: 0, count: COUNT, min: Array(COUNT).fill(-0.05), max: Array(COUNT).fill(0.05), ...(marked.length ? { marked } : {}) }];

await probe(async ({ open }) => {
  const { page } = await open({
    viewport: { width: 1600, height: 900 },
    init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)),
  });
  let seq = 0;
  const emit = (frame) => page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], ...frame });
  const playing = (reversed) => laneEvent(0, lane('Playing', { length: MASTER, canReverse: true, canUndo: true, reversed }));
  const boot = (reversed, marked) => ({
    reset: true,
    settings: [],
    events: [transport(MASTER, true), playing(reversed), ...[1, 2, 3, 4].map((i) => laneEvent(i, lane('Empty'))), { Selected: { frame: 0, lane: 0 } }],
    anchor: { frame: 0, atMs: Date.now(), rate: 0, grid: 0 },
    meter: { peak: [0, 0], clip: [false, false] },
    peaks: peaks(marked),
  });

  await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });

  /**
   * Lane 0's canvas two drawn frames on: the columns of its top row that carry the mark colour, and per
   * bar one pixel a fifth of the way down, 0.7 of the way into the bar (above the quiet wave, past the
   * bar's engraved number), whose red the wash raises.
   */
  const read = () =>
    page.evaluate(async () => {
      await new Promise((r) => requestAnimationFrame(r));
      await new Promise((r) => requestAnimationFrame(r));
      const c = document.querySelectorAll('.lp-lane')[0].querySelector('canvas');
      const ctx = c.getContext('2d');
      // The notch: nearly opaque rec-red in the top row (the wash under it is faint).
      const top = ctx.getImageData(0, 0, c.width, 1).data;
      const red = [];
      for (let x = 0; x < c.width; x++) {
        const [r, g, , a] = top.slice(4 * x, 4 * x + 4);
        if (a > 160 && r > 150 && r - g > 80) red.push(x);
      }
      const y = Math.round(c.height * 0.2);
      const row = ctx.getImageData(0, y, c.width, 1).data;
      const wash = [0, 1, 2, 3].map((b) => {
        const x = Math.round(((b + 0.7) / 4) * c.width);
        const [r, g, , a] = row.slice(4 * x, 4 * x + 4);
        return { r, g, a };
      });
      return { red, wash, width: c.width };
    });

  /** The mark sits in bar `bar` (0-based) alone: its notch in that bar's x range, its wash on it only. */
  function inBar(name, seen, bar) {
    const [lo, hi] = [(bar / 4) * seen.width, ((bar + 1) / 4) * seen.width];
    console.log(`${name}: notch columns ${seen.red.length ? `${seen.red[0]}..${seen.red.at(-1)}` : 'none'} (bar ${bar + 1}: ${Math.round(lo)}..${Math.round(hi)}), wash ${JSON.stringify(seen.wash)}`);
    assert.ok(seen.red.length > 0, `${name}: the notch is drawn`);
    assert.ok(seen.red.every((x) => x >= lo - 1 && x < hi + 1), `${name}: the notch lies inside bar ${bar + 1}`);
    // Nothing else is drawn that high in a quiet lane: the wash is the only paint there, faint rec-red.
    const washed = (w) => w.a > 0 && w.r > 150 && w.r - w.g > 80;
    for (let b = 0; b < 4; b++) {
      if (b === bar) assert.ok(washed(seen.wash[b]) && seen.wash[b].a < 80, `${name}: bar ${b + 1} takes the faint red wash`);
      else assert.ok(!washed(seen.wash[b]), `${name}: bar ${b + 1} takes no wash`);
    }
  }

  // ── A marked bin in bar 3 of a forward loop ────────────────────────────────────────────────────────
  await emit(boot(false, [MARKED]));
  inBar('forward', await read(), 2);

  // ── The same run with no marks (UNDO): both go ────────────────────────────────────────────────────
  await emit({ peaks: peaks([]) });
  const cleared = await read();
  console.log(`cleared: ${cleared.red.length} notch columns, wash ${JSON.stringify(cleared.wash)}`);
  assert.equal(cleared.red.length, 0, 'a run sent with no marks takes the notch away');
  assert.ok(cleared.wash.every((w) => !(w.a > 0 && w.r - w.g > 80)), 'and every wash');

  // ── Reversed: the feed sends the bin at the frame it plays, here bar 2 of 4 ────────────────────────
  // A frame 2.5 bars into the buffer plays 1.5 bars into the reversed loop (`Overview::heard_marked`).
  const heard = Math.floor((MASTER - 1 - 2.5 * BAR) / PEAK_FRAMES);
  await emit({ events: [playing(true)], peaks: peaks([heard]) });
  inBar('reversed', await read(), 1);

  // ── A reset frame re-sends every bin with its marks: the mark stays ────────────────────────────────
  await emit(boot(true, [heard]));
  inBar('after a reset', await read(), 1);
});
