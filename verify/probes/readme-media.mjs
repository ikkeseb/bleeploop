/**
 * README media, not a proof: the hero clip `docs/media/bleeploop-demo.webp` and the still
 * `docs/media/bleeploop.png`, rendered from the real UI at 1280x820 with the keyboard hidden. Engine mode
 * on the web engine fake (`src/platform/host.web.ts`), set up as `contact-sheet.mjs` sets up its
 * guitar-first scenes: the plugin host served as available, two stubbed amp-sims scanned and the first
 * picked in slot A (GO LIVE), and every lane, the clock, the beats, the waveforms and the input meter
 * scripted on the feed through `__lf.native`.
 *
 * The clip is a jam played with a pointer, the way a player clicks through one: the pointer starts take 1
 * from track 1's record core (a count-in follows), three two-bar takes land on tracks 1-3, and between
 * them, while the loops play, the player switches ECHO on in IN FX, engages DELAY in track 1's FX drawer,
 * swaps slot A's amp-sim (the slot reads "Updating…", then comes back live), then opens the stage view,
 * exits it and presses ✕ ALL twice. That ends it on the first frame's state, so it loops; the rig (IN FX,
 * the lane's FX, slot A) resets behind the stage view, which covers the UI. The pointer is an arrow
 * drawn in the page, moved on eased paths between targets found in the DOM, with a press cue on each
 * click; each click is a real mouse
 * click at the same point, so the UI reacts for real, and the feed answers as the engine would (a lane
 * arms on RecDub, the take starts on the loop's downbeat). The camera is a CSS transform on the root
 * (eased pans and zooms onto what changes, back to the full frame for a take's landing and the stage
 * view), captured at device scale 2 and downscaled once by ffmpeg: sub-pixel moves, crisp text.
 *
 * Playwright's clock holds the page's time: each frame advances it by 1/FPS, seeks every CSS animation
 * to it and takes a screenshot, so the clip keeps the tempo however slow the capture runs. The still is
 * the same jam at device scale 2 without the pointer or the camera, taken mid third take. The waveforms
 * and the meter come from seeded envelopes (strums, a muted riff, a lead line), not audio: the clip is
 * silent, as the browser build is. ffmpeg (with libwebp) encodes the frames at 15 fps, quality 35: the
 * camera moves change every pixel, and the clip stays under 3.5 MB. The frames stay in logs/readme-media/.
 *
 * Asserts only that the page logged no console error. After a UI change, rerun it and look at the result.
 * `--until=<seconds>` renders the clip's frames up to there and skips the still and the encode.
 * @no-ci renders README media and needs ffmpeg on PATH
 * Run: pnpm probe readme-media
 */
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdir, rm } from 'node:fs/promises';
import { arg, probe } from '../harness/probe.ts';

const VIEWPORT = { width: 1280, height: 820 };
const FPS = 20; // captured
const CLIP_FPS = 15; // encoded: the camera moves change every pixel, and the clip must stay under 3.5 MB
const RATE = 48000;
const BPM = 140;
const PEAK_FRAMES = 1024; // the engine's waveform bin (`lf-engine/src/overview.rs`)
const BAR = Math.round((RATE * 240) / BPM); // `framesPerBar`
const BEAT = BAR / 4;
const LOOP = 2 * BAR;
const BINS = Math.ceil(LOOP / PEAK_FRAMES);
const STEP = RATE / FPS; // device frames per clip frame
const sec = (s) => Math.round(s * RATE);
/** Seconds snapped to the clip's frame grid, in device frames. */
const at = (s) => Math.round(s * FPS) * STEP;
const s = (f) => f / RATE; // device frames → seconds

