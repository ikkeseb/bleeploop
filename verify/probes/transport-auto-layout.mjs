/**
 * Drives the rendered command bar and lane through AUTO REC's toggle and its sensitivity (the SENS
 * read-out's popover, its slider driven end to end by key) at seven desktop widths (960..1920 px), in
 * engine mode on the web engine fake (`window.__lfEngineFake` set before the app loads, a reset frame with
 * five EMPTY lanes), and asserts the command-bar height and lane top never move: AUTO must not reflow the
 * stage when switched on/off or set from 1 to 100, and the read-out is live only while AUTO is on.
 * At each width it also holds the bar's own layout: the stacked bar's row gap stays 3 px, the BPM
 * editor opens in the numeral's box (the bar and stage do not move while typing), every bar popover
 * (IN FX's, SENS's) opens below the bar's bottom edge, aligned to its pill, leaving END STOP uncovered,
 * and a disabled tool (Export, the looper empty) shows no hover face.
 * At 1440x900 with a stopped one-bar loop it holds the disabled tone: the BPM steppers (locked), IMPORT
 * (a loop is in), lane 1's core (STOPPED) and a disabled read-out's caret (SENS, AUTO off) fade by tone,
 * never by opacity: each at an effective opacity of 1 in the --faint legend a disabled pill wears, an
 * enabled tool (EXPORT) a brighter one. It saves that tool cluster (the native chrome on) at a
 * device-pixel ratio of 2 as `logs/transport-auto-layout/tools-loop.png`.
 * Screenshots the 1730 px case before and after enabling. Proves layout stability only; it says nothing
 * about auto-record detection itself (the engine's: lf-engine `tests/auto_record.rs`) or anything below
 * the DOM. Run: pnpm probe transport-auto-layout
 */
import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { probe } from '../harness/probe.ts';

const BAR = 2 * 48000; // one 4/4 bar at 120 BPM
const EMPTY = { state: 'Empty', length: 0, armed: false, autoArmed: false, canUndo: false, canReverse: false, reversed: false, stopAt: null, fading: false, retakePass: 0 };

