/** Stage view (src/ui/stage/StageView.tsx): the performance layer, driven in the web tier. Enters and
 * exits it by the B key, the `stageView` action (src/app/actions.ts) and the command-bar cap / EXIT
 * button, and by Escape; while open, the command bar and the stage are inert and the normal looper lane
 * canvases are the same elements before and after (nothing remounts). Drives real takes through the
 * transport keys INSIDE the view (Space counts in and records on the selected lane, Space again ends the
 * take), then loads a mixed session (lib pattern: contact-sheet.mjs) and checks every lane's state word
 * against the looper's own state across EMPTY, ARMED + the count-in numeral, REC, PLAY, DUB, STOP,
 * MUTED and an ARMED later take waiting for the downbeat; a refused Enter shows its reason on its lane;
 * digit and pointer selection move the warm-white edge. With the view open no computer key plays a note
 * or a drum pad (a held A / 3 lights no key, each checked against a control outside the view), and drum
 * mode's 3 selects lane 3. Legibility, measured at 1280x820, 1920x1080 and
 * 1000x700: the state word's cap height (Geist 'E' ascent) and the beat bar's height, and nothing clips
 * or overlaps (word and number inside the pillar, even for the longest words ENDING / TAKE 12; header
 * values inside their boxes; lanes inside the window). No console.error. Screenshots land in
 * logs/stage-view/<viewport>-<scene>.png for the eye. Sees the rendered DOM and computed styles in
 * Chromium, never WebView2, the engine-mode feed or the distance it is read from.
 * Run: pnpm probe stage-view
 */
import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { probe } from '../harness/probe.ts';

const outDir = 'logs/stage-view';
const viewports = [[1280, 820], [1920, 1080], [1000, 700]];

