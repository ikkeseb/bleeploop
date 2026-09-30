/**
 * README media, not a proof: the hero clip `docs/media/bleeploop-demo.webp` and the still
 * `docs/media/bleeploop.png`, rendered from the real UI at 1280x820 with the keyboard hidden. Engine mode
 * on the web engine fake (`src/platform/host.web.ts`), set up as `contact-sheet.mjs` sets up its
 * guitar-first scenes: the plugin host served as available, one stubbed amp-sim picked in slot A (GO
 * LIVE), and every lane, the clock, the beats, the waveforms and the input meter scripted on the feed
 * through `__lf.native`.
 *
 * The clip builds a jam: a count-in, three two-bar takes on tracks 1-3, each track playing on while the
 * next waits for the loop, the stage view, then CLEAR ALL back to the first frame, so it loops. Playwright's
 * clock holds the page's time: each frame advances it by 1/FPS, seeks every CSS animation to it and takes
 * a screenshot, so the clip keeps the tempo however slow the capture runs. The still is the same timeline
 * at device scale 2, taken mid third take. The waveforms and the meter come from seeded envelopes
 * (strums, a muted riff, a lead line), not audio: the clip is silent, as the browser build is.
 * ffmpeg (with libwebp) encodes the frames; the frames stay in logs/readme-media/.
 *
 * Asserts only that the page logged no console error. After a UI change, rerun it and look at the result.
 * @no-ci renders README media and needs ffmpeg on PATH
 * Run: pnpm probe readme-media
 */
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdir, rm } from 'node:fs/promises';
import { probe } from '../harness/probe.ts';

const VIEWPORT = { width: 1280, height: 820 };
const FPS = 20;
const RATE = 48000;
const BPM = 140;
const PEAK_FRAMES = 1024; // the engine's waveform bin (`lf-engine/src/overview.rs`)
const BAR = Math.round((RATE * 240) / BPM); // `framesPerBar`
const BEAT = BAR / 4;
const LOOP = 2 * BAR;
const BINS = Math.ceil(LOOP / PEAK_FRAMES);
const sec = (s) => Math.round(s * RATE);

const framesDir = 'logs/readme-media';
const CLIP = 'docs/media/bleeploop-demo.webp';
const STILL = 'docs/media/bleeploop.png';
const AMP_SIM = { id: 'neural.petrucci-x', name: 'Archetype Petrucci X', format: 'vst3', isEffect: true,
  path: 'C:\\Program Files\\Common Files\\VST3\\Archetype Petrucci X.vst3' };
const STATUS = { backend: 'Asio', sampleRate: RATE, block: 128, inputName: 'ASIO interface', outputName: 'ASIO interface',
  alignFrames: 700, inputFrames: 350, inputOpen: true, inputChannels: [0, 1] };

// ── The jam's timeline, in device frames from the first frame ─────────────────────────────────────────
const COUNT_IN = sec(0.6);
const TAKE = [COUNT_IN + BAR, COUNT_IN + BAR + 2 * LOOP, COUNT_IN + BAR + 4 * LOOP]; // each take's downbeat
const ARM = [COUNT_IN, TAKE[0] + LOOP + sec(0.35), TAKE[1] + LOOP + sec(0.35)];
const STAGE_OPEN = TAKE[2] + LOOP + sec(1);
const STAGE_CLOSE = STAGE_OPEN + sec(3.4);
const CLEAR = STAGE_CLOSE + sec(0.6);
const END = CLEAR + COUNT_IN;
const STILL_AT = TAKE[2] + Math.round(LOOP * 0.62);

// A fixed-seed Park–Miller generator per lane: every run draws the same waveforms.
const rng = (seed) => () => (seed = (seed * 16807) % 2147483647) / 2147483647;

