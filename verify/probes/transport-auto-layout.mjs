/**
 * Drives the rendered command bar and lane through AUTO REC's toggle and its sensitivity (carried on the
 * AUTO REC key, `ValueKey.tsx`, stepped end to end by key) at seven desktop widths (960..1920 px), in
 * engine mode on the web engine fake (`window.__lfEngineFake` set before the app loads, a reset frame with
 * five EMPTY lanes), and asserts the command-bar height and lane top never move: AUTO must not reflow the
 * stage when switched on/off or set from 1 to 100, and its value is live only while AUTO is on, its scale
 * window showing while it changes and gone after.
 * At each width it also holds the bar's own layout: the stacked bar's row gap stays 8 px, the BPM editor
 * opens in the numeral's box (the bar and stage do not move while typing), and a disabled tool (Export,
 * the looper empty) shows no hover face.
 * At 1000x700 and 1440x900, with every input send on (a reset frame from an engine that remembers them), the
 * IN FX popover's sliders are real controls: each at least 160 px long (side by side, the sends left each
 * about 40), and the popover's bottom edge inside the window; each input effect's popover (or the folded
 * IN FX one) hangs from its anchor (18 px inside its trigger, on the left edge or, flipped, the right),
 * both its edges inside the window, under its plate and over none of the slot row's controls.
 * At 1440x900 with a stopped one-bar loop it holds the disabled tone: TAP (the tempo is locked), IMPORT
 * (a loop is in), lane 1's core (STOPPED) and AUTO REC's value (locked with the tempo) fade by tone,
 * never by opacity: each at an effective opacity of 1 in the --faint legend a disabled key wears, an
 * enabled tool (EXPORT) a brighter one. It saves that tool cluster (the native chrome on) at a
 * device-pixel ratio of 2 as `logs/transport-auto-layout/tools-loop.png`.
 * Then the Audio Settings gear (the native chrome on: host.web.ts served with `available: true`), at 1440x900
 * over a stopped one-bar loop, at a device-pixel ratio of 1 and of 2: its rendered bitmap matches itself
 * turned by one tooth (60 degrees, resampled) and by three (180 degrees, the pixel grid's own turn, no
 * resampling), a ring through its tooth tips crosses six evenly spaced teeth, its hub is open, and its ink
 * box sits in the button's centre within 0.5 px. Writes logs/transport-auto-layout/
 * tools-<dpr>x.png (the tool cluster with 6 px around it) and gear-<dpr>x.png (32 px around the gear).
 * Screenshots the 1730 px case before and after enabling. Proves layout, tone and the gear's raster only
 * (in Chromium, not WebView2 on the rig's display scale); it says nothing
 * about auto-record detection itself (the engine's: lf-engine `tests/auto_record.rs`) or anything below
 * the DOM. Run: pnpm probe transport-auto-layout
 */
import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { probe } from '../harness/probe.ts';

const BAR = 2 * 48000; // one 4/4 bar at 120 BPM
const EMPTY = { state: 'Empty', length: 0, armed: false, autoArmed: false, canUndo: false, canReverse: false, reversed: false, stopAt: null, fading: false, retakePass: 0 };

/** A rendered icon's ink box (device pixels, sub-pixel edges) and how far the icon is from its own turn:
 * the summed |coverage difference| between the bitmap and itself turned by `deg` about the ink box's
 * centre (bilinear), over the summed coverage. Also the most ink on a pixel within 1 device px of that
 * centre (`hub`), and the ink runs met going round a ring at `ring` of the ink box's half-width (each
 * run's centre angle, degrees). Coverage is read from the channel where the icon's colour and the
 * background differ most. */
