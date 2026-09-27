/**
 * A track's length after recording, in engine mode on the web engine fake (`src/platform/host.web.ts`,
 * the `engine-seam` pattern: an init script sets `window.__lfEngineFake`, the probe scripts the feed
 * through `__lf.native` and reads what the UI sends):
 *
 * - ✂ TRIM (F16, `src/ui/looper/Trim.tsx`): the pill shows on a committed lane (PLAYING or STOPPED) over a
 *   loop of two whole bars or more, and on no other (EMPTY, RECORDING, OVERDUBBING, a one-bar loop, a
 *   loop of no whole number of bars); it is disabled while the lane stops at the loop end. Its popover
 *   starts at half the loop's bars, steps inside 1..bars−1, sends `Trim` with the chosen bars and closes;
 *   Escape closes it and hands focus back to the pill, an outside click closes it, neither sends. An
 *   engine refusal lands on its lane. ↶ UNDO names the trim.
 * - Halve (`halveTrack`, `src/app/actions.ts`): sends the selected lane's `Trim` at half its bars,
 *   rounded down; with nothing to halve it sends nothing and says why on the lane; MIDI learn lists it.
 *   In web mode (a second page, no engine) it says "needs the native engine" and no TRIM pill shows.
 * - E10: a free later take (FIXED off) runs until the press, so its record head sweeps the loops it has
 *   reached (1.5 loops in: three quarters of a lane two loops wide), never a close at the loop's end.
 *
 * Cannot see the native engine (lf-engine `tests/trim.rs` and `tests/multiply.rs` hold the audio), Tauri
 * IPC or any timing: the fake answers no command by itself, so every state the DOM shows was scripted.
 * Screenshots of the pill row and the popover land in logs/lane-length/ for the eye.
 * Run: pnpm probe lane-length
 */
import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { probe } from '../harness/probe.ts';

const RATE = 48000;
const BAR = 2 * RATE; // one 4/4 bar at 120 BPM
const outDir = 'logs/lane-length';

const lane = (state, extra = {}) => ({
  state,
  length: 0,
  armed: false,
  autoArmed: false,
  canUndo: false,
  canReverse: false,
  reversed: false,
  stopAt: null,
  retakePass: 0,
  ...extra,
});
const committed = (state, bars, extra = {}) => lane(state, { length: bars * BAR, canReverse: true, ...extra });
const laneEvent = (i, info, frame = 0) => ({ Lane: { frame, lane: i, info } });
const transport = (master, locked = true, bpm = 120) => ({ Transport: { frame: 0, master, bpm, locked } });
/** The clock anchor with `frame` rendering now. */
const anchorAt = (frame) => ({ frame, atMs: Date.now(), rate: RATE, grid: 0 });