/** One bin's peak from a sum of note envelopes: soft-clipped, with a little texture. */
function bins(notes, seed) {
  const r = rng(seed);
  const max = [];
  const min = [];
  for (let b = 0; b < BINS; b++) {
    const t = (b * PEAK_FRAMES) / RATE;
    let sum = 0.025;
    for (const n of notes) sum += n(t);
    const v = Math.tanh(sum) * (0.9 + 0.1 * r());
    max.push(v);
    min.push(-v * (0.86 + 0.12 * r()));
  }
  return { min, max };
}
const beatS = 60 / BPM;
/** A plucked hit at beat `at`: a fast attack and an exponential decay. */
const pluck = (at, amp, tau) => (t) => {
  const d = t - at * beatS;
  return d < 0 ? 0 : amp * Math.min(1, d / 0.008) * Math.exp(-d / tau);
};
/** A held lead note from beat `at` for `len` beats, with vibrato once it has sung a while. */
const held = (at, len, amp) => (t) => {
  const d = t - at * beatS;
  const end = len * beatS;
  if (d < 0 || d > end + 0.08) return 0;
  const release = d > end ? 1 - (d - end) / 0.08 : 1;
  const vib = d > 0.25 ? 1 + 0.18 * Math.sin(2 * Math.PI * 5.5 * d) : 1;
  return amp * Math.min(1, d / 0.015) * Math.exp(-d / 1) * release * vib;
};
const strums = [[0, 1], [0.5, 0.5], [1.5, 0.75], [2, 0.9], [3, 0.6], [3.5, 0.7], [4, 1], [5, 0.7], [5.5, 0.8], [6, 0.9], [7, 0.55], [7.5, 0.75]];
const riff = Array.from({ length: 32 }, (_, k) => k)
  .filter((k) => ![3, 7, 14, 15, 19, 23, 30, 31].includes(k))
  .map((k) => [k / 4, k % 4 === 0 ? 0.6 : 0.36]);
const lead = [[0, 1.5, 0.45], [1.5, 0.5, 0.35], [2, 2, 0.52], [4.5, 1, 0.4], [5.5, 0.5, 0.33], [6, 1.75, 0.5]];
const WAVES = [
  bins(strums.map(([at, a]) => pluck(at, 0.7 * a, 0.15)), 20260930),
  bins(riff.map(([at, a]) => pluck(at, a, 0.07)), 1729),
  bins(lead.map(([at, len, a]) => held(at, len, a)), 4242),
];

/** The input meter at frame `f`: the take's envelope while one records, else a guitar resting. */
function meterAt(f) {
  const k = TAKE.findIndex((t) => f >= t && f < t + LOOP);
  const noise = 0.02 + 0.02 * Math.abs(Math.sin(f / 5000));
  if (k < 0) return noise;
  return Math.max(noise, WAVES[k].max[Math.floor((f - TAKE[k]) / PEAK_FRAMES)]);
}

const lane = (state, extra = {}) => ({
  state, length: 0, armed: false, autoArmed: false, canUndo: false, canReverse: false, reversed: false,
  stopAt: null, fading: false, retakePass: 0, ...extra,
});
const laneEvent = (i, info, frame) => ({ Lane: { frame, lane: i, info } });
const transport = (master, locked, frame) => ({ Transport: { frame, master, bpm: BPM, locked } });

