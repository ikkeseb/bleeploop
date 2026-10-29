/**
 * The meter (time signature) in the UI, on the web engine fake (`src/platform/host.web.ts`, the
 * `engine-seam` pattern: an init script sets `window.__lfEngineFake`, the probe scripts the feed through
 * `__lf.native` and reads what the UI sends):
 *
 * - The picker (`src/ui/transport/Transport.tsx`, beside BPM) lists the engine's meters in its order,
 *   shows the meter the feed's `Transport` carries, and a pick sends `SetMeter` and shows nothing new
 *   until the engine's `Transport` echoes it. Locked with the tempo (a loop exists), it is disabled with
 *   the lock's title and `clock.setMeter` sends nothing.
 * - The beat dots are the meter's counted beats (6/8: two) and the lit one follows the engine's
 *   `Beat.beatInBar` with no `% 4`; a 6/8 count-in shows 2 then 1 in the lane's well.
 * - The bar math counts the meter: the loop readout of a 6/8 loop, and the waveform grid of a lane over
 *   one master length that is one 4/4 bar and then two 6/8 bars (the same 96000 frames at 120): the
 *   middle line turns into a bar line, read from the canvas pixels (the grid's cache must not keep 4/4).
 * - The stage view: a one-bar 6/8 loop (two beats) pulses on its beats and its downbeat (the beat check
 *   read a two-beat loop's every forward step as a step back), and the HUD shows two dots and bar 1 / 1.
 *   The beat check alone (`stepLight` on a scripted phase), in loops of four and two beats: a wrap pulses the downbeat, and a
 *   clock correction back over a beat line and on again pulses nothing early and nothing twice.
 * - The kept meter: the meter the engine applied is kept (`lf.meter`), a fresh engine that lacks it gets
 *   a `SetMeter`, its reset frame's 4/4 does not overwrite the kept one, and an engine that has it gets
 *   none, including a known 4/4 after older 6/8 Transports (a lock and an unlock among them) or picks
 *   4/4, 3/4, 4/4 inside the feed's lag. Each Transport is kept; a reset with a known meter sends none
 *   back. A pick a full command ring or a loop lock refused is never kept or sent again on a reset.
 * - The transport keys after a pointer pick in the picker (new value, same value, dismissed): Space and
 *   ArrowDown reach the looper (`src/app/transport-keys.ts`; the picker has no <label>, its name is its
 *   aria-label, so the label case of `transport-focus` does not arise here).
 *
 * Cannot see the native engine (lf-engine `tests/meter.rs` holds the click, the count-in and the bars),
 * the session file (`verify/guards/import.mjs`, the `export-context` and `session-state-roundtrip`
 * probes), or a real device's timing. Screenshots of
 * the command bar land in logs/meter-picker/ for the eye.
 * Run: pnpm probe meter-picker
 */
import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { probe } from '../harness/probe.ts';

const RATE = 48000;
const outDir = 'logs/meter-picker';
const FOUR = { num: 4, den: 4, beats: 4 };
const THREE = { num: 3, den: 4, beats: 3 };
const SIX = { num: 6, den: 8, beats: 2 };

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
const empties = (...is) => is.map((i) => laneEvent(i, lane('Empty')));
const transport = (master, locked, bpm, meter) => ({ Transport: { frame: 0, master, bpm, meter, locked } });
const anchorAt = (frame, rate = RATE) => ({ frame, atMs: Date.now(), rate, grid: 0 });