const framesDir = 'logs/readme-media';
const CLIP = 'docs/media/bleeploop-demo.webp';
const STILL = 'docs/media/bleeploop.png';
const AMP_SIMS = [
  { id: 'neural.petrucci-x', name: 'Archetype Petrucci X', format: 'vst3', isEffect: true,
    path: 'C:\\Program Files\\Common Files\\VST3\\Archetype Petrucci X.vst3' },
  { id: 'neural.nolly-x', name: 'Archetype Nolly X', format: 'vst3', isEffect: true,
    path: 'C:\\Program Files\\Common Files\\VST3\\Archetype Nolly X.vst3' },
];
const SWAP_MS = 700; // the stubbed plugin load's duration: the slot reads "Updating…" meanwhile
const STATUS = { backend: 'Asio', sampleRate: RATE, block: 128, inputName: 'ASIO interface', outputName: 'ASIO interface',
  alignFrames: 700, inputFrames: 350, inputOpen: true, inputChannels: [0, 1] };

// ── The jam's timeline, in device frames from the first frame ─────────────────────────────────────────
// The pointer's clicks set the pace; the feed answers each on the next frame.
const REC1 = at(0.95); // track 1's core: arm, count in
const COUNT_IN = REC1 + STEP;
const TAKE0 = COUNT_IN + BAR;
const bar = (k) => TAKE0 + k * LOOP; // the k-th loop boundary
const LAND1 = bar(1);
const nextBar = (f) => { let k = 0; while (bar(k) < f + sec(0.3)) k++; return bar(k); };
const REC2 = at(s(LAND1) + 8.7); // track 2's core, after IN FX and track 1's FX
const LAND2 = nextBar(REC2) + LOOP;
const REC3 = at(s(LAND2) + 3.6); // track 3's core, after the amp-sim swap
const TAKE = [TAKE0, nextBar(REC2), nextBar(REC3)]; // each take's downbeat
const ARM = [COUNT_IN, REC2 + STEP, REC3 + STEP];
const STAGE_OPEN = at(s(TAKE[2] + LOOP) + 1.0); // the stage view's pill
const RESET = STAGE_OPEN + sec(1.2); // the rig resets behind the stage view
const STAGE_CLOSE = at(s(STAGE_OPEN) + 3.6); // its EXIT
const CLEAR_ALL = [at(s(STAGE_CLOSE) + 0.9), at(s(STAGE_CLOSE) + 1.25)]; // ✕ ALL, twice: arm, confirm
const CLEAR = CLEAR_ALL[1] + STEP;
const END = at(s(CLEAR) + 1.1);
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

// ── The pointer and the camera ────────────────────────────────────────────────────────────────────────
const REST = { x: 1012, y: 198 }; // where the pointer waits: the gap between the slots and the lanes
const ease = (u) => (u < 0.5 ? 4 * u * u * u : 1 - Math.pow(-2 * u + 2, 3) / 2);
const clamp = (v, lo, hi) => Math.max(lo, Math.min(hi, v));
const center = (r) => ({ x: r.x + r.width / 2, y: r.y + r.height / 2 });

/**
 * The script: what the pointer and the camera do, in clip seconds. `move` glides the pointer to a
 * target (a role/name in the DOM, or a point) over `dur`; `click` presses where the pointer is; `pick`
 * chooses an option in the select under the pointer (a native dropdown never shows in a screenshot, so
 * the press cue stands for it); `cam` eases the camera onto a target at `zoom` (`full` for the whole
 * frame). A target names an element that exists when its move starts (the popover after its click).
 */