/** The feed's events in (from, to], in frame order. */
function eventsBetween(from, to) {
  const out = [];
  const at = (frame, ...events) => { if (frame > from && frame <= to) out.push(...events); };
  at(COUNT_IN, laneEvent(0, lane('Recording', { armed: true }), COUNT_IN), transport(0, true, COUNT_IN));
  // Beats from the count-in on: four counted, then the bar's beats while the loops run.
  for (let f = COUNT_IN, n = 0; f < CLEAR; f += BEAT, n++) {
    const frame = Math.round(f);
    at(frame, { Beat: { frame, beatInBar: n % 4, countLeft: Math.max(0, 4 - n), clicked: true } });
  }
  for (let k = 0; k < 3; k++) {
    if (k > 0) at(ARM[k], { Selected: { frame: ARM[k], lane: k } }, laneEvent(k, lane('Recording', { armed: true }), ARM[k]));
    at(TAKE[k], laneEvent(k, lane('Recording'), TAKE[k]));
    const done = TAKE[k] + LOOP;
    at(done, laneEvent(k, lane('Playing', { length: LOOP, canUndo: false, canReverse: true }), done));
    if (k === 0) at(done, transport(LOOP, true, done));
  }
  at(CLEAR, ...[0, 1, 2].flatMap((i) => [{ Cleared: { frame: CLEAR, lane: i } }, laneEvent(i, lane('Empty'), CLEAR)]),
    transport(0, false, CLEAR), { Selected: { frame: CLEAR, lane: 0 } });
  return out;
}

/** The waveform bins that arrived in (from, to]: a recording take's new bins, the last one again (it was
 * partial); a cleared lane's empty update. */
function peaksBetween(from, to) {
  const out = [];
  TAKE.forEach((t, k) => {
    const have = Math.min(BINS, Math.max(0, Math.floor((from - t) / PEAK_FRAMES)));
    const next = Math.min(BINS, Math.max(0, Math.floor((to - t) / PEAK_FRAMES)));
    if (next > have) {
      const start = Math.max(0, have - 1);
      out.push({ lane: k, start, count: next, min: WAVES[k].min.slice(start, next), max: WAVES[k].max.slice(start, next) });
    }
  });
  if (from < CLEAR && to >= CLEAR) out.push(...[0, 1, 2].map((k) => ({ lane: k, start: 0, count: 0, min: [], max: [] })));
  return out;
}