const inkOf = (page, png, { bg, fg, turns, ring }) => page.evaluate(async ({ b64, bg, fg, turns, ring }) => {
  const img = new Image();
  img.src = `data:image/png;base64,${b64}`;
  await img.decode();
  const W = img.width, H = img.height, canvas = document.createElement('canvas');
  canvas.width = W; canvas.height = H;
  const g = canvas.getContext('2d');
  g.drawImage(img, 0, 0);
  const px = g.getImageData(0, 0, W, H).data;
  const ch = [0, 1, 2].reduce((a, c) => (Math.abs(fg[c] - bg[c]) > Math.abs(fg[a] - bg[a]) ? c : a), 0);
  const cov = new Float64Array(W * H);
  for (let i = 0; i < W * H; i++) cov[i] = Math.min(1, Math.max(0, (px[i * 4 + ch] - bg[ch]) / (fg[ch] - bg[ch])));
  const at = (x, y) => (x < 0 || y < 0 || x >= W || y >= H ? 0 : cov[y * W + x]);
  const sample = (x, y) => {
    x -= 0.5; y -= 0.5;
    const x0 = Math.floor(x), y0 = Math.floor(y), fx = x - x0, fy = y - y0;
    return at(x0, y0) * (1 - fx) * (1 - fy) + at(x0 + 1, y0) * fx * (1 - fy) + at(x0, y0 + 1) * (1 - fx) * fy + at(x0 + 1, y0 + 1) * fx * fy;
  };
  // The ink box from the row and column coverage maxima, its edges sub-pixel (a column at half coverage
  // starts half a pixel in).
  const edges = (profile) => {
    const first = profile.findIndex((v) => v > 0.05), last = profile.findLastIndex((v) => v > 0.05);
    return [first + 1 - profile[first], last + profile[last]];
  };
  const cols = Array.from({ length: W }, (_, x) => Math.max(...Array.from({ length: H }, (_, y) => at(x, y))));
  const rows = Array.from({ length: H }, (_, y) => Math.max(...Array.from({ length: W }, (_, x) => at(x, y))));
  const [left, right] = edges(cols), [top, bottom] = edges(rows);
  const cx = (left + right) / 2, cy = (top + bottom) / 2;
  const turned = (deg) => {
    const t = (deg * Math.PI) / 180, co = Math.cos(t), si = Math.sin(t);
    let err = 0, ink = 0;
    for (let y = 0; y < H; y++) for (let x = 0; x < W; x++) {
      const dx = x + 0.5 - cx, dy = y + 0.5 - cy;
      err += Math.abs(at(x, y) - sample(cx + dx * co - dy * si, cy + dx * si + dy * co));
      ink += at(x, y);
    }
    return +(err / ink).toFixed(3);
  };
  let hub = 0;
  for (let y = 0; y < H; y++) for (let x = 0; x < W; x++) {
    if (Math.hypot(x + 0.5 - cx, y + 0.5 - cy) <= 1) hub = Math.max(hub, at(x, y));
  }
  const r = (ring * (right - left)) / 2, STEPS = 720;
  const inked = Array.from({ length: STEPS }, (_, i) => {
    const t = (2 * Math.PI * i) / STEPS;
    return sample(cx + r * Math.sin(t), cy - r * Math.cos(t)) > 0.5;
  });
  // Runs of ink round the ring (clockwise from 12 o'clock), started at a gap so none wraps.
  const start = inked.indexOf(false), runs = [];
  if (start >= 0) {
    let from = -1;
    for (let k = 1; k <= STEPS; k++) {
      const i = (start + k) % STEPS;
      if (inked[i] && from < 0) from = start + k;
      if (!inked[i] && from >= 0) { runs.push(((((from + start + k - 1) / 2) * 360) / STEPS) % 360); from = -1; }
    }
  }
  return { box: { left, top, right, bottom }, centre: [cx, cy], err: Object.fromEntries(turns.map((d) => [d, turned(d)])), hub,
    teeth: runs.map((a) => +a.toFixed(1)) };
}, { b64: png.toString('base64'), bg, fg, turns, ring });

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

    // The stacked bar keeps its 8 px row gap at every width (transport.css `.cmd--stack`, narrow.html's).
    const rowGap = await page.evaluate(() => {
      const bar = document.querySelector('.cmd');
      return bar.classList.contains('cmd--stack') ? getComputedStyle(bar).rowGap : null;
    });
    if (rowGap !== null) assert.equal(rowGap, '8px', `${width}: the stacked bar's row gap`);

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

    // AUTO REC carries its sensitivity on the key (`ValueKey.tsx`): dead while AUTO is off, stepped by key
    // while it is on (PageUp/PageDown ten at a time, the arrows one), its scale window over the slot row.
    const auto = page.getByRole('button', { name: 'Auto record', exact: true });
    const sens = () => page.evaluate(() => window.__lf.looper.autoRecordSensitivity());
    assert.equal(await auto.getAttribute('aria-pressed'), 'false');
    const sensBefore = await sens();
    await auto.press('ArrowUp');
    assert.equal(await sens(), sensBefore, 'the sensitivity is dead while AUTO is off');
    await auto.click();
    assert.equal(await auto.getAttribute('aria-pressed'), 'true');
    for (let k = 0; k < 11; k++) await auto.press('PageUp');
    assert.equal(await sens(), 100);
    assert.match(await auto.locator('.k-key__v').textContent(), /^100$/, 'the key shows the sensitivity');
    assert.ok(await auto.locator('.transport__scale').isVisible(), 'the scale window shows while it changes');
    await page.waitForTimeout(150);
    const enabled = await bounds();
    if (width === 1730) await page.screenshot({ path: 'logs/layout/auto-on.png' });
    for (let k = 0; k < 11; k++) await auto.press('PageDown');
    assert.equal(await sens(), 1);
    await page.waitForTimeout(900);
    assert.equal(await auto.locator('.transport__scale').count(), 0, 'the scale window goes when the change ends');
    const adjusted = await bounds();
    await auto.click();
    await page.waitForTimeout(150);
    assert.equal(await auto.getAttribute('aria-pressed'), 'false');
    const disabled = await bounds();
    const pass = [enabled, adjusted, disabled].every(value =>
      Math.abs(value.bar - before.bar) < 0.5 && Math.abs(value.lane - before.lane) < 0.5);
    console.log(JSON.stringify({ width, before, enabled, adjusted, disabled, pass }));
    if (!pass) failures.push(width);
    await page.close();
  }
  assert.deepEqual(failures, [], 'AUTO moved the stage at these viewport widths');

  // The input effects' settings with every send on: each slider a real control, each popover inside the window.
  const MIN_SLIDER = 160;
  for (const [width, height] of [[1000, 700], [1440, 900]]) {
    const { page } = await open({ viewport: { width, height }, init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)) });
    await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });
    await page.evaluate((f) => window.__lf.native.emit(f), {
      seq: 1,
      reset: true,
      settings: ['echo', 'reverb', 'ring', 'ott'].map((id) => ({ SetInputSend: [id, true] })),
      events: [
        ...[0, 1, 2, 3, 4].map((lane) => ({ Lane: { frame: 0, lane, info: EMPTY } })),
        { Transport: { frame: 0, master: 0, bpm: 120, locked: false } },
        { Selected: { frame: 0, lane: 0 } },
      ],
      anchor: { frame: 0, atMs: Date.now(), rate: 48000, grid: 0 },
      meter: { peak: [0, 0], clip: [false, false] },
    });
    // Narrow, the four fold into one IN FX plate whose popover lists them; wide, each is a plate in slot A's
    // line with its own settings popover: every popover is read, and each must hold inside the window.
    const folded = (await page.getByRole('button', { name: 'Input effects', exact: true }).count()) > 0;
    const opens = folded
      ? [['Input effects', 'Input effects']]
      : ['Echo', 'Reverb', 'Ring mod', 'OTT'].map((l) => [`Settings for input ${l}`, `Input ${l.toLowerCase()} settings`]);
    const sends = { on: [], sliders: [], bottom: 0, panel: null, placed: [] };
    for (const [trigger, dialog] of opens) {
      await page.getByRole('button', { name: trigger, exact: true }).click();
      await page.getByRole('dialog', { name: dialog, exact: true }).waitFor();
      const read = await page.evaluate((trigger) => {
        const pop = document.querySelector('.infx-pop');
        // Where the popover hangs (slot.css `.slot-pop`): under its plate, its left edge 18 px short of the
        // trigger's right edge, or flipped near the window's right edge, its right edge 18 px past the
        // trigger's left edge; both its edges inside the window, under its plate and covering none of the
        // slot row's controls (the plates, LIVE, the meters and volumes the player is looking at).
        const t = document.querySelector(`[aria-label="${trigger}"]`), plate = t.closest('.plate');
        const p = pop.getBoundingClientRect(), tr = t.getBoundingClientRect(), pl = plate.getBoundingClientRect();
        const over = (r) => p.right > r.left + 0.5 && r.right > p.left + 0.5 && p.bottom > r.top + 0.5 && r.bottom > p.top + 0.5;
        return {
          on: [...document.querySelectorAll('.plate__lamp[aria-label^="Input "]')].map((b) => b.getAttribute('aria-pressed')),
          sliders: [...pop.querySelectorAll('.fxp-param__range')].map((r) => [r.getAttribute('aria-label'), r.getBoundingClientRect().width]),
          bottom: pop.getBoundingClientRect().bottom,
          panel: (({ width, height }) => ({ width, height }))(pop.querySelector('.infx').getBoundingClientRect()),
          placed: {
            trigger,
            aligned: Math.min(Math.abs(p.left - (tr.right - 18)), Math.abs(p.right - (tr.left + 18))),
            left: p.left,
            right: p.right,
            gap: p.top - pl.bottom,
            covers: [...document.querySelectorAll('.slot__line .plate, .slot__line button, .slot__line select, .slot__line input, .slot__vol, .slot__meter')]
              .filter((el) => !pop.contains(el) && el.getBoundingClientRect().width > 0 && over(el.getBoundingClientRect()))
              .map((el) => el.getAttribute('aria-label') ?? el.className),
          },
        };
      }, trigger);
      sends.on = read.on;
      sends.sliders.push(...read.sliders);
      sends.bottom = Math.max(sends.bottom, read.bottom);
      sends.panel = read.panel;
      sends.placed.push(read.placed);
      await page.keyboard.press('Escape');
    }
    console.log(JSON.stringify({ infxSends: `${width}x${height}`, ...sends }));
    await page.close();
    assert.deepEqual(sends.on, ['true', 'true', 'true', 'true'], `${width}x${height}: not every send shows on`);
    assert.equal(sends.sliders.length, 9, `${width}x${height}: the sends' sliders`);
    for (const [name, w] of sends.sliders) assert.ok(w >= MIN_SLIDER, `${width}x${height}: the IN FX slider "${name}" is ${w.toFixed(1)} px long (at least ${MIN_SLIDER})`);
    assert.ok(sends.bottom <= height, `${width}x${height}: the IN FX popover's bottom edge ${sends.bottom} is below the window`);
    for (const q of sends.placed) {
      const at = `${width}x${height}: the popover of "${q.trigger}"`;
      assert.ok(q.aligned < 1, `${at} is ${q.aligned.toFixed(1)} px off its anchor (18 px inside its trigger, either edge)`);
      assert.ok(q.left >= 0 && q.right <= width, `${at} spans ${q.left.toFixed(1)}..${q.right.toFixed(1)}, past the window's edge`);
      assert.ok(q.gap >= 0, `${at} starts ${(-q.gap).toFixed(1)} px above its plate's bottom edge`);
      assert.deepEqual(q.covers, [], `${at} covers controls of the slot row`);
    }
  }

  // Disabled by tone, never by opacity (the recipe's .k-key:disabled and the lane pills state): with a
  // stopped one-bar loop TAP is locked, IMPORT waits for an empty looper and lane 1's core says play
  // first; AUTO REC (and its value) is locked with the tempo. Each keeps an effective opacity of 1 and the
  // --faint legend.
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
      TAP: read(q('button[aria-label="Tap tempo"]')),
      IMPORT: read(q('.tool--import')),
      'track 1 core': read(q('.lp-lane .lp-core')),
      'AUTO REC value': { ...read(q('button[aria-label="Auto record"] .k-key__v')), disabled: q('button[aria-label="Auto record"]').disabled },
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
  for (const name of ['TAP', 'IMPORT', 'track 1 core', 'AUTO REC value']) {
    const t = tone[name];
    assert.equal(t.disabled, true, `${name} is disabled in this state`);
    assert.equal(t.opacity, 1, `${name}: a disabled control faded by opacity (${t.opacity})`);
    assert.equal(t.color, tone.faint, `${name}: the disabled legend ${t.color} is not --faint ${tone.faint}`);
  }
  assert.equal(tone.EXPORT.disabled, false, 'EXPORT is live with a loop in');
  assert.notEqual(tone.EXPORT.color, tone.faint, 'an enabled tool reads as the disabled one');
  await context.close();

  // The Audio Settings gear (Lucide's settings icon, the owner's pick) is rotationally symmetric as
  // rendered, and centred in its button. A Feather path once put its teeth between pixels and read lopsided
  // (0.46 off its one-tooth turn at 1x). TURN_LIMIT allows a symmetric drawing's own antialiasing: a
  // diagonal tooth rasterizes unlike an upright one, so the resampled 60-degree turn sits near 0.29 at 1x
  // (0.14 at 2x); the 180-degree turn is exact on the grid and leaves only the rasterizer (0.08 and 0.04).
  // The turns alone pass a filled square or disc, so the drawing is also read as a gear: TOOTH_RING (of the
  // ink box's half-width, which runs across the teeth at 60 degrees: a ring of 9.2 drawing units, against
  // the stroke over the lobes' tips at 9.1 to 10.9) crosses TEETH teeth, each within SPACING degrees of its
  // place one tooth on from the last. The ring reads 6 teeth from 0.92 to 0.96 at both ratios (nearer the
  // root it cuts each lobe's two flanks apart, 0.84 to 0.91 at 1x), the eight-toothed gear it replaced 8,
  // a filled disc or square none. The hub's hole carries at most HUB_INK coverage (the drawn hole is 2.8 px
  // across at 1x: 0; a filled hub reads 1).
  const TEETH = 6, TURN_LIMIT = { [360 / TEETH]: 0.35, 180: 0.15 }, TOOTH_RING = 0.94, SPACING = 5, HUB_INK = 0.5;
  await mkdir('logs/transport-auto-layout', { recursive: true });
  for (const dpr of [1, 2]) {
    const context = await browser.newContext({ deviceScaleFactor: dpr });
    const { page } = await open({
      context,
      viewport: { width: 1440, height: 900 },
      init: async (p) => {
        await p.addInitScript(() => void (window.__lfEngineFake = true));
        await p.route('**/src/platform/host.web.ts*', async (route) => {
          const response = await route.fetch();
          const body = await response.text();
          if (!body.includes('available: false')) throw new Error('Could not enable simulated native chrome');
          await route.fulfill({ response, body: body.replace('available: false', 'available: true') });
        });
      },
    });
    await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });
    await page.evaluate((f) => window.__lf.native.emit(f), {
      seq: 1,
      reset: true,
      settings: [],
      events: [
        ...[0, 1, 2, 3, 4].map((lane) => ({ Lane: { frame: 0, lane, info: lane ? EMPTY : { ...EMPTY, state: 'Stopped', length: BAR, canReverse: true } } })),
        { Transport: { frame: 0, master: BAR, bpm: 120, locked: true } },
        { Selected: { frame: 0, lane: 0 } },
      ],
      anchor: { frame: 0, atMs: Date.now(), rate: 48000, grid: 0 },
      meter: { peak: [0, 0], clip: [false, false] },
    });
    const gear = page.locator('.tool--gear');
    await gear.waitFor();
    // The band's grain and its light from above would read as ink round the icon: the panel is flattened
    // to its one tone for the measurement (the icon's raster is what is measured, not the panel).
    await page.evaluate(() => void (document.querySelector('.cmd').style.background = 'var(--surf-1)'));
    await page.waitForTimeout(250);
    const place = await gear.evaluate((el) => {
      const b = el.getBoundingClientRect(), s = el.querySelector('svg').getBoundingClientRect();
      const t = el.closest('.tools').getBoundingClientRect();
      const rgb = (c) => c.match(/\d+/g).slice(0, 3).map(Number);
      return {
        button: { x: b.x, y: b.y, width: b.width, height: b.height },
        svg: { x: s.x, y: s.y, width: s.width, height: s.height },
        tools: { x: t.x, y: t.y, width: t.width, height: t.height },
        fg: rgb(getComputedStyle(el).color),
        bg: rgb(getComputedStyle(el.closest('.cmd')).backgroundColor),
      };
    });
    // The tool's own box (26 px, the tools sit edge to edge in their grid): a wider clip takes a neighbour's ink.
    const clip = { x: Math.floor(place.button.x), y: Math.floor(place.button.y), width: Math.floor(place.button.width), height: Math.floor(place.button.height) };
    const png = await page.screenshot({ clip });
    const ink = await inkOf(page, png, { bg: place.bg, fg: place.fg, turns: Object.keys(TURN_LIMIT).map(Number), ring: TOOTH_RING });
    // CSS px from the button's layout centre (the bar centres the cluster at a fractional y; the svg paints
    // snapped to the pixel grid, so up to half a pixel of this is the snap).
    const off = [ink.centre[0] / dpr + clip.x - (place.button.x + place.button.width / 2),
      ink.centre[1] / dpr + clip.y - (place.button.y + place.button.height / 2)];
    console.log(JSON.stringify({ gear: dpr, place, ink, off }));
    const t = place.tools;
    await page.screenshot({ path: `logs/transport-auto-layout/tools-${dpr}x.png`, clip: { x: t.x - 6, y: t.y - 6, width: t.width + 12, height: t.height + 12 } });
    const g = place.svg;
    await page.screenshot({ path: `logs/transport-auto-layout/gear-${dpr}x.png`, clip: { x: g.x + g.width / 2 - 16, y: g.y + g.height / 2 - 16, width: 32, height: 32 } });
    await context.close();
    for (const [deg, limit] of Object.entries(TURN_LIMIT)) {
      assert.ok(ink.err[deg] <= limit, `${dpr}x: the gear is ${ink.err[deg]} off itself turned ${deg} degrees (limit ${limit})`);
    }
    assert.equal(ink.teeth.length, TEETH, `${dpr}x: the gear's tooth ring crosses ${ink.teeth.length} teeth (${ink.teeth})`);
    const spacing = ink.teeth.map((a, i) => (ink.teeth[(i + 1) % TEETH] - a + 360) % 360);
    assert.ok(spacing.every((d) => Math.abs(d - 360 / TEETH) <= SPACING), `${dpr}x: the gear's teeth are spaced ${spacing.map((d) => d.toFixed(1))} degrees`);
    assert.ok(ink.hub <= HUB_INK, `${dpr}x: the gear's hub carries ${ink.hub.toFixed(2)} ink at its centre (limit ${HUB_INK})`);
    assert.ok(Math.abs(off[0]) <= 0.5 && Math.abs(off[1]) <= 0.5, `${dpr}x: the gear's ink box is ${off.map((v) => v.toFixed(2))} px off the button's centre`);
  }
}, { launch: {} });
