/**
 * A track's length after recording, in engine mode on the web engine fake (`src/platform/host.web.ts`,
 * the `engine-seam` pattern: an init script sets `window.__lfEngineFake`, the probe scripts the feed
 * through `__lf.native` and reads what the UI sends):
 *
 * - ✂ TRIM (F16, `src/ui/looper/Trim.tsx`), in the lane's EDIT popover (`Edit.tsx`): EDIT opens on a
 *   committed lane, and TRIM in it is enabled on a committed lane (PLAYING or STOPPED) over a loop of two
 *   whole bars or more, on no other (EMPTY, RECORDING, OVERDUBBING, a one-bar loop, a loop of no whole
 *   number of bars, where its line says why); it is disabled while the lane stops at the loop end. Its
 *   stepper starts at half the loop's bars, steps inside 1..bars−1, sends `Trim` with the chosen bars and
 *   closes; Escape closes it and hands focus back to EDIT, an outside click closes it, neither sends. TRIM
 *   pressed by key hands focus back to EDIT too, and the open stepper's TRIM is disabled while the lane
 *   stops. The menu is reachable by key (EDIT, then Tab to its edits); COPY says "track" in its line,
 *   hover and name. An engine refusal lands on its lane. ↶ UNDO names the trim.
 * - COPY or WIDE focused in the open menu when the last empty lane fills (its own press by key, or a pedal's
 *   copy on the feed): the menu closes and the focus is on the lane's EDIT, never dropped to the page.
 * - The lane's pill row is five fixed columns (FX, MUTE, ↺ REV, ↶ UNDO, EDIT) whose legends fit their pills
 *   at a wide and a narrow window, and ↶ UNDO holds its column, disabled, where there is nothing to undo.
 * - Halve (`halveTrack`, `src/app/actions.ts`): sends the engine's `Halve` (on the engine's own selection,
 *   or `ActionOn` a named track), never a `Trim` with bars the UI counted: the engine judges the lane and
 *   the loop's bars as they stand when the press lands (lf-engine `tests/actions.rs`). Its refusal, from
 *   the feed, lands on its lane; MIDI learn lists it.
 * - E10: a free later take (FIXED off) runs until the press, so its record head sweeps the loops it has
 *   reached (1.5 loops in: three quarters of a lane two loops wide), never a close at the loop's end.
 *
 * Cannot see the native engine (lf-engine `tests/trim.rs` and `tests/multiply.rs` hold the audio), Tauri
 * IPC or any timing: the fake answers no command by itself, so every state the DOM shows was scripted.
 * Screenshots of the pill row, the EDIT menu and the TRIM stepper land in logs/lane-length/ for the eye.
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
  fading: false,
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
  const { page } = await open({
    viewport: { width: 1600, height: 900 },
    init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)),
  });
  let seq = 0;
  const emit = (frame) => page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], ...frame });
  const sent = () => page.evaluate(() => window.__lf.native.sent.slice());
  const clearSent = () => page.evaluate(() => void (window.__lf.native.sent.length = 0));
  const trims = async () => (await sent()).filter((c) => c.Trim);
  const lanes = page.locator('.lp-lane');
  const edit = (i) => lanes.nth(i).getByRole('button', { name: `Edit track ${i + 1}`, exact: true });
  const menu = (i) => page.getByRole('dialog', { name: `Edit track ${i + 1}` });
  const trimItem = (i) => menu(i).getByRole('button', { name: `Trim track ${i + 1}`, exact: true });
  /** Lane `i`'s TRIM: null while its EDIT is disabled, else whether TRIM in its menu is enabled. */
  const trimOk = async (i) => {
    if (await edit(i).isDisabled()) return null;
    await edit(i).click();
    await menu(i).waitFor();
    const ok = !(await trimItem(i).isDisabled());
    await page.keyboard.press('Escape');
    await menu(i).waitFor({ state: 'detached' });
    return ok;
  };
  /** EDIT, then TRIM in its menu: lane 1's TRIM stepper. */
  const openTrim = async () => {
    await edit(0).click();
    await trimItem(0).click();
    await dialog.waitFor();
  };
  const well = (i) => lanes.nth(i).locator('.lp-lane__wellmsg');
  const runAction = (id, target = null) => page.evaluate(([a, t]) => import('/src/app/actions.ts').then((m) => m.runAction(a, t)), [id, target]);
  const shown = async () => {
    const out = [];
    for (const i of [0, 1, 2, 3, 4]) out.push(await trimOk(i));
    return out;
  };

  await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });
  await emit({
    reset: true,
    settings: [],
    events: [...[0, 1, 2, 3, 4].map((i) => laneEvent(i, lane('Empty'))), transport(0, false), { Selected: { frame: 0, lane: 0 } }],
    anchor: anchorAt(0),
    meter: { peak: [0, 0], clip: [false, false] },
  });

  // ── Where TRIM is enabled ────────────────────────────────────────────────────────────────────────
  const dialog = page.getByRole('dialog', { name: 'Trim track 1' });
  assert.deepEqual(await shown(), [null, null, null, null, null], 'no EDIT on an empty looper');
  await emit({ events: [laneEvent(0, committed('Playing', 1)), transport(BAR)] });
  assert.equal(await trimOk(0), false, 'a one-bar loop has nothing to trim');
  await edit(0).click();
  assert.equal((await trimItem(0).locator('.edit__hint').textContent()).trim(), 'nothing to trim, the loop needs two bars or more', 'and its line says why');
  await page.keyboard.press('Escape');
  await menu(0).waitFor({ state: 'detached' });
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
  console.log('TRIM enabled on lanes (null: EDIT disabled)', JSON.stringify(visibility));
  assert.deepEqual(visibility, [true, true, null, null, null], 'PLAYING and STOPPED lanes of an 8-bar loop only');
  await emit({ events: [transport(8 * BAR + 480)] });
  assert.deepEqual(await shown(), [false, false, null, null, null], 'a loop of no whole number of bars: no TRIM');
  await emit({ events: [transport(8 * BAR), laneEvent(1, committed('Playing', 8, { stopAt: 9 * BAR }))] });
  assert.equal(await trimOk(1), false, 'disabled while the lane stops at the loop end');
  assert.equal(await trimOk(0), true);

  // ── The menu by key: EDIT, then Tab to its edits; Escape hands focus back to EDIT ────────────────────
  // A focus the user moves (Escape above handed EDIT its focus on the app's behalf, which keeps the transport
  // keys): another control first, then EDIT.
  await lanes.nth(0).getByRole('button', { name: 'Track 1 mute', exact: true }).focus();
  await edit(0).focus();
  await page.keyboard.press('Enter');
  await menu(0).waitFor();
  assert.equal(await edit(0).getAttribute('aria-expanded'), 'true');
  await page.waitForFunction(() => document.activeElement?.getAttribute('aria-label') === 'Edit track 1' && document.activeElement.getAttribute('role') === 'dialog', undefined, { timeout: 5000 });
  const tabbed = [];
  for (let k = 0; k < 3; k++) {
    await page.keyboard.press('Tab');
    tabbed.push(await page.evaluate(() => document.activeElement?.getAttribute('aria-label')));
  }
  console.log('Tab through the EDIT menu', JSON.stringify(tabbed));
  assert.deepEqual(tabbed, ['Copy track 1 to the first empty track', 'Copy track 1 wide to the first empty track', 'Trim track 1'], 'every edit is reachable by key');
  // COPY's line, its hover and its name say "track", as the lanes do on screen (Track 1, Selected track).
  const copyWords = await menu(0).locator('.edit__item').first().evaluate((el) => ({ hint: el.querySelector('.edit__hint').textContent.trim(), title: el.title }));
  console.log('COPY words', JSON.stringify(copyWords));
  assert.deepEqual(copyWords, { hint: 'to the first empty track', title: 'Copy this track to the first empty track' }, 'COPY says track, never lane');
  await page.screenshot({ path: `${outDir}/edit-menu.png` });
  await page.keyboard.press('Escape');
  await menu(0).waitFor({ state: 'detached' });
  assert.equal(await page.evaluate(() => document.activeElement?.getAttribute('aria-label')), 'Edit track 1', 'Escape hands focus back to EDIT');
  // TRIM by key turns the menu into the stepper.
  await page.keyboard.press('Tab'); // off EDIT and back: a focus the user moved, so EDIT keeps its keys
  await page.keyboard.press('Shift+Tab');
  await page.keyboard.press('Enter');
  await menu(0).waitFor();
  for (let k = 0; k < 3; k++) await page.keyboard.press('Tab');
  await page.keyboard.press('Enter');
  await dialog.waitFor();
  assert.equal(await menu(0).count(), 0, 'the stepper takes the menu\'s place');
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'detached' });

  // ── The stepper ──────────────────────────────────────────────────────────────────────────────────
  const value = () => dialog.locator('.trim__val').textContent().then((t) => t.trim());
  const more = dialog.getByRole('button', { name: 'Keep more bars', exact: true });
  const fewer = dialog.getByRole('button', { name: 'Keep fewer bars', exact: true });
  await openTrim();
  assert.equal(await edit(0).getAttribute('aria-expanded'), 'true');
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

  await openTrim();
  assert.equal(await value(), '4', 'a new popover starts at half again');
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'detached' });
  assert.equal(await page.evaluate(() => document.activeElement?.getAttribute('aria-label')), 'Edit track 1', 'Escape hands focus back to EDIT');
  await openTrim();
  await page.mouse.click(8, 890);
  await dialog.waitFor({ state: 'detached' });
  assert.deepEqual(await trims(), [{ Trim: [0, 3] }], 'Escape and an outside click send nothing');
  // TRIM pressed by key sends and hands focus back to EDIT, as Escape does.
  const go = dialog.locator('.trim__go');
  await openTrim();
  await go.focus();
  await page.keyboard.press('Enter');
  await dialog.waitFor({ state: 'detached' });
  assert.deepEqual(await trims(), [{ Trim: [0, 3] }, { Trim: [0, 4] }], 'TRIM by key sends the chosen bars');
  assert.equal(await page.evaluate(() => document.activeElement?.getAttribute('aria-label')), 'Edit track 1', 'TRIM by key hands focus back to EDIT');
  // The open popover follows the lane's gate: a stop pending under it (a pedal) greys TRIM out, and its
  // end gives it back.
  await openTrim();
  await emit({ events: [laneEvent(0, committed('Playing', 8, { stopAt: 9 * BAR }))] });
  const gated = { edit: await edit(0).isDisabled(), go: await go.isDisabled() };
  await emit({ events: [laneEvent(0, committed('Playing', 8))] });
  const reopened = await go.isDisabled();
  console.log('TRIM in the open popover while the lane stops, then after', JSON.stringify({ gated, reopened }));
  assert.deepEqual(gated, { edit: false, go: true }, 'a lane stopping at the loop end disables the open stepper\'s TRIM (EDIT stays: COPY does not wait)');
  assert.equal(reopened, false, 'the gate opening again enables it');
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'detached' });
  assert.deepEqual(await trims(), [{ Trim: [0, 3] }, { Trim: [0, 4] }], 'the gated popover sent nothing');

  // An engine refusal lands on its lane; ↶ UNDO names the trim.
  await emit({ events: [{ Refused: { frame: BAR, lane: 0, reason: 'Capturing' } }] });
  assert.match(await well(0).textContent(), /this track is recording, stop it first/);
  await emit({ events: [laneEvent(0, committed('Playing', 8, { canUndo: true }))] });
  const undo = lanes.nth(0).getByRole('button', { name: 'Track 1 undo or redo the last overdub or trim', exact: true });
  assert.equal((await undo.textContent()).trim(), '↶ UNDO');
  assert.equal(await undo.getAttribute('title'), 'Undo / redo the last overdub or trim');
  // The pill row (FX, MUTE, ↺ REV, ↶ UNDO, EDIT) fits its cluster at a wide and a narrow window, and ↶ UNDO
  // holds its column, disabled, on a lane with nothing to undo.
  const undo4 = lanes.nth(3).getByRole('button', { name: 'Track 4 undo or redo the last overdub or trim', exact: true });
  assert.ok(await undo4.isDisabled(), 'nothing to undo: UNDO is disabled, not absent');
  assert.equal(await undo4.getAttribute('title'), 'Nothing to undo, overdub first');
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
    assert.deepEqual(row.map((p) => p.text), ['FX', 'MUTE', '↺ REV', '↶ UNDO', 'EDIT'], 'five pills, in their columns');
    assert.ok(row.every((p) => p.inside && !p.clipped), `every pill fits the row and its legend fits the pill at ${width} px`);
  }
  await page.setViewportSize({ width: 1600, height: 900 });

  // ── Halve ────────────────────────────────────────────────────────────────────────────────────────
  await clearSent();
  await runAction('halveTrack');
  await runAction('halveTrack', 2);
  const sentHalves = await sent();
  assert.deepEqual(sentHalves, [{ Action: 'Halve' }, { ActionOn: [2, 'Halve'] }], "halve: the engine's action, on its own selection or the named track");
  await emit({ events: [transport(7 * BAR), laneEvent(0, committed('Playing', 7))] });
  await openTrim();
  assert.equal(await value(), '3', "the popover's N starts at half of 7 bars, rounded down");
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'detached' });
  // Whatever the UI shows of the lane (an EMPTY one, an overdubbing one), the press goes to the engine,
  // whose refusal lands on its lane.
  const refusedOn = async (i, reason, text, what) => {
    await clearSent();
    await emit({ events: [{ Selected: { frame: 0, lane: i } }] });
    await runAction('halveTrack');
    assert.deepEqual(await sent(), [{ Action: 'Halve' }], `${what}: the press goes to the engine`);
    await emit({ events: [{ Refused: { frame: 0, lane: i, reason } }] });
    await page.waitForTimeout(100);
    assert.match(await well(i).textContent(), text, `${what}: the engine's reason on the lane`);
  };
  await refusedOn(4, 'NoTrim', /nothing to trim, the loop needs two bars or more/, 'an EMPTY lane');
  await refusedOn(2, 'Capturing', /this track is recording, stop it first/, 'an overdubbing lane');
  await emit({ events: [transport(BAR), laneEvent(0, committed('Playing', 1))] });
  await refusedOn(0, 'NoTrim', /nothing to trim, the loop needs two bars or more/, 'a one-bar loop');
  assert.deepEqual(await trims(), [], 'no Trim with bars the UI counted');
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

  // ── COPY or WIDE by key into the last empty lane: the focus stays in the lane ─────────────────────────
  // The copy fills the last empty lane, which closes COPY and WIDE in every lane: the focused button must not
  // drop the focus to the page, where the next Space is no longer this lane's.
  const focusAt = () =>
    page.evaluate(() => {
      const el = document.activeElement;
      return { label: el?.getAttribute('aria-label') ?? el?.tagName, lane: [...document.querySelectorAll('.lp-lane')].findIndex((l) => l.contains(el)) };
    });
  const lastFree = (on) => [transport(8 * BAR), ...[0, 1, 2, 3].map((i) => laneEvent(i, committed('Playing', 8))), laneEvent(4, on ? committed('Playing', 8) : lane('Empty'))];
  for (const [word, sends] of [['Copy track 1 to the first empty track', { Copy: 0 }], ['Copy track 1 wide to the first empty track', { CopyWide: 0 }]]) {
    for (const how of ['pressed', 'a pedal']) {
      await emit({ events: lastFree(false) });
      await lanes.nth(0).getByRole('button', { name: 'Track 1 mute', exact: true }).focus();
      await edit(0).focus();
      await page.keyboard.press('Enter');
      await menu(0).waitFor();
      await page.waitForFunction(() => document.activeElement?.getAttribute('role') === 'dialog', undefined, { timeout: 5000 });
      while ((await focusAt()).label !== word) await page.keyboard.press('Tab');
      await clearSent();
      if (how === 'pressed') {
        await page.keyboard.press('Enter');
        assert.deepEqual(await sent(), [sends], `${word} by key sends its edit`);
      }
      await emit({ events: [{ Copied: { frame: BAR, from: 0, to: 4, feedback: 1 } }, ...lastFree(true)] });
      await page.waitForTimeout(150);
      const at = await focusAt();
      console.log(`focus after ${word} (${how}) filled the last empty lane`, JSON.stringify(at));
      assert.deepEqual(at, { label: 'Edit track 1', lane: 0 }, `${word} (${how}): the focus is on the lane's EDIT, not dropped`);
      if (await menu(0).count()) {
        await page.keyboard.press('Escape');
        await menu(0).waitFor({ state: 'detached' });
      }
    }
  }
});