await probe(async ({ browser, open }) => {
  const errors = [];

  /** The app on the fake at `scale`, the amp-sim live in slot A, the looper empty, the page's clock
   * paused at device frame 0. Returns the page and the Unix ms of frame 0. */
  async function openJam(scale) {
    const context = await browser.newContext({ viewport: VIEWPORT, deviceScaleFactor: scale });
    const { page } = await open({ context, viewport: VIEWPORT, allowPageErrors: true, init: async (p) => {
      p.on('console', (m) => { if (m.type() === 'error') errors.push(m.text()); });
      p.on('pageerror', (e) => errors.push(String(e)));
      await p.clock.install();
      await p.route('**/src/platform/host.web.ts', async (route) => {
        const response = await route.fetch();
        await route.fulfill({ response, body: (await response.text()).replace('available: false', 'available: true') });
      });
      await p.addInitScript(() => { window.__lfEngineFake = true; });
    } });
    await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });
    await page.evaluate(() => window.__lf.layoutStore.setKeyboardPlacement('hidden'));
    await page.evaluate(async (ampSim) => {
      const { platform } = await import('/src/platform/index.ts');
      const instrument = await import('/src/ui/state/instrument.ts');
      const host = platform.pluginHost;
      const deadline = Date.now() + 10000;
      while (!instrument.nativeHostReady() || instrument.scanning()) {
        if (Date.now() > deadline) throw new Error('native host boot did not finish');
        await new Promise((r) => setTimeout(r, 50));
      }
      host.scanPlugins = async () => [ampSim];
      host.loadPlugin = async (slot) => ({ slot, descriptor: ampSim });
      host.unloadPlugin = async () => {};
      host.openEditor = async () => {};
      host.closeEditor = async () => {};
      host.listParams = async () => [];
      await instrument.scanForPlugins();
    }, AMP_SIM);
    const t0 = (await page.evaluate(() => Date.now())) + 1500;
    await page.evaluate(({ status, rate, t0 }) => window.__lf.native.emit({
      seq: 1, reset: true, device: [], status,
      settings: [{ SetMetronome: true }, { SetFixedLength: true }, { SetFixedBars: 2 }],
      events: [...[0, 1, 2, 3, 4].map((i) => ({ Lane: { frame: 0, lane: i, info: {
        state: 'Empty', length: 0, armed: false, autoArmed: false, canUndo: false, canReverse: false, reversed: false,
        stopAt: null, fading: false, retakePass: 0 } } })),
      { Transport: { frame: 0, master: 0, bpm: 140, locked: false } }, { Selected: { frame: 0, lane: 0 } }],
      anchor: { frame: 0, atMs: t0, rate, grid: 0 },
      meter: { peak: 0.03, clip: false },
      peaks: [],
    }), { status: STATUS, rate: RATE, t0 });
    const source = page.getByRole('combobox', { name: 'Source for slot 1', exact: true });
    await source.locator('option', { hasText: AMP_SIM.name }).waitFor({ state: 'attached' });
    await source.selectOption({ label: `${AMP_SIM.name} (vst3)` });
    await page.getByRole('button', { name: 'Stop live input for slot 1', exact: true }).waitFor();
    await page.evaluate(() => document.activeElement?.blur());
    await page.clock.pauseAt(t0);
    return { context, page, t0 };
  }

  /** Step the jam from frame 0 to `until`, calling `shot(n, frame)` after each step. */
  async function play(page, t0, until, shot) {
    let seq = 1;
    let prev = -1;
    const step = RATE / FPS;
    for (let n = 0; n * step <= until; n++) {
      const frame = Math.round(n * step);
      const grid = frame >= TAKE[0] ? TAKE[0] : 0;
      await page.evaluate((f) => window.__lf.native.emit(f), {
        seq: ++seq, reset: false, device: [],
        events: eventsBetween(prev, frame),
        anchor: { frame, atMs: t0 + (frame * 1000) / RATE, rate: RATE, grid },
        meter: { peak: meterAt(frame), clip: false },
        peaks: peaksBetween(prev, frame),
      });
      if (prev < STAGE_OPEN && frame >= STAGE_OPEN) await page.keyboard.press('b');
      if (prev < STAGE_CLOSE && frame >= STAGE_CLOSE) {
        await page.keyboard.press('Escape');
        await page.evaluate(() => document.activeElement?.blur()); // no focus ring on the stage-view cap
      }
      prev = frame;
      await page.clock.runFor(1000 / FPS);
      // CSS animations run on the compositor's clock, not the page's: hold each at its age in page time.
      await page.evaluate(() => {
        const now = performance.now();
        const born = (window.__mediaBorn ??= new WeakMap());
        for (const a of document.getAnimations()) {
          if (!born.has(a)) born.set(a, now);
          a.pause();
          a.currentTime = now - born.get(a);
        }
      });
      await shot(n, frame);
    }
  }

  await rm(framesDir, { recursive: true, force: true });
  await mkdir(framesDir, { recursive: true });
  const clip = await openJam(1);
  let count = 0;
  await play(clip.page, clip.t0, END, async (n) => {
    await clip.page.screenshot({ path: `${framesDir}/f${String(n).padStart(4, '0')}.png` });
    count = n + 1;
  });
  await clip.context.close();
  console.log(`${count} frames at ${FPS} fps (${(count / FPS).toFixed(1)} s) in ${framesDir}`);

  const still = await openJam(2);
  await play(still.page, still.t0, STILL_AT, async (_n, frame) => {
    if (frame + RATE / FPS > STILL_AT) await still.page.screenshot({ path: STILL });
  });
  await still.context.close();

  execFileSync('ffmpeg', ['-y', '-loglevel', 'error', '-framerate', String(FPS), '-i', `${framesDir}/f%04d.png`,
    '-c:v', 'libwebp_anim', '-loop', '0', '-quality', '80', '-compression_level', '6', CLIP], { stdio: 'inherit' });
  console.log(`wrote ${CLIP} and ${STILL}`);
  assert.deepEqual(errors, [], 'no console error or uncaught page error');
});
