/**
 * Stage view (src/ui/stage/StageView.tsx), the performance layer, in engine mode on the web engine fake
 * (`src/platform/host.web.ts`, the `engine-seam` pattern: an init script sets `window.__lfEngineFake`, the
 * probe scripts the feed through `__lf.native` and reads what the UI sends):
 *
 * - enter/exit: the B key (either case), the `stageView` action (src/app/actions.ts), the command-bar cap
 *   and the EXIT button, and Escape; while open the command bar and the stage are inert, and the normal
 *   looper lane canvas is the same element before and after (nothing remounts);
 * - gesture → command inside the view: Space sends the engine's `RecDub` action, Enter its `PlayStop`, a
 *   digit and a pointer press on a stage lane send `SelectTrack`; the resulting states are scripted back
 *   on the feed;
 * - feed → DOM: every lane's state word against the lane the feed reports, across EMPTY, ARMED with the
 *   count-in numeral and COUNT-IN, REC, PLAY, STOP, MUTED, DUB and an ARMED later take (WAITING FOR
 *   DOWNBEAT); an engine refusal (`Empty`) shows its reason on its lane; the warm-white edge and
 *   aria-current follow the feed's selection; the lane boxes do not move with the states; reduced motion
 *   stops the beat transition;
 * - the view hides the keyboard: a held A lights no key and sends no `NoteOn` inside it (checked against
 *   the same key outside the view), and drum mode's 3 plays no pad inside it but selects lane 3;
 * - the bar counter: — with no loop; on a 2- and a 4-bar loop at 240 BPM (a scripted clock anchor and a
 *   `Beat` a beat), the bar the loop phase is in, stepping one bar at a time across the loop boundary;
 * - legibility at 1280x820, 1920x1080 and 1000x700: the state word's cap height (Geist 'E' ascent) and the
 *   beat bar's height at 1920x1080, and nothing clips or overlaps (word and number inside the pillar, even
 *   for the longest words ENDING / TAKE 12; header values inside their boxes, the bar counter's widest
 *   "32 / 32" too, at the BPM's size; lanes inside the window). No console.error.
 *
 * Cannot see the native engine (the looper state machine, count-in, takes, overdubs and refusals are
 * lf-engine's tests: count_in.rs, first_take.rs, later_arm.rs, overdub_undo_reverse.rs, actions.rs),
 * Tauri IPC, WebView2 or the distance the view is read from: the fake answers no command by itself, so
 * every state the DOM shows was scripted. Screenshots land in logs/stage-view/<viewport>-<scene>.png for
 * the eye. Run: pnpm probe stage-view
 */
import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { probe } from '../harness/probe.ts';

const outDir = 'logs/stage-view';
const viewports = [[1280, 820], [1920, 1080], [1000, 700]];
const RATE = 48000;
const PEAK_FRAMES = 1024; // the engine's waveform bin (`lf-engine/src/overview.rs`)

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
const transport = (master, locked, bpm) => ({ Transport: { frame: 0, master, bpm, locked } });
/** The clock anchor with `frame` rendering now. */
const anchorAt = (frame) => ({ frame, atMs: Date.now(), rate: RATE, grid: 0 });
/** A lane's waveform over a loop of `frames`: a visible wave, a little different per lane. */
const wave = (i, frames) => {
  const count = Math.ceil(frames / PEAK_FRAMES);
  const max = Array.from({ length: count }, (_, b) => 0.05 + 0.4 * Math.abs(Math.sin((b * PEAK_FRAMES) / 9000 + i)));
  return { lane: i, start: 0, count, min: max.map((v) => -v), max };
};