const button = (name) => ({ role: 'button', name });
const SCRIPT = [
  // Take 1: arm track 1, count in, record.
  { t: 0.1, cam: button('Track 1 record'), zoom: 1.6, dur: 0.9, pad: { x: 140, y: 40 } },
  { t: 0.15, move: button('Track 1 record'), dur: 0.7 },
  { t: s(REC1), click: true },
  { t: s(TAKE0) - 0.2, cam: 'full', dur: 1.0 },
  { t: s(TAKE0) - 0.1, move: REST, dur: 0.8 },
  // Track 1 plays: ECHO on in IN FX.
  { t: s(LAND1) + 0.15, cam: button('Input effects'), zoom: 1.5, dur: 0.9, pad: { x: 0, y: 120 } },
  { t: s(LAND1) + 0.2, move: button('Input effects'), dur: 0.7 },
  { t: s(LAND1) + 1.0, click: true },
  { t: s(LAND1) + 1.1, cam: { role: 'dialog', name: 'Input effects' }, zoom: 1.6, dur: 0.7, pad: { x: 0, y: 44 } },
  { t: s(LAND1) + 1.3, move: button('Input echo'), dur: 0.6 },
  { t: s(LAND1) + 2.0, click: true },
  { t: s(LAND1) + 3.2, move: { role: 'dialog', name: 'Input effects', edge: 'below' }, dur: 0.5 },
  { t: s(LAND1) + 3.8, click: true }, // the backdrop closes it
  // DELAY on track 1's FX.
  { t: s(LAND1) + 3.9, cam: button('Track 1 FX'), zoom: 1.5, dur: 0.9, pad: { x: 200, y: 60 } },
  { t: s(LAND1) + 4.0, move: button('Track 1 FX'), dur: 0.8 },
  { t: s(LAND1) + 4.9, click: true },
  { t: s(LAND1) + 5.0, cam: { role: 'group', name: 'FX, Track 1' }, zoom: 1.35, dur: 0.7, pad: { x: 0, y: 40 } },
  { t: s(LAND1) + 5.1, move: button('Delay, track 1'), dur: 0.7 },
  { t: s(LAND1) + 5.9, click: true },
  { t: s(LAND1) + 7.1, move: button('Close FX panel'), dur: 0.6 },
  { t: s(LAND1) + 7.8, click: true },
  // Take 2: arm track 2.
  { t: s(LAND1) + 7.9, cam: 'full', dur: 1.0 },
  { t: s(REC2) - 0.85, move: button('Track 2 record'), dur: 0.7 },
  { t: s(REC2), click: true },
  { t: s(REC2) + 0.1, move: REST, dur: 0.8 },
  // Tracks 1-2 play: slot A swaps to the other amp-sim.
  { t: s(LAND2) + 0.15, cam: { role: 'group', name: /^Slot 1\b/ }, zoom: 1.5, dur: 0.9, pad: { x: 20, y: 60 } },
  { t: s(LAND2) + 0.2, move: { role: 'combobox', name: 'Source for slot 1' }, dur: 0.8 },
  { t: s(LAND2) + 1.1, click: true, pick: `${AMP_SIMS[1].name} (vst3)` },
  { t: s(LAND2) + 2.6, cam: 'full', dur: 1.0 },
  // Take 3: arm track 3.
  { t: s(REC3) - 0.9, move: button('Track 3 record'), dur: 0.8 },
  { t: s(REC3), click: true },
  { t: s(REC3) + 0.1, move: REST, dur: 0.8 },
  // The stage view, then CLEAR ALL: back to the first frame.
  { t: s(STAGE_OPEN) - 0.8, move: button('Stage view'), dur: 0.7 },
  { t: s(STAGE_OPEN), click: true },
  { t: s(STAGE_CLOSE) - 0.8, move: button('Exit stage view'), dur: 0.7 },
  { t: s(STAGE_CLOSE), click: true },
  { t: s(CLEAR_ALL[0]) - 0.8, move: button('Clear all tracks'), dur: 0.7 },
  { t: s(CLEAR_ALL[0]), click: true },
  { t: s(CLEAR_ALL[0]) + 0.1, move: button('Clear all tracks, press again to confirm'), dur: 0.2 }, // it widens to SURE?
  { t: s(CLEAR_ALL[1]), click: true },
  { t: s(CLEAR_ALL[1]) + 0.1, move: REST, dur: 0.8 },
];

/** The pointer's arrow and press ring, drawn in the page under the camera's transform (it zooms with
 * the screen, as a recorded cursor would). */