await probe(async ({ open }) => {
  await mkdir(outDir, { recursive: true });
  const { page, consoleErrors } = await open({ viewport: { width: 1280, height: 820 } });
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
  /** Each stage lane's word and selection, beside the word the looper's own state implies. */
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
  const checkWords = async (scene) => {
    const ls = await lanes();
    console.log(JSON.stringify({ scene, lanes: ls.map(({ lane, state, word, expected, selected }) => ({ lane, state, word, expected, selected })) }));
    for (const l of ls) assert.equal(l.word, l.expected, `${scene}: lane ${l.lane} (${l.state}) reads ${l.word}, want ${l.expected}`);
    const sel = await page.evaluate(() => window.__lf.looper.selectedTrack());
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
    return { beatBarPx: Math.round(beats.height), beatBoxes, words, problems };
  });

  await page.evaluate(() => {
    const L = window.__lf.looper;
    L.init();
    L.setFixedLengthEnabled(false);
    L.setLoopEndStopEnabled(false);
  });
  // Nothing may remount under the stage view: remember a normal lane's canvas element.
  await page.evaluate(() => { window.__svCanvas = document.querySelector('.lp-lane canvas'); });

  // ---- enter/exit: key, action, button, Escape ----
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

  // ---- real takes through the transport keys, inside the view ----
  await page.waitForTimeout(200);
  await checkWords('empty');
  // Fixed sizes: the lane boxes measured now must not move when the states change (checked at 'mixed').
  const boxes = () => page.evaluate(() => [...document.querySelectorAll('.sv-lane')].map((l) =>
    ['.sv-pillar', '.sv-well', '.sv-ind'].map((q) => { const b = l.querySelector(q).getBoundingClientRect(); return [b.x, b.y, b.width, b.height].map(Math.round).join(','); }).join(' ')));
  const emptyBoxes = await boxes();
  await shoot('empty');
  await page.keyboard.press('Space');
  await page.waitForFunction(() => window.__lf.looper.trackInfo(0).armed);
  await page.waitForFunction(() => document.querySelector('.sv-count') !== null);
  const countIn = await checkWords('count-in');
  assert.match(countIn[0].count ?? '', /^[1-4]$/, 'the count-in numeral shows in the armed lane');
  assert.equal(countIn[0].msg, 'COUNT-IN');
  await shoot('count-in');
  await page.waitForFunction(() => window.__lf.looper.stateOf(0) === 'RECORDING' && !window.__lf.looper.trackInfo(0).armed, undefined, { timeout: 10000 });
  await page.waitForTimeout(600);
  await checkWords('recording');
  await shoot('recording');
  await page.keyboard.press('Space');
  await page.waitForFunction(() => window.__lf.looper.stateOf(0) === 'PLAYING', undefined, { timeout: 10000 });
  await checkWords('first take playing');

  // ---- a mixed session: PLAY, STOP, MUTED, DUB, EMPTY ----
  await page.evaluate(async () => {
    const lf = window.__lf;
    const { defaultFxStates } = await import('/src/audio/fx/fx.ts');
    lf.looper.clearAll();
    const bpm = 60, bars = 2;
    const frames = Math.round(lf.engine.ctx.sampleRate * (60 / bpm) * 4 * bars);
    const tracks = [0, 1, 2, 3].map((index) => {
      const pcm = new Float32Array(frames);
      for (let f = 0; f < frames; f++) pcm[f] = 0.4 * Math.sin(f / (40 + index * 9)) * Math.abs(Math.sin(f / 9000));
      return { index, pcm, volume: [1, 0.5, 1, 1.4][index], muted: false, reversed: index === 0,
        state: index === 1 ? 'STOPPED' : 'PLAYING', fx: defaultFxStates() };
    });
    await lf.looper.loadSession({ bpm, bars, masterLengthFrames: frames, tracks });
  });
  await page.waitForTimeout(1000);
  await page.evaluate(() => {
    const L = window.__lf.looper;
    L.setMute(2, true);
    L.recDub(3);
  });
  await page.waitForFunction(() => window.__lf.looper.stateOf(3) === 'OVERDUBBING', undefined, { timeout: 10000 });
  await page.keyboard.press('4'); // a digit selects inside the view too
  await page.waitForTimeout(300);
  const mixed = await checkWords('mixed');
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
    console.log(JSON.stringify({ vp, beatBarPx: m.beatBarPx, beatBoxes: m.beatBoxes, word: m.words[0], problems: m.problems }));
    assert.deepEqual(m.problems, [], `${vp}: nothing clips or overlaps`);
    await shoot('mixed');
  }
  const big = legibility['1920x1080'];
  assert.ok(big.words.every((w) => w.capPx >= 40), `1920x1080: every state word's cap height >= 40 px (${big.words.map((w) => w.capPx).join(', ')})`);
  assert.ok(big.beatBarPx >= 40, `1920x1080: the beat bar is >= 40 px tall (${big.beatBarPx})`);

  // ---- an ARMED later take waiting for the downbeat, then a refused press on its lane ----
  vp = '1280x820';
  await page.setViewportSize({ width: 1280, height: 820 });
  await page.keyboard.press('Space'); // ends the overdub on the selected lane 4
  await page.waitForFunction(() => window.__lf.looper.stateOf(3) === 'PLAYING', undefined, { timeout: 10000 });
  await page.keyboard.press('5');
  await page.keyboard.press('Space'); // arms lane 5 for the next loop boundary
  await page.waitForFunction(() => window.__lf.looper.trackInfo(4).armed);
  await page.waitForTimeout(200);
  const armed = await checkWords('armed-waiting');
  assert.equal(armed[4].word, 'ARMED');
  assert.equal(armed[4].msg, 'WAITING FOR DOWNBEAT');
  await shoot('armed-waiting');
  await page.evaluate(() => window.__lf.looper.stop(4));
  await page.waitForFunction(() => window.__lf.looper.stateOf(4) === 'EMPTY');
  await page.keyboard.press('Enter'); // play/stop on an EMPTY lane is refused, and says why on the lane
  await page.waitForFunction(() => document.querySelectorAll('.sv-lane')[4].querySelector('.sv-msg.is-cue') !== null);
  const cued = await lanes();
  console.log(JSON.stringify({ scene: 'refusal-cue', msg: cued[4].msg }));
  assert.equal(cued[4].msg, 'nothing to play, record first');
  await page.waitForTimeout(250); // past the cue's fade-in
  await shoot('refusal-cue');

  // ---- pointer selection: a press on a lane selects it ----
  await page.locator('.sv-lane').nth(1).click({ position: { x: 40, y: 40 } });
  await page.waitForFunction(() => window.__lf.looper.selectedTrack() === 1);
  await checkWords('pointer-select');

  // ---- close; the normal UI underneath never remounted ----
  await page.keyboard.press('Escape');
  await expectOpen(false, 'Escape closes at the end');
  const same = await page.evaluate(() => window.__svCanvas === document.querySelector('.lp-lane canvas') && window.__svCanvas.isConnected);
  assert.equal(same, true, 'the normal lane canvas is the same element after the stage view (no remount)');

  // ---- the view hides the keyboard: no computer key plays inside it, and drum mode's 1–4 select ----
  // Each key is HELD and the keyboard's own lit cell read (its downNotes), first outside the view as the
  // control that the key does play there, then inside, where it must not.
  const hold = async (key, litSelector) => {
    await page.keyboard.down(key);
    await page.waitForTimeout(250);
    const lit = await page.evaluate((s) => document.querySelector(s) !== null, litSelector);
    await page.keyboard.up(key);
    await page.waitForTimeout(150);
    return lit;
  };
  assert.equal(await hold('a', '.kb__key--down'), true, 'control: outside the view, A plays a note');
  await page.keyboard.press('b');
  await expectOpen(true, 'B opens for the key checks');
  assert.equal(await hold('a', '.kb__key--down'), false, 'inside the view, A plays no note');
  await page.keyboard.press('Escape');
  await expectOpen(false, 'Escape closes');
  await page.evaluate(() => window.__lf.selectSynth(window.__lf.activeSlot(), 'drum'));
  await page.waitForSelector('.kb__pads');
  const selBefore = await page.evaluate(() => window.__lf.looper.selectedTrack());
  assert.equal(await hold('3', '.kb__pad--down'), true, 'control: outside the view, drum mode 3 plays the Cowbell pad');
  assert.equal(await page.evaluate(() => window.__lf.looper.selectedTrack()), selBefore, 'control: outside the view, drum mode 3 does not select');
  await page.keyboard.press('b');
  await expectOpen(true, 'B opens in drum mode');
  assert.equal(await hold('3', '.kb__pad--down'), false, 'inside the view, drum mode 3 plays no pad');
  const drumSel = await lanes();
  console.log(JSON.stringify({ scene: 'drum-digit', selected: drumSel.findIndex((l) => l.selected) + 1 }));
  assert.equal(drumSel[2].selected, true, 'inside the view, drum mode 3 selects lane 3');
  await page.keyboard.press('Escape');
  await expectOpen(false, 'Escape closes after the drum check');

  const errors = consoleErrors.filter((t) => !t.startsWith('[rec-comp] snapshot'));
  assert.deepEqual(errors, [], 'no console.error');
  console.log(JSON.stringify({ pass: true, legibility: Object.fromEntries(Object.entries(legibility).map(([k, v]) => [k, { beatBarPx: v.beatBarPx, capPx: v.words.map((w) => w.capPx), fontPx: v.words[0].fontPx }])) }));
});