await probe(async ({ open }) => {
  await mkdir(outDir, { recursive: true });
  const { page, consoleErrors } = await open({
    viewport: { width: 1280, height: 820 },
    init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)),
  });
  let seq = 0;
  const emit = (frame) => page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], ...frame });
  const sent = () => page.evaluate(() => window.__lf.native.sent.slice());
  const clearSent = () => page.evaluate(() => void (window.__lf.native.sent.length = 0));
  const sentAtLeast = async (count) => {
    await page.waitForFunction((n) => window.__lf.native.sent.length >= n, count, { timeout: 5000 });
    return sent();
  };
  /** Press `key` with the sent log cleared and return the one command it sent. */
  const pressSends = async (key, expected, what) => {
    await clearSent();
    await page.keyboard.press(key);
    assert.deepEqual(await sentAtLeast(1), [expected], what);
  };

  let vp = '1280x820';
  const shoot = async (scene) => {
    const file = `${outDir}/${vp}-${scene}.png`;
    await page.screenshot({ path: file });
    console.log(`shot ${file}`);
  };
  const isOpen = () => page.evaluate(() => {
    const sv = document.querySelector('.sv');
    return { open: !!sv, cmdInert: document.querySelector('.cmd').inert, stageInert: document.querySelector('main.stage').inert };
  });
  const expectOpen = async (want, how) => {
    await page.waitForFunction((w) => !!document.querySelector('.sv') === w, want);
    const s = await isOpen();
    assert.deepEqual(s, { open: want, cmdInert: want, stageInert: want }, `${how}: stage view ${want ? 'open' : 'closed'}, normal UI inert iff open`);
    console.log(JSON.stringify({ how, ...s }));
  };
  /** Each stage lane's word and selection, beside the word the lane the feed reported implies. */
  const lanes = () => page.evaluate(() => {
    const L = window.__lf.looper;
    const expected = (i) => {
      const s = L.stateOf(i);
      const info = L.trackInfo(i);
      if (info.stopAt !== null) return 'ENDING';
      if (s === 'RECORDING') return info.armed ? 'ARMED' : 'REC';
      if (s === 'OVERDUBBING') return 'DUB';
      if (s === 'PLAYING') return L.mutedOf(i) ? 'MUTED' : 'PLAY';
      if (s === 'STOPPED') return L.mutedOf(i) ? 'MUTED' : 'STOP';
      return 'EMPTY';
    };
    return [...document.querySelectorAll('.sv-lane')].map((el, i) => ({
      lane: i + 1,
      state: L.stateOf(i),
      word: el.querySelector('.sv-word').textContent,
      expected: expected(i),
      selected: el.classList.contains('is-selected'),
      current: el.getAttribute('aria-current') === 'true',
      count: el.querySelector('.sv-count')?.textContent ?? null,
      msg: el.querySelector('.sv-msg__text')?.textContent ?? null,
      cue: !!el.querySelector('.sv-msg.is-cue'),
    }));
  });
  const checkWords = async (scene, sel) => {
    const ls = await lanes();
    console.log(JSON.stringify({ scene, lanes: ls.map(({ lane, state, word, expected, selected }) => ({ lane, state, word, expected, selected })) }));
    for (const l of ls) assert.equal(l.word, l.expected, `${scene}: lane ${l.lane} (${l.state}) reads ${l.word}, want ${l.expected}`);
    assert.equal(await page.evaluate(() => window.__lf.looper.selectedTrack()), sel, `${scene}: the feed selected lane ${sel + 1}`);
    assert.deepEqual(ls.map((l) => l.selected), ls.map((_, i) => i === sel), `${scene}: exactly the selected lane (${sel + 1}) carries the edge`);
    assert.deepEqual(ls.map((l) => l.current), ls.map((_, i) => i === sel), `${scene}: aria-current follows the selection`);
    return ls;
  };
  /** Legibility + no clipping/overlap at the current viewport. */
  const measure = () => page.evaluate(async () => {
    await document.fonts.ready;
    const r = (el) => el.getBoundingClientRect();
    const inside = (a, b, slack = 0.5) => a.left >= b.left - slack && a.right <= b.right + slack && a.top >= b.top - slack && a.bottom <= b.bottom + slack;
    const overlap = (a, b) => a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom;
    const ctx = document.createElement('canvas').getContext('2d');
    const problems = [];
    const words = [];
    for (const [i, lane] of [...document.querySelectorAll('.sv-lane')].entries()) {
      const pillar = lane.querySelector('.sv-pillar');
      const word = lane.querySelector('.sv-word');
      const num = lane.querySelector('.sv-num');
      const cs = getComputedStyle(word);
      ctx.font = `${cs.fontWeight} ${cs.fontSize} ${cs.fontFamily}`;
      const cap = ctx.measureText('E').actualBoundingBoxAscent;
      const pw = r(pillar).width - 2 * parseFloat(getComputedStyle(pillar).borderLeftWidth);
      // The widest words a pillar may have to hold, measured in its own font.
      const widest = Math.max(...['ENDING', 'LISTEN', 'TAKE 12', 'MUTED', 'EMPTY', 'ARMED'].map((w) => ctx.measureText(w).width));
      words.push({ lane: i + 1, fontPx: parseFloat(cs.fontSize), capPx: Math.round(cap * 10) / 10, pillarPx: Math.round(pw), widestPx: Math.round(widest) });
      if (widest > pw - 8) problems.push(`lane ${i + 1}: the widest word (${Math.round(widest)} px) does not fit its pillar (${Math.round(pw)} px)`);
      if (!inside(r(word), r(pillar))) problems.push(`lane ${i + 1}: word clips its pillar`);
      if (overlap(r(word), r(num))) problems.push(`lane ${i + 1}: lane number overlaps the word`);
      if (!inside(r(lane), { left: 0, top: 0, right: innerWidth, bottom: innerHeight })) problems.push(`lane ${i + 1}: outside the window`);
      for (const part of lane.querySelectorAll('.sv-vol__db, .sv-flag, .sv-label')) {
        if (!inside(r(part), r(lane.querySelector('.sv-ind')))) problems.push(`lane ${i + 1}: ${part.className} clips the indicator column`);
      }
      const msg = lane.querySelector('.sv-msg__text');
      if (msg && !inside(r(msg), r(lane.querySelector('.sv-well')))) problems.push(`lane ${i + 1}: well message clips the well`);
    }
    for (const stat of document.querySelectorAll('.sv-stat')) {
      for (const part of stat.children) if (!inside(r(part), r(stat))) problems.push(`header: ${part.className} clips ${stat.className}`);
    }
    const beats = r(document.querySelector('.sv-beats'));
    const exit = r(document.querySelector('.sv-exit'));
    if (!inside(exit, { left: 0, top: 0, right: innerWidth, bottom: innerHeight })) problems.push('EXIT outside the window');
    const beatBoxes = [...document.querySelectorAll('.sv-beat')].map((b) => Math.round(r(b).width));
    // The bar counter's widest reading, "32 / 32", in a hidden copy of its box.
    const barStat = document.querySelector('.sv-stat--bar');
    const widest = barStat.cloneNode(true);
    widest.style.cssText = 'position: absolute; visibility: hidden';
    const widestVal = widest.querySelector('.sv-stat__val');
    widestVal.className = 'sv-stat__val';
    widestVal.innerHTML = '32<span class="sv-unit">/ 32</span>';
    barStat.parentElement.append(widest);
    for (const part of widest.children) if (!inside(r(part), r(widest))) problems.push('header: "32 / 32" clips the bar counter');
    widest.remove();
    const px = (q) => parseFloat(getComputedStyle(document.querySelector(q)).fontSize);
    const barFontPx = px('.sv-stat--bar .sv-stat__val');
    if (barFontPx !== px('.sv-stat--bpm .sv-stat__val')) problems.push(`the bar counter (${barFontPx} px) is smaller than the BPM`);
    return { beatBarPx: Math.round(beats.height), beatBoxes, barFontPx, words, problems };
  });

  // ── Boot: a fresh engine, every lane EMPTY ────────────────────────────────────────────────────────
  await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });
  await emit({
    reset: true,
    settings: [],
    events: [...[0, 1, 2, 3, 4].map((i) => laneEvent(i, lane('Empty'))), transport(0, false, 120), { Selected: { frame: 0, lane: 0 } }],
    anchor: anchorAt(0),
    meter: { peak: 0, clip: false },
  });
  // Nothing may remount under the stage view: remember a normal lane's canvas element.
  await page.evaluate(() => { window.__svCanvas = document.querySelector('.lp-lane canvas'); });

  // ── enter/exit: key, action, button, Escape ───────────────────────────────────────────────────────
  await expectOpen(false, 'at launch');
  await page.keyboard.press('b');
  await expectOpen(true, 'B key opens');
  await page.keyboard.press('b');
  await expectOpen(false, 'B key closes');
  await page.evaluate(() => import('/src/app/actions.ts').then((m) => m.runAction('stageView')));
  await expectOpen(true, 'stageView action opens');
  await page.evaluate(() => import('/src/app/actions.ts').then((m) => m.runAction('stageView')));
  await expectOpen(false, 'stageView action closes');
  await page.getByRole('button', { name: 'Stage view', exact: true }).click();
  await expectOpen(true, 'command-bar cap opens');
  await page.getByRole('button', { name: 'Exit stage view', exact: true }).click();
  await expectOpen(false, 'EXIT button closes');
  await page.keyboard.press('B');
  await expectOpen(true, 'Shift+B opens');
  await page.keyboard.press('Escape');
  await expectOpen(false, 'Escape closes');
  await page.keyboard.press('b');
  await expectOpen(true, 'B key opens again');

  // ── a first take through the transport keys, inside the view ──────────────────────────────────────
  await page.waitForTimeout(200);
  await checkWords('empty', 0);
  assert.equal(await page.locator('.sv-stat--bar .sv-stat__val').textContent(), '—', 'no loop: the bar counter reads —');
  // Fixed sizes: the lane boxes measured now must not move when the states change (checked at 'mixed').
  const boxes = () => page.evaluate(() => [...document.querySelectorAll('.sv-lane')].map((l) =>
    ['.sv-pillar', '.sv-well', '.sv-ind'].map((q) => { const b = l.querySelector(q).getBoundingClientRect(); return [b.x, b.y, b.width, b.height].map(Math.round).join(','); }).join(' ')));
  const emptyBoxes = await boxes();
  await shoot('empty');
  await pressSends('Space', { Action: 'RecDub' }, 'Space inside the view sends the engine action');
  // The engine arms lane 1 behind the count-in (lf-engine count_in.rs) and says so on the feed.
  await emit({
    events: [laneEvent(0, lane('Recording', { armed: true })), transport(0, true, 120), { Beat: { frame: 0, beatInBar: 0, countLeft: 4, clicked: true } }],
    anchor: anchorAt(0),
  });
  await page.waitForFunction(() => document.querySelector('.sv-count') !== null);
  const countIn = await checkWords('count-in', 0);
  assert.match(countIn[0].count ?? '', /^[1-4]$/, 'the count-in numeral shows in the armed lane');
  assert.equal(countIn[0].msg, 'COUNT-IN');
  await shoot('count-in');
  const BAR120 = 2 * RATE;
  await emit({
    events: [laneEvent(0, lane('Recording')), { Beat: { frame: BAR120, beatInBar: 0, countLeft: 0, clicked: true } }],
    anchor: anchorAt(BAR120 + RATE / 2),
    peaks: [{ lane: 0, start: 0, count: 24, min: Array(24).fill(-0.3), max: Array(24).fill(0.3) }],
  });
  await page.waitForTimeout(300);
  await checkWords('recording', 0);
  await shoot('recording');
  await pressSends('Space', { Action: 'RecDub' }, 'Space again sends the engine action');
  // The engine commits the take and plays it (lf-engine first_take.rs).
  await emit({ events: [laneEvent(0, lane('Playing', { length: BAR120, canReverse: true })), transport(BAR120, true, 120)], peaks: [wave(0, BAR120)] });
  await checkWords('first take playing', 0);

  // ── a mixed looper: PLAY, STOP, MUTED, DUB, EMPTY ─────────────────────────────────────────────────
  const MIX = 8 * RATE; // two bars at 60 BPM
  await emit({
    events: [
      transport(MIX, true, 60),
      laneEvent(0, lane('Playing', { length: MIX, canReverse: true, reversed: true })),
      laneEvent(1, lane('Stopped', { length: MIX, canReverse: true })),
      laneEvent(2, lane('Playing', { length: MIX, canReverse: true })),
      laneEvent(3, lane('Overdubbing', { length: MIX })),
      laneEvent(4, lane('Empty')),
    ],
    anchor: anchorAt(RATE),
    peaks: [0, 1, 2, 3].map((i) => wave(i, MIX)),
  });
  await page.evaluate(() => {
    const L = window.__lf.looper;
    L.setVolume(1, 0.5);
    L.setVolume(3, 1.4);
    L.setMute(2, true);
  });
  await pressSends('4', { SelectTrack: 3 }, 'a digit inside the view sends SelectTrack');
  await emit({ events: [{ Selected: { frame: MIX, lane: 3 } }] });
  await page.waitForTimeout(300);
  const mixed = await checkWords('mixed', 3);
  assert.deepEqual(mixed.map((l) => l.word), ['PLAY', 'STOP', 'MUTED', 'DUB', 'EMPTY']);
  assert.equal(mixed[3].selected, true, 'digit 4 moved the edge to lane 4');
  assert.deepEqual(await boxes(), emptyBoxes, 'the pillar, well and indicator boxes did not move between EMPTY and the mixed states');
  // Reduced motion: the view's transitions and the cue's fade-in are off.
  await page.emulateMedia({ reducedMotion: 'reduce' });
  const still = await page.evaluate(() => getComputedStyle(document.querySelector('.sv-beat')).transitionDuration);
  assert.equal(still, '0s', 'reduced motion: no beat transition');
  await page.emulateMedia({ reducedMotion: 'no-preference' });

  const legibility = {};
  for (const [width, height] of viewports) {
    vp = `${width}x${height}`;
    await page.setViewportSize({ width, height });
    await page.waitForTimeout(300);
    const m = await measure();
    legibility[vp] = m;
    console.log(JSON.stringify({ vp, beatBarPx: m.beatBarPx, beatBoxes: m.beatBoxes, barFontPx: m.barFontPx, word: m.words[0], problems: m.problems }));
    assert.deepEqual(m.problems, [], `${vp}: nothing clips or overlaps`);
    await shoot('mixed');
  }
  const big = legibility['1920x1080'];
  assert.ok(big.words.every((w) => w.capPx >= 40), `1920x1080: every state word's cap height >= 40 px (${big.words.map((w) => w.capPx).join(', ')})`);
  assert.ok(big.beatBarPx >= 40, `1920x1080: the beat bar is >= 40 px tall (${big.beatBarPx})`);

  // ── an ARMED later take waiting for the downbeat, then a refused press on its lane ────────────────
  vp = '1280x820';
  await page.setViewportSize({ width: 1280, height: 820 });
  await pressSends('Space', { Action: 'RecDub' }, 'Space on the overdubbing lane 4 sends the engine action');
  await emit({ events: [laneEvent(3, lane('Playing', { length: MIX, canReverse: true, canUndo: true }))] });
  await pressSends('5', { SelectTrack: 4 }, 'digit 5 sends SelectTrack');
  await emit({ events: [{ Selected: { frame: MIX, lane: 4 } }] });
  await pressSends('Space', { Action: 'RecDub' }, 'Space on the EMPTY lane 5 sends the engine action');
  // The engine arms the later take for the next loop boundary (lf-engine later_arm.rs).
  await emit({ events: [laneEvent(4, lane('Recording', { armed: true }))] });
  await page.waitForTimeout(200);
  const armed = await checkWords('armed-waiting', 4);
  assert.equal(armed[4].word, 'ARMED');
  assert.equal(armed[4].msg, 'WAITING FOR DOWNBEAT');
  await shoot('armed-waiting');
  await emit({ events: [laneEvent(4, lane('Empty'))] });
  await pressSends('Enter', { Action: 'PlayStop' }, 'Enter inside the view sends the engine action');
  // The engine refuses PLAY/STOP on an EMPTY lane and names why (lf-engine actions.rs).
  await emit({ events: [{ Refused: { frame: MIX, lane: 4, reason: 'Empty' } }] });
  await page.waitForFunction(() => document.querySelectorAll('.sv-lane')[4].querySelector('.sv-msg.is-cue') !== null);
  const cued = await lanes();
  console.log(JSON.stringify({ scene: 'refusal-cue', msg: cued[4].msg }));
  assert.equal(cued[4].msg, 'nothing to play, record first');
  await page.waitForTimeout(250); // past the cue's fade-in
  await shoot('refusal-cue');

  // ── pointer selection: a press on a lane selects it ───────────────────────────────────────────────
  await clearSent();
  await page.locator('.sv-lane').nth(1).click({ position: { x: 40, y: 40 } });
  assert.deepEqual(await sentAtLeast(1), [{ SelectTrack: 1 }], 'a pointer press on a stage lane sends SelectTrack');
  await emit({ events: [{ Selected: { frame: MIX, lane: 1 } }] });
  await page.waitForFunction(() => window.__lf.looper.selectedTrack() === 1);
  await checkWords('pointer-select', 1);

  // ── close; the normal UI underneath never remounted ───────────────────────────────────────────────
  await page.keyboard.press('Escape');
  await expectOpen(false, 'Escape closes at the end');
  const same = await page.evaluate(() => window.__svCanvas === document.querySelector('.lp-lane canvas') && window.__svCanvas.isConnected);
  assert.equal(same, true, 'the normal lane canvas is the same element after the stage view (no remount)');

  // ── the view hides the keyboard: no computer key plays inside it, and drum mode's 1–4 select ──────
  // Each key is HELD and the keyboard's own lit cell read (its downNotes) with the notes it sent, first
  // outside the view as the control that the key does play there, then inside, where it must not.
  const hold = async (key, litSelector) => {
    await clearSent();
    await page.keyboard.down(key);
    await page.waitForTimeout(250);
    const lit = await page.evaluate((s) => document.querySelector(s) !== null, litSelector);
    await page.keyboard.up(key);
    await page.waitForTimeout(150);
    const commands = await sent();
    return { lit, notes: commands.filter((c) => c.NoteOn).length, commands };
  };
  const outsideA = await hold('a', '.kb__key--down');
  assert.equal(outsideA.lit, true, 'control: outside the view, A lights a key');
  assert.ok(outsideA.notes > 0, 'control: outside the view, A sends NoteOn');
  await page.keyboard.press('b');
  await expectOpen(true, 'B opens for the key checks');
  const insideA = await hold('a', '.kb__key--down');
  assert.equal(insideA.lit, false, 'inside the view, A lights no key');
  assert.equal(insideA.notes, 0, `inside the view, A sends no note (${JSON.stringify(insideA.commands)})`);
  await page.keyboard.press('Escape');
  await expectOpen(false, 'Escape closes');
  await page.evaluate(() => window.__lf.selectSynth(window.__lf.activeSlot(), 'drum'));
  await page.waitForSelector('.kb__pads');
  const outside3 = await hold('3', '.kb__pad--down');
  assert.equal(outside3.lit, true, 'control: outside the view, drum mode 3 plays the Cowbell pad');
  assert.ok(outside3.notes > 0, 'control: outside the view, drum mode 3 sends its pad note');
  assert.ok(!outside3.commands.some((c) => c.SelectTrack !== undefined), 'control: outside the view, drum mode 3 does not select');
  await page.keyboard.press('b');
  await expectOpen(true, 'B opens in drum mode');
  const inside3 = await hold('3', '.kb__pad--down');
  assert.equal(inside3.lit, false, 'inside the view, drum mode 3 plays no pad');
  assert.equal(inside3.notes, 0, 'inside the view, drum mode 3 sends no note');
  assert.deepEqual(inside3.commands.filter((c) => c.SelectTrack !== undefined), [{ SelectTrack: 2 }], 'inside the view, drum mode 3 sends SelectTrack 2');
  await emit({ events: [{ Selected: { frame: MIX, lane: 2 } }] });
  const drumSel = await lanes();
  console.log(JSON.stringify({ scene: 'drum-digit', selected: drumSel.findIndex((l) => l.selected) + 1 }));
  assert.equal(drumSel[2].selected, true, 'inside the view, drum mode 3 selects lane 3');
  await page.keyboard.press('Escape');
  await expectOpen(false, 'Escape closes after the drum check');

  // ── the bar counter: the bar of the loop that plays, across a loop boundary, on a 2- and a 4-bar loop ──
  // A loop at 240 BPM (one bar a second) on a clock anchored now, a `Beat` on the feed at each beat as the
  // engine sends them, sampled every 40 ms for a loop and a half: the counter's reading beside the loop
  // phase read in the same instant. Away from a bar line the reading is the phase's bar; at a bar line it
  // may still show the bar before (it moves on the beat, not per frame).
  const BEAT240 = RATE / 4;
  const barCounter = async (bars) => {
    const master = bars * 4 * BEAT240;
    await emit({
      events: [transport(master, true, 240), laneEvent(0, lane('Playing', { length: master, canReverse: true })),
        ...[1, 2, 3, 4].map((i) => laneEvent(i, lane('Empty')))],
      anchor: anchorAt(0),
      peaks: [wave(0, master)],
    });
    await page.keyboard.press('b');
    await expectOpen(true, `B opens over a ${bars}-bar loop`);
    const samples = await page.evaluate(async ({ bars, beat, seq0 }) => {
      const seen = [];
      const t0 = performance.now();
      window.__lf.native.emit({ seq: seq0, reset: false, events: [], anchor: { frame: 0, atMs: Date.now(), rate: 48000, grid: 0 } });
      let next = 0;
      let seq = seq0;
      const until = t0 + 300 + (bars + 1.5) * 1000;
      while (performance.now() < until) {
        const elapsed = performance.now() - t0;
        // The engine sends a beat as it renders it; the store shows it when it is heard.
        while (next * 250 <= elapsed) {
          window.__lf.native.emit({ seq: ++seq, reset: false, events: [{ Beat: { frame: next * beat, beatInBar: next % 4, countLeft: 0, clicked: false } }] });
          next++;
        }
        if (elapsed > 300) seen.push({ text: document.querySelector('.sv-stat--bar .sv-stat__val').textContent, phase: window.__lf.looper.phaseValue() });
        await new Promise((resolve) => setTimeout(resolve, 40));
      }
      return seen;
    }, { bars, beat: BEAT240, seq0: (seq += 1000) });
    vp = '1280x820';
    await shoot(`bar-counter-${bars}`);
    await page.keyboard.press('Escape');
    await expectOpen(false, 'Escape closes after the bar counter');
    const readings = samples.map(({ text, phase }) => {
      const m = /^(\d+)\s*\/\s*(\d+)$/.exec(text.trim());
      const at = phase * bars;
      return { bar: m ? Number(m[1]) : null, of: m ? Number(m[2]) : null, want: Math.floor(at) + 1, nearLine: at % 1 < 0.15 || at % 1 > 0.9 };
    });
    const sequence = readings.map((x) => x.bar).filter((b, i, all) => i === 0 || b !== all[i - 1]);
    console.log(JSON.stringify({ scene: `bar-counter-${bars}`, samples: readings.length, sequence }));
    assert.ok(readings.every((x) => x.of === bars), `${bars}-bar loop: every reading is "N / ${bars}" (${[...new Set(samples.map((x) => x.text))].join(', ')})`);
    const wrong = readings.filter((x) => !x.nearLine && x.bar !== x.want);
    assert.deepEqual(wrong, [], `${bars}-bar loop: away from a bar line the counter shows the phase's bar`);
    assert.ok(sequence.every((b, i) => i === 0 || b === (sequence[i - 1] % bars) + 1), `${bars}-bar loop: the counter steps one bar at a time and wraps (${sequence.join(' ')})`);
    assert.ok(sequence.some((b, i) => i > 0 && b === 1 && sequence[i - 1] === bars), `${bars}-bar loop: it crossed the loop boundary ${bars} → 1 (${sequence.join(' ')})`);
    assert.deepEqual([...new Set(sequence)].sort((a, b) => a - b), Array.from({ length: bars }, (_, i) => i + 1), `${bars}-bar loop: every bar shows`);
  };
  await barCounter(2);
  await barCounter(4);

  assert.deepEqual(consoleErrors, [], 'no console.error');
  console.log(JSON.stringify({ pass: true, legibility: Object.fromEntries(Object.entries(legibility).map(([k, v]) => [k, { beatBarPx: v.beatBarPx, capPx: v.words.map((w) => w.capPx), fontPx: v.words[0].fontPx }])) }));
});