const POINTER_CSS = `
  #lf-media-pointer, #lf-media-ring { position: fixed; left: 0; top: 0; z-index: 10000; pointer-events: none; }
  #lf-media-pointer { width: 22px; height: 30px; transform-origin: 2px 2px; filter: drop-shadow(0 1px 1.5px rgba(0,0,0,.55)) drop-shadow(0 4px 8px rgba(0,0,0,.35)); }
  #lf-media-ring { width: 12px; height: 12px; margin: -6px 0 0 -6px; border-radius: 50%; border: 2px solid rgba(255,255,255,.9); box-sizing: border-box; }
`;
const POINTER_SVG = `<svg viewBox="0 0 22 30" xmlns="http://www.w3.org/2000/svg">
  <path d="M2 2 L2 23.5 L7.3 18.8 L11 27 L14.6 25.4 L11 17.4 L18 17.4 Z" fill="#fff" stroke="#1a1a1e" stroke-width="1.6" stroke-linejoin="round"/>
</svg>`;

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
    await page.evaluate(async ({ ampSims, swapMs }) => {
      const { platform } = await import('/src/platform/index.ts');
      const instrument = await import('/src/ui/state/instrument.ts');
      const host = platform.pluginHost;
      const deadline = Date.now() + 10000;
      while (!instrument.nativeHostReady() || instrument.scanning()) {
        if (Date.now() > deadline) throw new Error('native host boot did not finish');
        await new Promise((r) => setTimeout(r, 50));
      }
      host.scanPlugins = async () => ampSims;
      // A load takes a moment, as a native one does; the first pick, before the clock pauses, at once.
      host.loadPlugin = async (slot, path) => {
        const descriptor = ampSims.find((p) => p.path === path);
        if (window.__mediaSlowLoad) await new Promise((r) => setTimeout(r, swapMs));
        return { slot, descriptor };
      };
      host.unloadPlugin = async () => {};
      host.openEditor = async () => {};
      host.closeEditor = async () => {};
      host.listParams = async () => [];
      await instrument.scanForPlugins();
    }, { ampSims: AMP_SIMS, swapMs: SWAP_MS });
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
    await source.locator('option', { hasText: AMP_SIMS[0].name }).waitFor({ state: 'attached' });
    await source.selectOption({ label: `${AMP_SIMS[0].name} (vst3)` });
    await page.getByRole('button', { name: 'Live input for slot 1', exact: true, pressed: true }).waitFor();
    await page.evaluate(() => document.activeElement?.blur());
    await page.evaluate(() => { window.__mediaSlowLoad = true; });
    await page.clock.pauseAt(t0);
    return { context, page, t0 };
  }

  /**
   * The pointer and the camera over `page`, driven by SCRIPT. `tick(seconds)` runs the actions due,
   * moves the pointer and the camera to their state at that time and draws them; `state()` is what the
   * frame shows (for the seam check). The camera is `translate(tx, ty) scale(z)` on the root: a page
   * point p shows at p * z + t.
   */
  function director(page) {
    const pointer = { x: REST.x, y: REST.y, from: null, to: null, t0: 0, dur: 0, pressAt: -1 };
    const cam = { x: 640, y: 410, z: 1, from: null, to: null, t0: 0, dur: 0 };
    const pending = SCRIPT.map((a) => ({ ...a, done: false }));
    const screen = (p) => ({ x: p.x * cam.z + 640 - cam.x * cam.z, y: p.y * cam.z + 410 - cam.y * cam.z });
    const unscreen = (p) => ({ x: (p.x - 640) / cam.z + cam.x, y: (p.y - 410) / cam.z + cam.y });
    /** A target's box in page coordinates (its on-screen box, taken back through the camera). */
    async function rectOf(target) {
      const box = await page.getByRole(target.role, { name: target.name, exact: typeof target.name === 'string' }).first().boundingBox();
      if (!box) throw new Error(`no ${target.role} "${target.name}" on screen for the pointer`);
      const a = unscreen(box);
      return { x: a.x, y: a.y, width: box.width / cam.z, height: box.height / cam.z };
    }
    async function pointOf(target) {
      if (!target.role) return target;
      const r = await rectOf(target);
      if (target.edge === 'below') return { x: r.x + r.width / 2, y: r.y + r.height + 34 };
      return center(r);
    }
    /** The camera window (its centre, clamped inside the frame) that frames `target` at `zoom`. */
    async function windowOf(target, zoom, pad) {
      if (target === 'full') return { x: 640, y: 410, z: 1 };
      const r = await rectOf(target);
      const c = center(r);
      // A padded target that overflows the window at `zoom` lowers the zoom until it fits.
      const need = { w: r.width + 2 * (pad?.x ?? 0), h: r.height + 2 * (pad?.y ?? 0) };
      const z = Math.min(zoom, 1280 / need.w, 820 / need.h);
      return { x: clamp(c.x, 640 / z, 1280 - 640 / z), y: clamp(c.y, 410 / z, 820 - 410 / z), z };
    }
    async function tick(now) {
      for (const a of pending) {
        if (a.done || a.t > now + 1e-9) continue;
        a.done = true;
        if (a.move) {
          pointer.from = { x: pointer.x, y: pointer.y };
          pointer.to = await pointOf(a.move);
          pointer.t0 = a.t;
          pointer.dur = a.dur;
        }
        if (a.cam) {
          cam.from = { x: cam.x, y: cam.y, z: cam.z };
          cam.to = await windowOf(a.cam, a.zoom, a.pad);
          cam.t0 = a.t;
          cam.dur = a.dur;
        }
        if (a.click) {
          pointer.pressAt = a.t;
          const p = screen(pointer);
          if (a.pick) {
            // The select under the pointer: chosen without opening its native dropdown.
            await page.mouse.move(p.x, p.y);
            const el = page.getByRole('combobox', { name: 'Source for slot 1', exact: true });
            await el.selectOption({ label: a.pick });
            await page.evaluate(() => document.activeElement?.blur());
          } else {
            await page.mouse.click(p.x, p.y);
          }
        }
      }
      // The pointer's glide: eased, with a slight arc.
      if (pointer.to) {
        const u = ease(clamp((now - pointer.t0) / pointer.dur, 0, 1));
        const dx = pointer.to.x - pointer.from.x;
        const dy = pointer.to.y - pointer.from.y;
        const dist = Math.hypot(dx, dy) || 1;
        const arc = Math.sin(Math.PI * u) * Math.min(22, dist * 0.08);
        pointer.x = pointer.from.x + dx * u - (dy / dist) * arc;
        pointer.y = pointer.from.y + dy * u + (dx / dist) * arc;
        if (u >= 1) pointer.to = null;
      }
      if (cam.to) {
        const u = ease(clamp((now - cam.t0) / cam.dur, 0, 1));
        cam.x = cam.from.x + (cam.to.x - cam.from.x) * u;
        cam.y = cam.from.y + (cam.to.y - cam.from.y) * u;
        cam.z = cam.from.z + (cam.to.z - cam.from.z) * u;
        if (u >= 1) cam.to = null;
      }
      const press = pointer.pressAt < 0 ? 1 : (now - pointer.pressAt) / 0.4; // the press cue's progress
      const sp = screen(pointer);
      await page.mouse.move(sp.x, sp.y);
      await page.evaluate(({ p, cam, press, css, svg }) => {
        let arrow = document.getElementById('lf-media-pointer');
        if (!arrow) {
          const style = document.createElement('style');
          style.textContent = css;
          document.head.append(style);
          arrow = document.createElement('div');
          arrow.id = 'lf-media-pointer';
          arrow.innerHTML = svg;
          const ring = document.createElement('div');
          ring.id = 'lf-media-ring';
          document.body.append(arrow, ring);
        }
        const dip = press < 0.5 ? Math.sin(Math.PI * press) * 0.14 : 0; // a quick scale-down on the press
        arrow.style.transform = `translate(${p.x - 2}px, ${p.y - 2}px) scale(${1 - dip})`;
        const ring = document.getElementById('lf-media-ring');
        if (press >= 1) ring.style.display = 'none';
        else {
          ring.style.display = 'block';
          ring.style.opacity = String(0.8 * (1 - press));
          ring.style.transform = `translate(${p.x}px, ${p.y}px) scale(${1 + 3.2 * press})`;
        }
        const root = document.documentElement;
        if (cam.z === 1 && cam.x === 640 && cam.y === 410) root.style.transform = '';
        else {
          root.style.overflow = 'hidden';
          root.style.transformOrigin = '0 0';
          root.style.transform = `translate(${640 - cam.x * cam.z}px, ${410 - cam.y * cam.z}px) scale(${cam.z})`;
        }
      }, { p: { x: pointer.x, y: pointer.y }, cam: { x: cam.x, y: cam.y, z: cam.z }, press, css: POINTER_CSS, svg: POINTER_SVG });
    }
    const state = () => ({ pointer: { x: pointer.x, y: pointer.y }, cam: { x: cam.x, y: cam.y, z: cam.z } });
    return { tick, state };
  }

  /** Step the jam from frame 0 to `until`, calling `shot(n, frame)` after each step; with a `director`,
   * its pointer and camera play along. */
  async function play(page, t0, until, shot, dir) {
    let seq = 1;
    let prev = -1;
    for (let n = 0; n * STEP <= until; n++) {
      const frame = Math.round(n * STEP);
      const grid = frame >= TAKE[0] ? TAKE[0] : 0;
      if (dir) await dir.tick(s(frame));
      await page.evaluate((f) => window.__lf.native.emit(f), {
        seq: ++seq, reset: false, device: [],
        events: eventsBetween(prev, frame),
        anchor: { frame, atMs: t0 + (frame * 1000) / RATE, rate: RATE, grid },
        meter: { peak: meterAt(frame), clip: false },
        peaks: peaksBetween(prev, frame),
      });
      if (dir && prev < RESET && frame >= RESET) {
        // Behind the stage view: the rig back to the first frame's (a rig setting outlives CLEAR ALL).
        await page.evaluate(async (name) => {
          const { engineInputSends } = await import('/src/ui/state/engine-store.ts');
          engineInputSends.setOn('echo', false);
          window.__lf.looper.setFxBypass(0, 3, true); // DELAY, `FX_META`'s order
          // The picker sits under the modal stage view, out of Playwright's reach: picked as a change event.
          const picker = document.querySelector('select[aria-label="Source for slot 1"]');
          picker.value = [...picker.options].find((o) => o.textContent.includes(name)).value;
          picker.dispatchEvent(new Event('change', { bubbles: true }));
        }, AMP_SIMS[0].name);
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
  const until = arg('until');
  const clip = await openJam(2);
  const dir = director(clip.page);
  let count = 0;
  await play(clip.page, clip.t0, until ? sec(Number(until)) : END, async (n) => {
    await clip.page.screenshot({ path: `${framesDir}/f${String(n).padStart(4, '0')}.png` });
    count = n + 1;
  }, dir);
  const last = dir.state();
  await clip.context.close();
  console.log(`${count} frames at ${FPS} fps (${(count / FPS).toFixed(1)} s) in ${framesDir}`);
  if (until) return;
  assert.deepEqual(last, { pointer: REST, cam: { x: 640, y: 410, z: 1 } }, 'the last frame is framed as the first');

  const still = await openJam(2);
  await play(still.page, still.t0, STILL_AT, async (_n, frame) => {
    if (frame + STEP > STILL_AT) await still.page.screenshot({ path: STILL });
  });
  await still.context.close();

  execFileSync('ffmpeg', ['-y', '-loglevel', 'error', '-framerate', String(FPS), '-i', `${framesDir}/f%04d.png`,
    '-vf', `fps=${CLIP_FPS},scale=1280:820:flags=area`, '-c:v', 'libwebp_anim', '-loop', '0', '-quality', '35', '-compression_level', '6', CLIP],
    { stdio: 'inherit' });
  console.log(`wrote ${CLIP} and ${STILL}`);
  assert.deepEqual(errors, [], 'no console error or uncaught page error');
});