await probe(async ({ open }) => {
  await mkdir(outDir, { recursive: true });
  const { page, consoleErrors } = await open({
    viewport: { width: 1600, height: 900 },
    init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)),
  });
  let seq = 0;
  const emit = (frame) => page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], ...frame });
  const sent = () => page.evaluate(() => window.__lf.native.sent.slice());
  const clearSent = () => page.evaluate(() => void (window.__lf.native.sent.length = 0));
  const trims = async () => (await sent()).filter((c) => c.Trim);
  const lanes = page.locator('.lp-lane');
  const pill = (i) => lanes.nth(i).getByRole('button', { name: `Trim track ${i + 1}`, exact: true });
  const well = (i) => lanes.nth(i).locator('.lp-lane__wellmsg');
  const runAction = (id) => page.evaluate((a) => import('/src/app/actions.ts').then((m) => m.runAction(a)), id);
  const shown = () => Promise.all([0, 1, 2, 3, 4].map((i) => pill(i).count().then((n) => n === 1)));

  await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });
  await emit({
    reset: true,
    settings: [],
    events: [...[0, 1, 2, 3, 4].map((i) => laneEvent(i, lane('Empty'))), transport(0, false), { Selected: { frame: 0, lane: 0 } }],
    anchor: anchorAt(0),
    meter: { peak: 0, clip: false },
  });

  // ── Where the pill shows ─────────────────────────────────────────────────────────────────────────
  assert.deepEqual(await shown(), [false, false, false, false, false], 'no TRIM on an empty looper');
  await emit({ events: [laneEvent(0, committed('Playing', 1)), transport(BAR)] });
  assert.equal(await pill(0).count(), 0, 'a one-bar loop has nothing to trim');
  await emit({
    events: [
      transport(8 * BAR),
      laneEvent(0, committed('Playing', 8)),
      laneEvent(1, committed('Stopped', 8)),
      laneEvent(2, lane('Overdubbing', { length: 8 * BAR })),
      laneEvent(3, lane('Recording')),
      laneEvent(4, lane('Empty')),
    ],
  });
  const visibility = await shown();
  console.log('TRIM shown on lanes', JSON.stringify(visibility));
  assert.deepEqual(visibility, [true, true, false, false, false], 'PLAYING and STOPPED lanes of an 8-bar loop only');
  await emit({ events: [transport(8 * BAR + 480)] });
  assert.deepEqual(await shown(), [false, false, false, false, false], 'a loop of no whole number of bars: no TRIM');
  await emit({ events: [transport(8 * BAR), laneEvent(1, committed('Playing', 8, { stopAt: 9 * BAR }))] });
  assert.ok(await pill(1).isDisabled(), 'disabled while the lane stops at the loop end');
  assert.ok(!(await pill(0).isDisabled()));

  // ── The popover ──────────────────────────────────────────────────────────────────────────────────
  const dialog = page.getByRole('dialog', { name: 'Trim track 1' });
  const value = () => dialog.locator('.trim__val').textContent().then((t) => t.trim());
  const more = dialog.getByRole('button', { name: 'Keep more bars', exact: true });
  const fewer = dialog.getByRole('button', { name: 'Keep fewer bars', exact: true });
  await pill(0).click();
  await dialog.waitFor();
  assert.equal(await pill(0).getAttribute('aria-expanded'), 'true');
  assert.equal(await value(), '4', 'N starts at half of 8 bars');
  await page.screenshot({ path: `${outDir}/popover.png` });
  for (let k = 0; k < 5; k++) await more.click({ force: true });
  assert.equal(await value(), '7', 'at most the loop less one bar');
  assert.ok(await more.isDisabled());
  for (let k = 0; k < 8; k++) await fewer.click({ force: true });
  assert.equal(await value(), '1', 'at least one bar');
  assert.ok(await fewer.isDisabled());
  await more.click();
  await more.click();
  assert.match(await dialog.locator('.trim__unit').textContent(), /BARS/);
  await clearSent();
  await dialog.getByRole('button', { name: 'Trim track 1 to its first 3 bars', exact: true }).click();
  await dialog.waitFor({ state: 'detached' });
  assert.deepEqual(await trims(), [{ Trim: [0, 3] }], 'TRIM sends the chosen bars and closes');

  await pill(0).click();
  await dialog.waitFor();
  assert.equal(await value(), '4', 'a new popover starts at half again');
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'detached' });
  assert.equal(await page.evaluate(() => document.activeElement?.getAttribute('aria-label')), 'Trim track 1', 'Escape hands focus back to the pill');
  await pill(0).click();
  await dialog.waitFor();
  await page.mouse.click(8, 890);
  await dialog.waitFor({ state: 'detached' });
  assert.deepEqual(await trims(), [{ Trim: [0, 3] }], 'Escape and an outside click send nothing');

  // An engine refusal lands on its lane; ↶ UNDO names the trim.
  await emit({ events: [{ Refused: { frame: BAR, lane: 0, reason: 'Capturing' } }] });
  assert.match(await well(0).textContent(), /this track is recording, stop it first/);
  await emit({ events: [laneEvent(0, committed('Playing', 8, { canUndo: true }))] });
  const undo = lanes.nth(0).getByRole('button', { name: 'Track 1 undo or redo the last overdub or trim', exact: true });
  assert.equal((await undo.textContent()).trim(), '↶ UNDO');
  assert.equal(await undo.getAttribute('title'), 'Undo / redo the last overdub or trim');
  // The fullest pill row (FX, MUTE, ↶ UNDO, ↺ REV, ⧉ COPY, ✂ TRIM) fits its cluster at a wide and a narrow
  // window.
  for (const [width, height] of [[1600, 900], [1000, 700]]) {
    await page.setViewportSize({ width, height });
    await page.waitForTimeout(100);
    await page.locator('.lp-lane__mods').first().screenshot({ path: `${outDir}/pill-row-${width}.png` });
    const row = await page.evaluate(() => {
      const mods = document.querySelector('.lp-lane__mods');
      const box = mods.getBoundingClientRect();
      return [...mods.children].map((el) => {
        const r = el.getBoundingClientRect();
        return { text: el.innerText.trim(), w: Math.round(r.width), need: el.scrollWidth, inside: r.left >= box.left - 0.5 && r.right <= box.right + 0.5, clipped: el.scrollWidth > el.clientWidth + 1 };
      });
    });
    console.log(`pill row at ${width} px`, JSON.stringify(row));
    assert.equal(row.length, 6, 'six pills');
    assert.ok(row.every((p) => p.inside && !p.clipped), `every pill fits the row and its legend fits the pill at ${width} px`);
  }
  await page.setViewportSize({ width: 1600, height: 900 });

  // ── Halve ────────────────────────────────────────────────────────────────────────────────────────
  await clearSent();
  await runAction('halveTrack');
  assert.deepEqual(await trims(), [{ Trim: [0, 4] }], 'halve an 8-bar loop: its first 4 bars');
  await emit({ events: [transport(7 * BAR), laneEvent(0, committed('Playing', 7))] });
  await runAction('halveTrack');
  assert.deepEqual((await trims()).at(-1), { Trim: [0, 3] }, 'halve a 7-bar loop: 3, rounded down');
  await pill(0).click();
  await dialog.waitFor();
  assert.equal(await value(), '3', "the popover's N starts at half of 7 bars, rounded down");
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'detached' });
  const refusedOn = async (i, reason, what) => {
    await clearSent();
    await emit({ events: [{ Selected: { frame: 0, lane: i } }] });
    await runAction('halveTrack');
    await page.waitForTimeout(100);
    assert.deepEqual(await trims(), [], `${what}: nothing sent`);
    assert.match(await well(i).textContent(), reason, `${what}: the reason on the lane`);
  };
  await refusedOn(4, /nothing to trim, the loop needs two bars or more/, 'an EMPTY lane');
  await refusedOn(2, /this track is recording, stop it first/, 'an overdubbing lane');
  await emit({ events: [transport(BAR), laneEvent(0, committed('Playing', 1))] });
  await refusedOn(0, /nothing to trim, the loop needs two bars or more/, 'a one-bar loop');
  const label = await page.evaluate(() => import('/src/app/actions.ts').then((m) => m.ACTION_LABELS.halveTrack));
  assert.equal(label, 'Halve track (keep first half)');
  await page.evaluate(() => window.__lf.ui.openSettings());
  const picked = page.locator('[aria-label="Action to learn"] option', { hasText: 'Halve track (keep first half)' });
  await picked.waitFor({ state: 'attached', timeout: 5000 });
  assert.equal(await picked.getAttribute('value'), 'halveTrack', 'MIDI learn lists it');
  await page.keyboard.press('Escape');

  // ── E10: a free take's record head sweeps the loops it has reached ─────────────────────────────────
  await emit({ events: [laneEvent(1, lane('Empty')), laneEvent(2, lane('Empty')), laneEvent(3, lane('Empty'))] });
  const recHeadAt = (i) =>
    lanes.nth(i).locator('canvas').evaluate((c) => {
      const row = c.getContext('2d').getImageData(0, 0, c.width, 1).data;
      const hits = [];
      for (let x = 0; x < c.width; x++) {
        const [r, g, b, a] = row.slice(4 * x, 4 * x + 4);
        if (a > 200 && r > 200 && g < 120 && b < 140) hits.push(x);
      }
      return hits.length ? hits[hits.length >> 1] / c.width : -1;
    });
  /** Lane `i`'s head `loops` loops into a free take, and where the lane's `span` loops put it: the clock
   * runs on while the probe measures, so the head may sit up to that time past `loops`. */
  const headAfter = async (i, loops, span) => {
    const t0 = Date.now();
    await emit({ events: [laneEvent(i, lane('Recording'), 0)], anchor: anchorAt(loops * BAR) });
    let at = -1;
    for (let tries = 0; tries < 40 && at < 0; tries++) {
      await page.waitForTimeout(50);
      at = await recHeadAt(i);
    }
    const late = ((Date.now() - t0) / 1000) * RATE;
    await emit({ events: [laneEvent(i, lane('Empty'))] });
    const [lo, hi] = [(loops * BAR) / (span * BAR), (loops * BAR + late) / (span * BAR)];
    console.log(`free take ${loops} loops in: the head at ${at.toFixed(3)} of the lane, ${lo.toFixed(3)}..${hi.toFixed(3)} over ${span} loop(s)`);
    return at >= lo - 0.02 && at <= hi + 0.02;
  };
  assert.ok(await headAfter(1, 0.5, 1), 'half a loop in, the lane spans the loop');
  assert.ok(await headAfter(2, 1.5, 2), '1.5 loops in, the lane spans two loops: the take runs on');
  assert.deepEqual(consoleErrors, [], 'no console errors');

  // ── Web mode: no TRIM, and halve says why ─────────────────────────────────────────────────────────
  const web = await open({ viewport: { width: 1600, height: 900 } });
  await web.page.evaluate(async () => {
    const lf = window.__lf;
    const { defaultFxStates } = await import('/src/audio/fx/fx.ts');
    lf.looper.init();
    const frames = Math.round(lf.engine.ctx.sampleRate * 2 * 4);
    const pcm = new Float32Array(frames).map((_, f) => 0.2 * Math.sin(f / 40));
    await lf.looper.loadSession({ bpm: 120, bars: 4, masterLengthFrames: frames, tracks: [{ index: 0, pcm, volume: 1, muted: false, reversed: false, state: 'PLAYING', fx: defaultFxStates() }] });
  });
  await web.page.waitForFunction(() => window.__lf.looper.stateOf(0) === 'PLAYING');
  assert.equal(await web.page.locator('.lp-pb--trim').count(), 0, 'web mode shows no TRIM');
  await web.page.evaluate(() => import('/src/app/actions.ts').then((m) => m.runAction('halveTrack')));
  assert.match(await web.page.locator('.lp-lane').nth(0).locator('.lp-lane__wellmsg').textContent(), /needs the native engine/);
  assert.deepEqual(web.consoleErrors, [], 'no console errors in web mode');
});