await probe(async ({ browser, open }) => {
  await mkdir('logs/layout', { recursive: true });
  const failures = [];
  for (const width of [960, 1280, 1400, 1500, 1600, 1730, 1920]) {
    const { page } = await open({
      viewport: { width, height: 900 },
      init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)),
    });
    await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });
    await page.evaluate((f) => window.__lf.native.emit(f), {
      seq: 1,
      reset: true,
      settings: [],
      events: [
        ...[0, 1, 2, 3, 4].map((lane) => ({ Lane: { frame: 0, lane, info: EMPTY } })),
        { Transport: { frame: 0, master: 0, bpm: 120, locked: false } },
        { Selected: { frame: 0, lane: 0 } },
      ],
      anchor: { frame: 0, atMs: Date.now(), rate: 48000, grid: 0 },
      meter: { peak: [0, 0], clip: [false, false] },
    });
    await page.evaluate(() => window.__lf.looper.setAutoRecordEnabled(false));
    await page.waitForTimeout(150);
    const bounds = () => page.evaluate(() => ({
      bar: document.querySelector('.cmd').getBoundingClientRect().height,
      lane: document.querySelector('.lp-lane').getBoundingClientRect().top,
    }));
    const before = await bounds();
    if (width === 1730) await page.screenshot({ path: 'logs/layout/auto-off.png' });

    // The stacked bar keeps its 3 px row gap at every width (the narrow media rule sets column-gap only).
    const rowGap = await page.evaluate(() => {
      const bar = document.querySelector('.cmd');
      return bar.classList.contains('cmd--stack') ? getComputedStyle(bar).rowGap : null;
    });
    if (rowGap !== null) assert.equal(rowGap, '3px', `${width}: the stacked bar's row gap`);

    // A disabled tool shows no hover face (Export, nothing to export).
    const exportTool = page.locator('.tool--export');
    assert.equal(await exportTool.isDisabled(), true, 'Export is disabled on an empty looper');
    const eb = await exportTool.boundingBox();
    await page.mouse.move(eb.x + eb.width / 2, eb.y + eb.height / 2);
    await page.waitForTimeout(250);
    const exportFace = await exportTool.evaluate((el) => getComputedStyle(el).backgroundColor);
    await page.mouse.move(1, 1);
    assert.equal(exportFace, 'rgba(0, 0, 0, 0)', `${width}: a disabled tool lit a hover face`);

    // The BPM editor opens in the numeral's box: nothing in the bar or the stage moves while typing.
    const numBox = await page.locator('.transport__bpm-num').boundingBox();
    await page.locator('.transport__bpm-num').click();
    const editor = page.locator('.transport__bpm-input');
    await editor.waitFor();
    await editor.fill('300');
    await page.waitForTimeout(150);
    const editBox = await editor.boundingBox();
    const editing = await bounds();
    console.log(JSON.stringify({ width, rowGap, exportFace, numBox, editBox, editing }));
    await page.keyboard.press('Escape');
    await editor.waitFor({ state: 'detached' });
    assert.ok(Math.abs(editBox.width - numBox.width) < 0.5 && Math.abs(editBox.height - numBox.height) < 0.5,
      `${width}: the BPM editor ${editBox.width}x${editBox.height} is not the numeral's ${numBox.width}x${numBox.height}`);
    assert.ok(Math.abs(editing.bar - before.bar) < 0.5 && Math.abs(editing.lane - before.lane) < 0.5,
      `${width}: typing a tempo moved the bar (${before.bar} -> ${editing.bar}) or the stage`);

    // A bar popover hangs below the bar, aligned to its pill, over nothing in the bar (IN FX on row 1
    // used to hang inside the bar over END STOP on row 2).
    const popoverPlace = (pillName, dialogName) => page.evaluate(([pillName, dialogName]) => {
      const bar = document.querySelector('.cmd').getBoundingClientRect();
      const pill = document.querySelector(`button[aria-label="${pillName}"]`).getBoundingClientRect();
      const pop = document.querySelector(`[role="dialog"][aria-label="${dialogName}"]`);
      const p = pop.getBoundingClientRect();
      const end = document.querySelector('button[aria-label="Stop playing loops at loop end"]');
      const e = end.getBoundingClientRect();
      const hits = document.elementsFromPoint(e.left + e.width / 2, e.top + e.height / 2);
      return {
        barBottom: bar.bottom, top: p.top, aligned: Math.min(Math.abs(p.left - pill.left), Math.abs(p.right - pill.right)),
        endCovered: hits.some((el) => pop.contains(el)),
      };
    }, [pillName, dialogName]);
    await page.getByRole('button', { name: 'Input effects', exact: true }).click();
    await page.getByRole('dialog', { name: 'Input effects', exact: true }).waitFor();
    const infx = await popoverPlace('Input effects', 'Input effects');
    console.log(JSON.stringify({ width, infx }));
    await page.keyboard.press('Escape');
    await page.getByRole('dialog', { name: 'Input effects', exact: true }).waitFor({ state: 'detached' });
    assert.ok(infx.top >= infx.barBottom, `${width}: the IN FX popover's top ${infx.top} is inside the bar (bottom ${infx.barBottom})`);
    assert.ok(infx.aligned < 1, `${width}: the IN FX popover is ${infx.aligned} px off its pill's edge`);
    assert.equal(infx.endCovered, false, `${width}: the IN FX popover covers END STOP`);
    const auto = page.getByRole('button', { name: 'Auto record', exact: true });
    const sens = page.getByRole('button', { name: 'Auto record sensitivity', exact: true });
    assert.equal(await auto.getAttribute('aria-pressed'), 'false');
    assert.equal(await sens.isDisabled(), true, 'SENS is dead while AUTO is off');
    await auto.click();
    assert.equal(await sens.isDisabled(), false, 'SENS is live while AUTO is on');
    await sens.click();
    const slider = page.getByRole('slider', { name: 'Auto record sensitivity', exact: true });
    await slider.waitFor({ state: 'visible' });
    const sensPop = await popoverPlace('Auto record sensitivity', 'Auto record sensitivity');
    assert.ok(sensPop.top >= sensPop.barBottom && sensPop.aligned < 1, `${width}: the SENS popover ${JSON.stringify(sensPop)}`);
    await slider.focus();
    await page.keyboard.press('End');
    assert.equal(await slider.inputValue(), '100');
    await page.waitForTimeout(150);
    const enabled = await bounds();
    if (width === 1730) await page.screenshot({ path: 'logs/layout/auto-on.png' });
    await page.keyboard.press('Home');
    assert.equal(await slider.inputValue(), '1');
    await page.keyboard.press('Escape');
    await slider.waitFor({ state: 'detached' });
    await page.waitForTimeout(150);
    const adjusted = await bounds();
    assert.equal(await auto.getAttribute('aria-pressed'), 'true');
    await auto.click();
    await page.waitForTimeout(150);
    assert.equal(await sens.isDisabled(), true);
    const disabled = await bounds();
    const pass = [enabled, adjusted, disabled].every(value =>
      Math.abs(value.bar - before.bar) < 0.5 && Math.abs(value.lane - before.lane) < 0.5);
    console.log(JSON.stringify({ width, before, enabled, adjusted, disabled, pass }));
    if (!pass) failures.push(width);
    await page.close();
  }
  assert.deepEqual(failures, [], 'AUTO moved the stage at these viewport widths');

  // Disabled by tone, never by opacity (the rule .tgl:disabled and the lane pills state): with a stopped
  // one-bar loop the BPM steppers are locked, IMPORT waits for an empty looper and lane 1's core says play
  // first; SENS is dead while AUTO is off. Each keeps an effective opacity of 1 and the --faint legend.
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 2 });
  const { page } = await open({
    context,
    viewport: { width: 1440, height: 900 },
    init: async (p) => {
      // The native chrome on (host.web.ts served with `available: true`), so the cluster holds all seven
      // tools, as the walk's crop does.
      await p.route('**/src/platform/host.web.ts', async (route) => {
        const response = await route.fetch();
        const body = (await response.text()).replace('available: false', 'available: true');
        await route.fulfill({ response, body });
      });
      await p.addInitScript(() => void (window.__lfEngineFake = true));
    },
  });
  await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });
  await page.evaluate((f) => window.__lf.native.emit(f), {
    seq: 1,
    reset: true,
    settings: [],
    events: [
      { Lane: { frame: 0, lane: 0, info: { ...EMPTY, state: 'Stopped', length: BAR } } },
      ...[1, 2, 3, 4].map((lane) => ({ Lane: { frame: 0, lane, info: EMPTY } })),
      { Transport: { frame: 0, master: BAR, bpm: 120, locked: true } },
      { Selected: { frame: 0, lane: 0 } },
    ],
    anchor: { frame: 0, atMs: Date.now(), rate: 48000, grid: 0 },
    meter: { peak: [0, 0], clip: [false, false] },
  });
  await page.locator('.lp-lane[data-state="stop"] .lp-core:disabled').first().waitFor({ timeout: 5000 });
  await page.locator('.tool--import:disabled').waitFor();
  await page.mouse.move(1, 1);
  await page.waitForTimeout(250); // past every colour transition
  const tone = await page.evaluate(() => {
    const probe = document.createElement('span');
    probe.style.color = 'var(--faint)';
    document.body.append(probe);
    const faint = getComputedStyle(probe).color;
    probe.remove();
    const read = (el, pseudo) => {
      const cs = getComputedStyle(el, pseudo);
      let opacity = Number(cs.opacity) * (pseudo ? Number(getComputedStyle(el).opacity) : 1);
      for (let up = el.parentElement; up; up = up.parentElement) opacity *= Number(getComputedStyle(up).opacity);
      return { disabled: el.disabled, opacity, color: cs.color, fill: cs.backgroundColor };
    };
    const q = (sel) => document.querySelector(sel);
    return {
      faint,
      'BPM minus': read(q('button[aria-label="BPM minus"]')),
      'BPM plus': read(q('button[aria-label="BPM plus"]')),
      IMPORT: read(q('.tool--import')),
      'track 1 core': read(q('.lp-lane .lp-core')),
      'SENS caret': { ...read(q('button[aria-label="Auto record sensitivity"]'), '::after'), disabled: q('button[aria-label="Auto record sensitivity"]').disabled },
      EXPORT: read(q('.tool--export')),
    };
  });
  console.log(JSON.stringify({ tone }));
  // The crop first, so a red run leaves the picture too.
  const tools = await page.locator('.tools').boundingBox();
  await mkdir('logs/transport-auto-layout', { recursive: true });
  await page.screenshot({
    path: 'logs/transport-auto-layout/tools-loop.png',
    clip: { x: tools.x - 6, y: tools.y - 6, width: tools.width + 12, height: tools.height + 12 },
  });
  for (const name of ['BPM minus', 'BPM plus', 'IMPORT', 'track 1 core', 'SENS caret']) {
    const t = tone[name];
    assert.equal(t.disabled, true, `${name} is disabled in this state`);
    assert.equal(t.opacity, 1, `${name}: a disabled control faded by opacity (${t.opacity})`);
    const legend = name === 'SENS caret' ? t.fill : t.color;
    assert.equal(legend, tone.faint, `${name}: the disabled legend ${legend} is not --faint ${tone.faint}`);
  }
  assert.equal(tone.EXPORT.disabled, false, 'EXPORT is live with a loop in');
  assert.notEqual(tone.EXPORT.color, tone.faint, 'an enabled tool reads as the disabled one');
  await context.close();
}, { launch: {} });