await probe(async ({ open }) => {
  await mkdir(outDir, { recursive: true });
  const { page, consoleErrors, consumeConsoleErrors } = await open({
    viewport: { width: 1280, height: 820 },
    init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)),
  });
  let seq = 0;
  const emit = (frame) => page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], ...frame });
  const sent = () => page.evaluate(() => window.__lf.native.sent.slice());
  const clearSent = () => page.evaluate(() => void (window.__lf.native.sent.length = 0));
  const settle = () => page.waitForTimeout(120);
  const picker = page.getByRole('combobox', { name: 'Time signature', exact: true });
  const dots = page.locator('.transport__beats .transport__beat');
  const litDot = () => page.evaluate(() => [...document.querySelectorAll('.transport__beats .transport__beat')].findIndex((d) => d.classList.contains('on')));
  const kept = () => page.evaluate(() => localStorage.getItem('lf.meter'));
  const reset = (events, settings = []) =>
    emit({ reset: true, settings, events, anchor: anchorAt(0), meter: { peak: [0, 0], clip: [false, false] } });

  await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });
  await page.evaluate(() => window.__lf.layoutStore.setKeyboardPlacement('hidden'));
  await reset([transport(0, false, 120, FOUR), ...empties(0, 1, 2, 3, 4), { Selected: { frame: 0, lane: 0 } }]);
  await settle();

  // ── The picker: the engine's list, the feed's meter ───────────────────────────────────────────────
  const options = await picker.locator('option').allTextContents();
  console.log('picker options', JSON.stringify(options), 'value', await picker.inputValue(), 'dots', await dots.count());
  assert.deepEqual(options, ['4/4', '3/4', '6/8', '12/8', '5/4'], "the picker lists the engine's meters in its order");
  assert.equal(await picker.inputValue(), '4/4');
  assert.equal(await dots.count(), 4, 'four dots in 4/4');
  assert.ok(!(await sent()).some((c) => 'SetMeter' in c), 'nothing kept: a fresh engine gets no SetMeter');

  // A pick sends SetMeter and waits for the engine's word.
  await clearSent();
  await picker.selectOption('3/4');
  await settle();
  console.log('sent after picking 3/4', JSON.stringify(await sent()), 'value before the echo', await picker.inputValue());
  assert.deepEqual(await sent(), [{ SetMeter: [3, 4] }], 'a pick sends SetMeter');
  assert.equal(await picker.inputValue(), '4/4', 'the picker shows the engine\'s meter until it echoes the pick');
  await emit({ events: [transport(0, false, 120, THREE)] });
  await settle();
  assert.equal(await picker.inputValue(), '3/4', "the engine's Transport sets the picker");
  assert.equal(await dots.count(), 3, 'three dots in 3/4');
  assert.equal(await kept(), '1', 'the applied meter is kept (its index)');
  await page.locator('.cmd').screenshot({ path: `${outDir}/picker-3-4.png` });

  // ── The transport keys after a pointer pick (transport-keys.ts) ──────────────────────────────────
  for (const [name, listKeys] of [
    ['a pointer pick of a new meter', ['ArrowDown', 'Enter']],
    ['a pointer pick of the same meter', ['Enter']],
    ['a dismissed meter list', ['Escape']],
  ]) {
    await page.evaluate(() => document.activeElement?.blur());
    await clearSent();
    await picker.click();
    await page.waitForTimeout(150);
    for (const key of listKeys) await page.keyboard.press(key);
    await page.waitForTimeout(100);
    const picks = (await sent()).filter((c) => 'SetMeter' in c);
    await clearSent();
    await page.keyboard.press('Space');
    await page.keyboard.press('ArrowDown');
    await page.waitForTimeout(150);
    const after = await sent();
    console.log(`after ${name}: SetMeter sent ${JSON.stringify(picks)}; Space + ArrowDown sent`, JSON.stringify(after));
    if (listKeys.length === 2) assert.deepEqual(picks, [{ SetMeter: [6, 8] }], `${name} sent no SetMeter for the next meter`);
    else assert.deepEqual(picks, [], `${name} sent a SetMeter`);
    assert.ok(after.some((c) => c.Action === 'RecDub'), `Space after ${name} did not send REC/DUB`);
    assert.ok(after.some((c) => c.Action === 'NextTrack'), `ArrowDown after ${name} did not send NEXT TRACK`);
    assert.equal(await picker.inputValue(), '3/4', `the keys after ${name} changed the picker`);
  }
  // A keyboard user keeps the picker: an arrow on the focused select picks there, no NEXT TRACK.
  await picker.focus();
  await clearSent();
  await page.keyboard.press('ArrowDown');
  await page.waitForTimeout(100);
  console.log('sent after ArrowDown on the focused picker', JSON.stringify(await sent()));
  assert.ok(await picker.evaluate((el) => document.activeElement === el), 'a keyboard pick lost the picker its focus');
  assert.deepEqual(await sent(), [{ SetMeter: [6, 8] }], 'ArrowDown on the focused picker did not pick the next meter, or reached the transport');
  await page.evaluate(() => document.activeElement?.blur());

  // ── 6/8: the count-in, the dots, the lock ─────────────────────────────────────────────────────────
  await emit({ events: [transport(0, false, 60, SIX)] });
  await settle();
  assert.equal(await picker.inputValue(), '6/8');
  assert.equal(await dots.count(), 2, 'two dots in 6/8: the dotted quarters');
  assert.equal(await kept(), '2');
  // A first take's count-in at a dotted 60: the engine counts 2, 1 (its beats, not its six eighths).
  await emit({
    events: [laneEvent(0, lane('Recording', { armed: true })), transport(0, true, 60, SIX), { Beat: { frame: 0, beatInBar: 0, countLeft: 2, clicked: true } }],
    anchor: anchorAt(0),
  });
  const numeral = page.locator('.lp-lane').nth(0).locator('.lp-lane__count');
  await page.waitForFunction(() => document.querySelector('.lp-lane .lp-lane__count')?.textContent === '2', undefined, { timeout: 3000 });
  console.log('6/8 count-in: numeral', await numeral.textContent(), 'lit dot', await litDot());
  assert.equal(await litDot(), 0, 'the count lights the first dot');
  assert.ok(await picker.isDisabled(), 'the count-in locks the meter');
  assert.equal(await picker.getAttribute('title'), 'Meter locked to the loop. Clear all to change');
  await page.locator('.cmd').screenshot({ path: `${outDir}/six-eight-count-in.png` });
  await emit({ events: [{ Beat: { frame: RATE, beatInBar: 1, countLeft: 1, clicked: true } }], anchor: anchorAt(RATE) });
  await page.waitForFunction(() => document.querySelector('.lp-lane .lp-lane__count')?.textContent === '1', undefined, { timeout: 3000 });
  assert.equal(await litDot(), 1, 'the second beat lights the second dot');
  await clearSent();
  await page.evaluate(() => import('/src/ui/state/audio.ts').then(({ clock }) => clock.setMeter(clock.meters[1])));
  await settle();
  assert.deepEqual(await sent(), [], 'a locked meter sends nothing');

  // A two-bar 6/8 loop at a dotted 60 (192000 frames): the readout counts 6/8 bars.
  const master68 = 2 * 2 * RATE;
  await emit({
    events: [transport(master68, true, 60, SIX), laneEvent(0, lane('Playing', { length: master68, canReverse: true })), { Beat: { frame: 2 * RATE, beatInBar: 0, countLeft: 0, clicked: true } }],
    anchor: anchorAt(2 * RATE),
  });
  await settle();
  const readout = (await page.locator('.transport__loop-v').innerText()).replace(/\s+/g, ' ').trim();
  console.log('6/8 loop readout', readout);
  assert.match(readout, /^2 BARS/, 'a 192000-frame loop at a dotted 60 is two 6/8 bars');
  await page.locator('.cmd').screenshot({ path: `${outDir}/six-eight-loop.png` });

  // ── The waveform grid over one master length in two meters (96000 frames at 120) ──────────────────
  // Lane 3 stays EMPTY: its grid shows with no wave or playhead over it. The three inner lines sit at a
  // quarter, a half and three quarters in both: beat lines in one 4/4 bar, and in two 6/8 bars the
  // middle one is a bar line, drawn brighter.
  const gridLines = () =>
    page.evaluate(() => {
      const c = document.querySelectorAll('.lp-lane canvas')[2];
      const ctx = c.getContext('2d');
      const y = Math.round(c.height * 0.8);
      const row = ctx.getImageData(0, y, c.width, 1).data;
      // The grid's tints are one colour at two alphas over a clear canvas: the alpha tells them apart.
      const lum = (x) => row[x * 4 + 3];
      const at = (f) => {
        const x = Math.round(f * c.width);
        return Math.max(lum(x - 1), lum(x), lum(x + 1));
      };
      return { quarter: at(0.25), half: at(0.5), threeQuarters: at(0.75), between: at(0.375) };
    });
  await emit({ events: [transport(2 * RATE, true, 120, FOUR), laneEvent(0, lane('Playing', { length: 2 * RATE }))] });
  await page.waitForTimeout(250);
  const four = await gridLines();
  await emit({ events: [transport(2 * RATE, true, 120, SIX)] });
  await page.waitForTimeout(250);
  const six = await gridLines();
  console.log('grid alpha, one 4/4 bar', JSON.stringify(four), 'two 6/8 bars', JSON.stringify(six));
  assert.ok(four.quarter > four.between && four.half > four.between, '4/4: the beat lines show');
  assert.ok(Math.abs(four.half - four.quarter) <= 3, '4/4: the middle line is a beat line like its neighbours');
  assert.ok(six.half > six.quarter + 3 && six.half > six.threeQuarters + 3, "6/8: the middle line is a bar line (the grid's cache kept 4/4)");

  // ── The stage view: a one-bar 6/8 loop pulses (two beats a loop) ──────────────────────────────────
  // A dotted 120: one bar is 48000 frames, a second; the clock runs from now.
  await emit({
    events: [transport(RATE, true, 120, SIX), laneEvent(0, lane('Playing', { length: RATE })), { Beat: { frame: 0, beatInBar: 0, countLeft: 0, clicked: true } }],
    anchor: anchorAt(0),
  });
  await page.keyboard.press('b');
  await page.waitForSelector('.sv', { timeout: 3000 });
  const pulses = await page.evaluate(async () => {
    const { light } = await import('/src/ui/stage/visual.ts');
    let beats = 0;
    let bars = 0;
    let lastBeat = light.beatAt;
    let lastBar = light.barAt;
    const until = performance.now() + 2600;
    while (performance.now() < until) {
      await new Promise((r) => requestAnimationFrame(r));
      if (light.beatAt !== lastBeat) beats++;
      if (light.barAt !== lastBar) bars++;
      lastBeat = light.beatAt;
      lastBar = light.barAt;
    }
    return { beats, bars };
  });
  const hud = await page.evaluate(() => ({
    dots: document.querySelectorAll('.sv-dots .sv-dot').length,
    bar: document.querySelector('.sv-bar')?.textContent.replace(/\s+/g, ' ').trim() ?? null,
  }));
  console.log('stage, one-bar 6/8 loop over 2.6 s: pulses', JSON.stringify(pulses), 'HUD', JSON.stringify(hud));
  assert.ok(pulses.beats >= 4, `a one-bar 6/8 loop pulses on its beats (${pulses.beats} in 2.6 s, about 5 due)`);
  assert.ok(pulses.bars >= 2, `and on its downbeats (${pulses.bars} in 2.6 s, about 2 due)`);
  assert.deepEqual(hud, { dots: 2, bar: '1 / 1' }, 'the stage HUD shows the 6/8 dots and bar');
  await page.keyboard.press('Escape');
  await page.waitForSelector('.sv', { state: 'detached', timeout: 3000 });

  // The beat check itself, frame by frame (`stepLight`, the stage closed, the loop's phase scripted): a
  // loop wrapping, and a clock correction that steps the phase back over a beat line, then on again,
  // pulses nothing early and nothing twice. One loop is one bar, so beat 0 is the bar's pulse.
  const sequences = [
    { name: 'four beats, a wrap', n: 4, phases: [0.6, 0.74, 0.76, 0.99, 0.01, 0.2, 0.26], want: [3, 0, 1] },
    { name: 'four beats, a correction back over beat 2 and on', n: 4, phases: [0.1, 0.26, 0.24, 0.245, 0.25, 0.3, 0.5], want: [1, 2] },
    { name: 'four beats, a correction back over the downbeat', n: 4, phases: [0.9, 0.01, 0.99, 0.995, 0.005, 0.1, 0.25], want: [0, 1] },
    { name: 'two beats, a wrap', n: 2, phases: [0.2, 0.49, 0.51, 0.99, 0.02, 0.4, 0.6], want: [1, 0, 1] },
    { name: 'two beats, a correction back over beat 2 and on', n: 2, phases: [0.3, 0.51, 0.49, 0.495, 0.5, 0.9, 0.02], want: [1, 0] },
    { name: 'two beats, a correction back over the downbeat', n: 2, phases: [0.9, 0.01, 0.99, 0.995, 0.005, 0.3, 0.55], want: [0, 1] },
  ];
  const crossed = await page.evaluate(async (sequences) => {
    const { light, resetLight, stepLight } = await import('/src/ui/stage/visual.ts');
    const { feed } = await import('/src/ui/stage/stage-feed.ts');
    const { looper } = await import('/src/ui/state/audio.ts');
    const kept = { phaseValue: looper.phaseValue, masterFramesValue: looper.masterFramesValue,
      moving: feed.moving, beatsPerLoop: feed.beatsPerLoop, beatsPerBar: feed.beatsPerBar };
    let phase = 0;
    try {
      looper.phaseValue = () => phase;
      looper.masterFramesValue = () => 48000;
      feed.moving = true;
      return sequences.map(({ n, phases }) => {
        feed.beatsPerLoop = n;
        feed.beatsPerBar = n;
        resetLight();
        const out = [];
        phases.forEach((p, k) => {
          phase = p;
          const now = 1e6 + 16 * k;
          stepLight(now);
          if (light.beatAt === now) out.push(light.beatBar ? 'bar' : 'beat');
        });
        return out;
      });
    } finally {
      Object.assign(looper, { phaseValue: kept.phaseValue, masterFramesValue: kept.masterFramesValue });
      Object.assign(feed, { moving: kept.moving, beatsPerLoop: kept.beatsPerLoop, beatsPerBar: kept.beatsPerBar });
    }
  }, sequences);
  sequences.forEach((seq, i) => assert.deepEqual(crossed[i], seq.want.map((b) => (b === 0 ? 'bar' : 'beat')),
    `the beat check, ${seq.name}: pulses ${JSON.stringify(crossed[i])}`));

  // ── The kept meter across engines ─────────────────────────────────────────────────────────────────
  // A fresh engine (no SetMeter in its settings, 4/4 on its Transport) gets the kept 6/8, and its 4/4
  // does not overwrite the kept one.
  await clearSent();
  await reset([transport(0, false, 120, FOUR), ...empties(0, 1, 2, 3, 4)]);
  await settle();
  console.log('sent to a fresh engine', JSON.stringify((await sent()).filter((c) => 'SetMeter' in c)), 'kept', await kept());
  assert.ok((await sent()).some((c) => c.SetMeter?.[0] === 6 && c.SetMeter[1] === 8), 'a fresh engine gets the kept meter');
  assert.equal(await kept(), '2', "the fresh engine's 4/4 did not overwrite the kept 6/8");
  await emit({ events: [transport(0, false, 120, SIX)] });
  await settle();
  assert.equal(await picker.inputValue(), '6/8');
  // An engine that has it (a reload over a running engine) gets none.
  await clearSent();
  await reset([transport(0, false, 120, SIX), ...empties(0, 1, 2, 3, 4)], [{ SetMeter: [6, 8] }]);
  await settle();
  assert.ok(!(await sent()).some((c) => 'SetMeter' in c), 'an engine that keeps the meter gets no SetMeter');
  // A full command ring refuses a pick: a reset asks only the kept meter.
  await clearSent();
  await page.evaluate(() => void (window.__lf.native.dropped = 'full'));
  try {
    await picker.selectOption('3/4');
    await settle();
    assert.deepEqual(await sent(), [{ SetMeter: [3, 4] }], 'the refused pick was sent');
    assert.deepEqual(consumeConsoleErrors('[platform] native MIDI dropped input: full'),
      ['[platform] native MIDI dropped input: full'], 'the batch was answered as full');
  } finally {
    await page.evaluate(() => void (window.__lf.native.dropped = null));
  }
  assert.equal(await picker.inputValue(), '6/8', 'a refused pick does not change the picker');
  assert.equal(await kept(), '2', 'a refused pick does not change the kept meter');
  await clearSent();
  await reset([transport(0, false, 120, FOUR), ...empties(0, 1, 2, 3, 4)]);
  await settle();
  const afterFull = (await sent()).filter((c) => 'SetMeter' in c);
  console.log('full command ring: sent after reset', JSON.stringify(afterFull), 'kept', await kept());
  assert.deepEqual(afterFull, [{ SetMeter: [6, 8] }], 'the reset sends the kept meter, never the refused pick');
  assert.equal(await kept(), '2', 'the reset keeps 6/8');
  await emit({ events: [transport(0, false, 120, SIX)] });
  await settle();

  // A queued Transport predates the pick; a reset loses its echo but the host keeps the applied 4/4.
  await picker.selectOption('4/4');
  await settle();
  await emit({ events: [transport(0, false, 121, SIX)] });
  await settle();
  assert.equal(await kept(), '2', 'the queued Transport keeps the applied 6/8');
  await clearSent();
  await reset([transport(0, false, 121, FOUR), ...empties(0, 1, 2, 3, 4)], [{ SetMeter: [4, 4] }]);
  await settle();
  console.log('sent after a stale Transport and reset', JSON.stringify((await sent()).filter((c) => 'SetMeter' in c)), 'kept', await kept());
  assert.ok(!(await sent()).some((c) => 'SetMeter' in c), 'a reset with known 4/4 sends no SetMeter after a stale Transport');
  assert.equal(await kept(), '0', "the reset's Transport keeps the applied 4/4");
  // Older frames still queued: a take locked the tempo in 6/8 and a clear unlocked it, then the 4/4 pick.
  await emit({ events: [transport(0, false, 121, SIX)] });
  await settle();
  await picker.selectOption('4/4');
  await settle();
  await emit({ events: [transport(0, true, 121, SIX)] });
  await emit({ events: [transport(0, false, 121, SIX)] });
  await settle();
  await clearSent();
  await reset([transport(0, false, 121, FOUR), ...empties(0, 1, 2, 3, 4)], [{ SetMeter: [4, 4] }]);
  await settle();
  assert.ok(!(await sent()).some((c) => 'SetMeter' in c), 'an older lock and unlock before a reset send no SetMeter over the 4/4 pick');
  assert.equal(await kept(), '0', 'the 4/4 pick is kept across an older lock and unlock and a reset');

  // A-B-A inside the feed's lag: every echo is kept, and the host's known 4/4 needs no setter.
  await emit({ events: [transport(0, false, 121, SIX)] });
  await settle();
  await clearSent();
  for (const pick of ['4/4', '3/4', '4/4']) {
    await picker.selectOption(pick);
    await settle();
  }
  assert.deepEqual(await sent(), [{ SetMeter: [4, 4] }, { SetMeter: [3, 4] }, { SetMeter: [4, 4] }], 'A-B-A: all three picks were sent before their echoes');
  assert.equal(await kept(), '2', 'A-B-A: only the applied 6/8 is kept before the echoes');
  for (const [meter, index] of [[FOUR, '0'], [THREE, '1'], [FOUR, '0']]) {
    await emit({ events: [transport(0, false, 121, meter)] });
    await settle();
    assert.equal(await kept(), index, 'A-B-A: each Transport keeps its applied meter');
  }
  await clearSent();
  await reset([transport(0, false, 121, FOUR), ...empties(0, 1, 2, 3, 4)], [{ SetMeter: [4, 4] }]);
  await settle();
  const afterAba = (await sent()).filter((c) => 'SetMeter' in c);
  console.log('A-B-A: sent after reset', JSON.stringify(afterAba), 'kept', await kept());
  assert.deepEqual(afterAba, [], 'A-B-A: the known 4/4 gets no SetMeter on reset');
  assert.equal(await kept(), '0', 'A-B-A: the applied 4/4 stays kept');
  assert.equal(await picker.inputValue(), '4/4', 'A-B-A: the picker shows the applied 4/4');

  // A pick the engine never takes, since a loop locked the tempo first: it is never kept.
  await picker.selectOption('3/4');
  await settle();
  await emit({ events: [transport(0, true, 121, FOUR)] });
  await settle();
  assert.equal(await kept(), '0', 'a pick a lock refused leaves the applied meter kept');
  await clearSent();
  await reset([transport(0, true, 121, FOUR), ...empties(0, 1, 2, 3, 4)], [{ SetMeter: [4, 4] }]);
  await settle();
  assert.ok(!(await sent()).some((c) => 'SetMeter' in c), 'a reset sends no SetMeter after a lock refused the pick');
  assert.equal(await kept(), '0', 'a pick a lock refused is not kept for the next launch');
  // An engine whose loop locked a 3/4 import: that is the meter kept.
  await emit({ events: [transport(3 * 72000, true, 120, THREE)] });
  await settle();
  assert.equal(await kept(), '1', "the engine's applied meter is kept, whoever set it");

  assert.deepEqual(consoleErrors, [], 'no console errors');
});
