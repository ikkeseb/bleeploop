/**
 * The lane mix as the engine applied it, and the controls' overlay over it (D21 slice 3b; the rule lives
 * in `src/ui/state/engine-store.ts`, the lane mix section), in engine mode on the web engine fake
 * (`src/platform/host.web.ts`, the `engine-seam` pattern). The fake reports each change of a lane's mix
 * as the engine does (`Mix`, a microtask after the change); its seams hold that report (`holdEcho`), hold
 * the commands unapplied (`holdApply`) or refuse a batch carrying a mix command (`refuseMix`). The probe drives the real controls:
 *
 * - (a) a refused gesture: the fader and an FX division select go back to the applied value; a command
 *   the engine has not applied yet (`holdApply`) keeps the fader on the request until its report; the
 *   FX drawer names a parameter by track, effect and parameter, and its ✕ pressed by key hands focus
 *   back to the lane's FX pill (by pointer, to nothing);
 * - (b) an equal report during a drag keeps the drag's protection: an unequal `Mix` after it does not move
 *   the fader, and the release and the matching report leave it on the applied value; an equal report
 *   while the pointer is down with none after it: the release ends the overlay;
 * - (c) an unequal `Mix` during a drag whose reports are held does not move the fader, nor does the
 *   release; the matching report shows the applied value;
 * - (b), (c), (f): once settled, a scripted unequal `Mix` moves the control (no overlay left behind);
 * - (d) a release before the report (two key steps, held): each step builds on the shown value, no
 *   snap-back, and the report ends the overlay with no visible change (a later unequal `Mix` then moves
 *   the fader);
 * - (e) MUTE sends the engine's toggle on each press (two quick presses: two toggles, one batch) and
 *   shows only the engine's mute, following its reports;
 * - (f) an effect's bypass pressed twice before any report builds on what it shows: it ends unbypassed;
 * - (i) the FX pill's lamp lights while the engine's mix has any of the lane's effects engaged, the
 *   drawer open or not, never on an EMPTY lane; a double-click on the fader's thumb sends 0 dB once, and
 *   nothing when it already shows 0 dB or the lane is EMPTY;
 * - (h) a lane's `Cleared` and a `Copied` into it cancel its pending overlay; the lane going EMPTY and a
 *   reset frame too;
 * - (g) autosave: a save whose snapshot pinned another mix (B) than the one it inspected (A) does not
 *   record A as saved, so the jam saves again; once a save's snapshot carries A, none follows.
 *
 * Cannot see the native engine's reports (their timing, their coalescing across a feed tick), WebView2's
 * pointer handling or the feel of the fader: the owner's ear and hand judge that. Run: pnpm probe lane-mix
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

const RATE = 48000;
const BAR = 2 * RATE; // one 4/4 bar at 120 BPM

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
const playing = () => lane('Playing', { length: BAR, canReverse: true });
const transport = (master) => ({ Transport: { frame: 0, master, bpm: 120, locked: master > 0 } });

await probe(async ({ open }) => {
  const { page, consumeConsoleErrors } = await open({
    viewport: { width: 1600, height: 900 },
    init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)),
  });
  await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });

  let seq = 0;
  const emit = (frame) => page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], ...frame });
  /** A scripted report of lane `i`'s mix: the defaults with `patch` (not the fake's own: a stale or
   * foreign report). */
  const emitMix = (i, patch) =>
    page.evaluate(async ([i, patch, seq]) => {
      const { defaultLaneMix } = await import('/src/platform/host.web.ts');
      window.__lf.native.emit({ seq, reset: false, events: [{ Mix: { frame: 0, lane: i, mix: { ...defaultLaneMix(), ...patch } } }] });
    }, [i, patch, ++seq]);
  const resetFrame = () =>
    emit({
      reset: true,
      settings: [],
      events: [transport(BAR), ...[0, 1, 2].map((i) => laneEvent(i, playing())), laneEvent(3, lane('Empty')), laneEvent(4, lane('Empty')), { Selected: { frame: 0, lane: 0 } }],
      anchor: { frame: 0, atMs: Date.now(), rate: RATE, grid: 0 },
      meter: { peak: [0, 0], clip: [false, false] },
    });
  const sent = () => page.evaluate(() => window.__lf.native.sent.slice());
  const clearSent = () => page.evaluate(() => void (window.__lf.native.sent.length = 0));
  const seam = (name, on) => page.evaluate(([name, on]) => void (window.__lf.native[name] = on), [name, on]);
  const fader = (i) => page.getByRole('slider', { name: `Track ${i + 1} volume`, exact: true });
  const shown = async (i) => Number(await fader(i).inputValue());
  const applied = (i) => page.evaluate((i) => window.__lf.looper.trackVolume(i), i);
  /** Wait until the fader of lane `i` shows `pct` (it may already). */
  const faderShows = (i, pct) =>
    page.waitForFunction(([i, pct]) => document.querySelector(`[aria-label="Track ${i + 1} volume"]`)?.value === String(pct), [i, pct], { timeout: 5000 });
  /** Wait until the engine's volume of lane `i` is `volume` (key steps sum in doubles: 0.75 + 0.05 + 0.05). */
  const engineHas = (i, volume) =>
    page.waitForFunction(([i, v]) => Math.abs(window.__lf.looper.trackVolume(i) - v) < 1e-9, [i, volume], { timeout: 5000 });
  /** Put the pointer down on lane `i`'s fader at `frac` of its width and drag to `to`; stays down. */
  const dragFrom = async (i, frac, to) => {
    const box = await fader(i).boundingBox();
    const y = box.y + box.height / 2;
    await page.mouse.move(box.x + box.width * frac, y);
    await page.mouse.down();
    await page.mouse.move(box.x + box.width * to, y, { steps: 4 });
  };
  /** Once lane `i`'s gesture has settled: a scripted unequal `Mix` (its volume `volume`) must move the
   * fader, so no overlay was left behind. Returns what the fader shows; then lets the fake report its own
   * mix again and waits for the fader to show `restore` (percent). */
  const followsMix = async (i, volume, restore) => {
    await seam('holdEcho', true);
    await emitMix(i, { volume });
    await faderShows(i, Math.round(volume * 100)).catch(() => {});
    const got = await shown(i);
    await seam('holdEcho', false);
    await faderShows(i, restore);
    return got;
  };
  const blur = () => page.evaluate(() => (document.activeElement instanceof HTMLElement ? document.activeElement.blur() : undefined));
  /** Open or close lane 1's FX drawer. */
  const fxDrawer = async (open) => {
    const button = lanes(page).nth(0).getByRole('button', { name: 'Track 1 FX', exact: true });
    if ((await button.getAttribute('aria-pressed')) !== String(open)) await button.click();
    await page.locator('.lp-drawer').waitFor({ state: open ? 'visible' : 'detached' });
  };

  await resetFrame();
  await faderShows(0, 100);

  // ── (a) A refused gesture returns the control to the applied value ────────────────────────────────
  await seam('refuseMix', true);
  await clearSent();
  await fader(0).fill('60');
  await page.waitForFunction(() => window.__lf.native.sent.some((c) => c.SetVolume?.[0] === 0), undefined, { timeout: 5000 });
  await faderShows(0, 100);
  const refused = { sent: await sent(), shown: await shown(0), applied: await applied(0) };
  console.log('(a) refused fader', JSON.stringify(refused));
  assert.deepEqual(refused.sent, [{ SetVolume: [0, 0.6] }, { SetVolume: [0, 0.6] }], 'the gesture sent its value, once more after the refusal (a refused batch ran nothing)');
  assert.equal(refused.shown, 100, 'the refused gesture returns to the applied value');
  assert.equal(refused.applied, 1, 'the engine never had it');
  // A select changes its own value before the engine has it: a refused change sets it back.
  await fxDrawer(true);
  await seam('refuseMix', false);
  const delay = page.getByRole('button', { name: 'Delay, track 1', exact: true });
  await delay.click();
  await page.waitForFunction(() => window.__lf.looper.fxState(0)[3].bypassed === false, undefined, { timeout: 5000 });
  const division = page.getByRole('combobox', { name: 'Track 1 delay time', exact: true });
  const before = await division.inputValue();
  await seam('refuseMix', true);
  await division.selectOption({ index: before === '3' ? 0 : 3 });
  await page.waitForFunction(
    (b) => document.querySelector('select[aria-label="Track 1 delay time"]')?.value === b,
    before,
    { timeout: 5000 },
  ).catch(() => {});
  const selectBack = await division.inputValue();
  console.log('(a) refused select', JSON.stringify({ before, after: selectBack }));
  assert.equal(selectBack, before, 'a refused division goes back to the applied one');
  await seam('refuseMix', false);
  assert.ok(consumeConsoleErrors('input batch failed').length > 0, 'a refused batch reaches the release log');
  // A parameter's name is its track, effect and parameter, with no readout read into it.
  const named = {
    time: await page.getByRole('combobox', { name: 'Track 1 delay time', exact: true }).count(),
    mix: await page.getByRole('slider', { name: 'Track 1 delay mix', exact: true }).count(),
  };
  console.log('(a) delay parameter names', JSON.stringify(named));
  assert.deepEqual(named, { time: 1, mix: 1 }, 'the delay parameters are named track + effect + parameter');
  // The drawer's ✕ pressed by key hands focus back to the lane's FX pill; by pointer it leaves none there.
  const closeFx = page.getByRole('button', { name: 'Close FX panel', exact: true });
  await closeFx.focus();
  await page.keyboard.press('Enter');
  await page.locator('.lp-drawer').waitFor({ state: 'detached' });
  const keyFocus = await page.evaluate(() => document.activeElement?.getAttribute('aria-label'));
  await fxDrawer(true);
  await closeFx.click();
  await page.locator('.lp-drawer').waitFor({ state: 'detached' });
  const pointerFocus = await page.evaluate(() => document.activeElement?.getAttribute('aria-label') ?? document.activeElement?.tagName);
  console.log('(a) focus after the FX drawer closed by key, then by pointer', JSON.stringify({ keyFocus, pointerFocus }));
  assert.equal(keyFocus, 'Track 1 FX', 'a keyboard close of the FX drawer hands focus back to its pill');
  assert.equal(pointerFocus, 'BODY', 'a pointer close of the FX drawer leaves no control focused');
  await blur();
  // A command the engine has not applied yet: the fader keeps the request until the engine reports it.
  await seam('holdApply', true);
  await fader(0).fill('90');
  const unapplied = { shown: await shown(0), applied: await applied(0) };
  await seam('holdApply', false);
  await engineHas(0, 0.9);
  console.log('(a) unapplied', JSON.stringify({ unapplied, applied: await applied(0), shown: await shown(0) }));
  assert.deepEqual(unapplied, { shown: 90, applied: 1 }, 'an unapplied command keeps the requested value shown');
  assert.equal(await shown(0), 90, 'and its report leaves it there');

  // ── (b) An equal report during a drag keeps its protection ───────────────────────────────────────
  await dragFrom(0, 0.66, 0.3);
  const dragB = await shown(0);
  await engineHas(0, dragB / 100); // the fake's report of the drag's value, while the pointer is down
  await seam('holdEcho', true);
  await emitMix(0, { volume: 1.2 });
  const duringB = { shown: await shown(0), applied: await applied(0) };
  await page.mouse.up();
  const releasedB = await shown(0);
  await seam('holdEcho', false); // the fake reports the drag's value again (it last reported 1.2)
  await engineHas(0, dragB / 100);
  await faderShows(0, dragB);
  console.log('(b)', JSON.stringify({ dragB, duringB, releasedB }));
  assert.ok(dragB < 100, 'the drag moved the fader');
  assert.deepEqual(duringB, { shown: dragB, applied: 1.2 }, 'an unequal Mix after an equal one does not move a held fader');
  assert.equal(releasedB, dragB, 'nor does the release');
  assert.equal(await followsMix(0, 0.45, dragB), 45, 'settled, the fader follows the engine again');
  // The equal report arrives while the pointer is down and none follows: the release itself ends the overlay.
  await dragFrom(0, 0.5, 0.8);
  const dragB2 = await shown(0);
  await engineHas(0, dragB2 / 100);
  await page.mouse.up();
  const followedB2 = await followsMix(0, 0.4, dragB2);
  console.log('(b) equal report while held', JSON.stringify({ dragB2, followedB2 }));
  assert.ok(dragB2 > dragB, 'the second drag moved the fader');
  assert.equal(followedB2, 40, 'the release after an equal report leaves no overlay: the fader follows the engine');

  // ── (c) An unequal Mix during a drag whose reports wait ──────────────────────────────────────────
  await seam('holdEcho', true);
  await dragFrom(0, 0.2, 0.5);
  const dragC = await shown(0);
  await emitMix(0, { volume: 0.1 });
  const duringC = await shown(0);
  await page.mouse.up();
  const releasedC = { shown: await shown(0), applied: await applied(0) };
  await seam('holdEcho', false);
  await engineHas(0, dragC / 100);
  await faderShows(0, dragC);
  console.log('(c)', JSON.stringify({ dragC, duringC, releasedC, applied: await applied(0) }));
  assert.notEqual(dragC, dragB, 'a second drag to another value');
  assert.equal(duringC, dragC, 'an unequal Mix during the drag does not move the fader');
  assert.deepEqual(releasedC, { shown: dragC, applied: 0.1 }, 'the release keeps the requested value over the stale report');
  assert.equal(await applied(0), dragC / 100, 'the matching report is the applied value the fader shows');
  assert.equal(await followsMix(0, 0.15, dragC), 15, 'settled, the fader follows the engine again');

  // ── (d) A release before the report: no snap-back, no visible change at the report ─────────────────
  await seam('holdEcho', true);
  await fader(0).focus();
  const watch = page.evaluate(() => {
    const text = document.querySelector('.lp-lane .lp-vdb');
    window.__dbSeen = [text.textContent];
    new MutationObserver(() => window.__dbSeen.push(text.textContent)).observe(text, { childList: true, characterData: true, subtree: true });
  });
  await watch;
  await page.keyboard.press('ArrowRight');
  await page.keyboard.press('ArrowRight');
  const steppedD = await shown(0);
  const releasedD = { shown: await shown(0), applied: await applied(0) };
  await seam('holdEcho', false);
  await engineHas(0, steppedD / 100);
  const settledD = await shown(0);
  const dbSeen = await page.evaluate(() => window.__dbSeen);
  await blur();
  // The report ended the overlay: a later Mix moves the fader (the fake's own report waits meanwhile).
  await seam('holdEcho', true);
  await emitMix(0, { volume: 0.3 });
  await faderShows(0, 30);
  await seam('holdEcho', false);
  await engineHas(0, steppedD / 100);
  console.log('(d)', JSON.stringify({ steppedD, releasedD, settledD, dbSeen }));
  assert.equal(steppedD, dragC + 10, 'each key step builds on the shown value, not the unreported engine one');
  assert.deepEqual(releasedD, { shown: steppedD, applied: dragC / 100 }, 'released before the report: no snap-back');
  assert.equal(settledD, steppedD, 'the report changes nothing visible');
  assert.ok(dbSeen.length >= 3 && dbSeen[1] !== dbSeen[0], 'the read-out moved with the steps');
  assert.equal(dbSeen.at(-1), dbSeen[2], 'and not at the report');
  assert.equal(new Set(dbSeen).size, 3, 'once per step');

  // ── (e) MUTE: the engine's toggle, the engine's state ─────────────────────────────────────────────
  const mute = page.getByRole('button', { name: 'Track 1 mute', exact: true });
  await clearSent();
  await mute.evaluate((b) => {
    b.click();
    b.click();
  });
  await page.waitForFunction(() => window.__lf.native.sent.length >= 2, undefined, { timeout: 5000 });
  const twice = { sent: await sent(), pressed: await mute.getAttribute('aria-pressed'), muted: await page.evaluate(() => window.__lf.looper.trackMuted(0)) };
  await mute.click();
  await page.waitForFunction(() => window.__lf.looper.trackMuted(0), undefined, { timeout: 5000 });
  const once = await mute.getAttribute('aria-pressed');
  await seam('holdEcho', true);
  await mute.click();
  const unreported = await mute.getAttribute('aria-pressed');
  await seam('holdEcho', false);
  await page.waitForFunction(() => !window.__lf.looper.trackMuted(0), undefined, { timeout: 5000 });
  const reported = await mute.getAttribute('aria-pressed');
  console.log('(e)', JSON.stringify({ twice, once, unreported, reported }));
  assert.deepEqual(twice.sent, [{ ActionOn: [0, 'Mute'] }, { ActionOn: [0, 'Mute'] }], 'two quick presses: two toggles');
  assert.deepEqual([twice.pressed, twice.muted], ['false', false], 'two toggles leave it unmuted');
  assert.equal(once, 'true', 'a press shows the reported mute');
  assert.equal(unreported, 'true', 'MUTE shows only the engine mute: nothing before the report');
  assert.equal(reported, 'false', 'and follows the report');

  // ── (f) A bypass pressed twice before any report ends unbypassed ─────────────────────────────────
  await fxDrawer(true);
  const filter = page.getByRole('button', { name: 'Filter, track 1', exact: true });
  await filter.click();
  await page.waitForFunction(() => window.__lf.looper.fxState(0)[0].bypassed === false, undefined, { timeout: 5000 });
  await seam('holdEcho', true);
  await clearSent();
  await filter.click();
  const between = await filter.getAttribute('aria-pressed');
  await filter.click();
  const pressedTwice = { sent: await sent(), pressed: await filter.getAttribute('aria-pressed') };
  await seam('holdEcho', false);
  const model = await page.evaluate(async () => (await import('/src/platform/host.web.ts')).fakeMixModel.lane(0).fx[0].bypassed);
  console.log('(f)', JSON.stringify({ between, pressedTwice, model }));
  assert.equal(between, 'false', 'the first press shows bypassed');
  assert.deepEqual(pressedTwice.sent, [{ SetFxBypass: [0, 'filter', true] }, { SetFxBypass: [0, 'filter', false] }], 'the second press builds on the first');
  assert.equal(pressedTwice.pressed, 'true', 'it ends unbypassed');
  assert.equal(model, false, 'as the engine ends');
  // Settled, the button follows the engine: a scripted Mix with the filter bypassed shows it bypassed.
  await page.waitForFunction(() => window.__lf.looper.fxState(0)[0].bypassed === false, undefined, { timeout: 5000 });
  await seam('holdEcho', true);
  await emitMix(0, { volume: await applied(0) });
  await page.waitForFunction(() => document.querySelector('[aria-label="Filter, track 1"]')?.getAttribute('aria-pressed') === 'false', undefined, { timeout: 5000 }).catch(() => {});
  const followedF = await filter.getAttribute('aria-pressed');
  await seam('holdEcho', false);
  await page.waitForFunction(() => document.querySelector('[aria-label="Filter, track 1"]')?.getAttribute('aria-pressed') === 'true', undefined, { timeout: 5000 });
  console.log('(f) settled', JSON.stringify({ followedF }));
  assert.equal(followedF, 'false', 'settled, the bypass follows the engine again');
  await fxDrawer(false);
  await blur();

  // ── (i) The FX pill's lamp and the fader's double-click ───────────────────────────────────────────
  // Lane 1 carries the filter and the delay (above). The pill's lamp lights from the engine's mix, the
  // drawer open or not (its pill then wears the engaged legend too); an EMPTY lane never lights it.
  const pill = (i) => lanes(page).nth(i).getByRole('button', { name: `Track ${i + 1} FX`, exact: true });
  const lamp = (i) => pill(i).evaluate((el) => el.classList.contains('has-fx'));
  const lampShows = (i, on) =>
    page.waitForFunction(([i, on]) => document.querySelectorAll('.lp-pb--fx')[i]?.classList.contains('has-fx') === on, [i, on], { timeout: 5000 });
  const engagedMix = () =>
    page.evaluate(async () => {
      const { defaultLaneMix } = await import('/src/platform/host.web.ts');
      const mix = defaultLaneMix();
      return mix.fx.map((s, k) => (k === 0 ? { ...s, bypassed: false } : s));
    });
  await lampShows(0, true);
  const closed = { lane1: await lamp(0), lane2: await lamp(1) };
  await fxDrawer(true);
  const drawerOpen = { lamp: await lamp(0), legend: await pill(0).evaluate((el) => el.classList.contains('is-on')) };
  await fxDrawer(false);
  await blur();
  await seam('holdEcho', true);
  await emitMix(0, { volume: await applied(0) }); // the engine reports every effect bypassed
  await lampShows(0, false).catch(() => {});
  const bypassedReport = await lamp(0);
  await emitMix(3, { fx: await engagedMix() }); // an EMPTY lane's mix with the filter engaged
  const emptyLane = await lamp(3);
  await emitMix(3, {});
  await seam('holdEcho', false);
  await emitMix(0, { volume: await applied(0), fx: await page.evaluate(async () => (await import('/src/platform/host.web.ts')).fakeMixModel.lane(0).fx) });
  await lampShows(0, true);
  console.log('(i) FX lamp', JSON.stringify({ closed, drawerOpen, bypassedReport, emptyLane }));
  assert.deepEqual(closed, { lane1: true, lane2: false }, 'the pill lights while any of its effects is engaged');
  assert.deepEqual(drawerOpen, { lamp: true, legend: true }, 'the open drawer: the legend, and the lamp still lit');
  assert.equal(bypassedReport, false, "the lamp follows the engine's mix");
  assert.equal(emptyLane, false, 'an EMPTY lane never lights it');
  // A double-click returns the fader to 0 dB: one send, none at 0 dB already, none on an EMPTY lane.
  await fader(1).fill('60');
  await engineHas(1, 0.6);
  await blur();
  await clearSent();
  const vbox = await fader(1).boundingBox();
  await fader(1).dblclick({ position: { x: vbox.width * 0.4, y: vbox.height / 2 } }); // on the thumb
  await engineHas(1, 1);
  const volumes = async () => (await sent()).filter((c) => c.SetVolume); // a click also selects the lane
  const reset = { sent: await volumes(), shown: await shown(1) };
  await blur();
  await clearSent();
  await fader(1).dblclick({ position: { x: vbox.width * (2 / 3), y: vbox.height / 2 } }); // on the thumb at unity
  await fader(3).dblclick({ force: true });
  await page.waitForTimeout(100);
  const idle = await volumes();
  await blur();
  console.log('(i) double-click', JSON.stringify({ reset, idle }));
  assert.deepEqual(reset, { sent: [{ SetVolume: [1, 1] }], shown: 100 }, 'a double-click on the thumb sends 0 dB once and shows it');
  assert.deepEqual(idle, [], 'at 0 dB already, or on an EMPTY lane, it sends nothing');

  // ── (h) The cancels: Cleared, a Copied into the lane, EMPTY, a reset frame ────────────────────────
  await seam('holdEcho', true);
  await fader(1).fill('50');
  const pendingH = await shown(1);
  await emit({ events: [{ Cleared: { frame: BAR, lane: 1 } }] });
  const afterCleared = await shown(1);
  await fader(2).fill('30');
  await emit({ events: [{ Copied: { frame: BAR, from: 0, to: 2, feedback: 1 } }] });
  const afterCopied = await shown(2);
  await seam('holdEcho', false);
  const source = await applied(0);
  await engineHas(2, source);
  const copiedShown = await shown(2);
  await seam('holdEcho', true);
  await fader(1).fill('70');
  await emit({ events: [laneEvent(1, lane('Empty'))] });
  const afterEmpty = await shown(1);
  await emit({ events: [laneEvent(1, playing())] });
  await fader(1).fill('80');
  await resetFrame();
  const afterReset = await shown(1);
  await seam('holdEcho', false);
  console.log('(h)', JSON.stringify({ pendingH, afterCleared, afterCopied, source, copiedShown, afterEmpty, afterReset }));
  assert.equal(pendingH, 50, 'the overlay shows while its report waits');
  assert.equal(afterCleared, 100, "the lane's Cleared cancels it");
  assert.equal(afterCopied, 100, 'a Copied into the lane cancels it');
  assert.equal(copiedShown, Math.round(source * 100), "the copy's Mix then shows the source's volume");
  assert.equal(afterEmpty, 100, 'the lane going EMPTY cancels it');
  assert.equal(afterReset, 100, 'a reset frame cancels it');

  // ── (g) Autosave: the saved fingerprint is the persisted snapshot's mix ───────────────────────────
  // One loop on lane 1, its jam saved and clean at A (the engine's mix). A change of its loop dirties it;
  // that save's snapshot pins B (another volume) once: it must not count A as saved, so a second save
  // follows (with A), and then none.
  const snapshots = () => page.evaluate(() => window.__lf.native.snapshots.length);
  /** Wait until no snapshot was asked for `quietMs` (autosave is clean). */
  const autosaveSettles = async (quietMs) => {
    let count = await snapshots();
    let since = Date.now();
    for (const end = Date.now() + 20000; Date.now() < end; ) {
      await page.waitForTimeout(250);
      const now = await snapshots();
      if (now !== count) [count, since] = [now, Date.now()];
      else if (Date.now() - since >= quietMs) return count;
    }
    throw new Error('autosave never settled');
  };
  await page.evaluate(async ([BAR]) => {
    const { encodeSessionBytes } = await import('/src/platform/engine-wire.ts');
    const { defaultLaneMix } = await import('/src/platform/host.web.ts');
    const native = window.__lf.native;
    const pcm = new Float32Array(BAR).fill(0.25);
    const script = (mix) =>
      (native.snapshotBytes = encodeSessionBytes({ rate: 48000, masterLengthFrames: BAR, bpm: 120,
        tracks: [{ index: 0, frames: BAR, reversed: false, state: 'Playing', ...(mix ? { mix } : {}) }] }, [pcm]).buffer);
    script(null); // A: the fake's own mix of the lane
    // Arm B for one snapshot: it answers B, and the snapshots after it answer A again.
    window.__pinB = () => {
      script({ ...defaultLaneMix(), volume: 0.25 });
      const real = native.snapshot;
      native.snapshot = async (master) => {
        native.snapshot = real;
        const answer = await real.call(native, master);
        script(null);
        return answer;
      };
    };
  }, [BAR]);
  await emit({ events: [laneEvent(1, lane('Empty')), laneEvent(2, lane('Empty')), laneEvent(0, playing())] });
  const clean = await autosaveSettles(3000);
  await page.evaluate(() => window.__pinB());
  await emit({ peaks: [{ lane: 0, start: 0, count: 1, min: [-0.25], max: [0.25] }] }); // its loop may have changed
  const settled = await autosaveSettles(3500);
  const saves = settled - clean;
  console.log('(g)', JSON.stringify({ engineVolume: await applied(0), saves }));
  assert.equal(saves, 2, 'the save that pinned B leaves the jam dirty at A: one more save, then none');

});

/** The looper lanes. */
function lanes(page) {
  return page.locator('.lp-lane');
}
