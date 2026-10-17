/** Rendered controls and source labels at the native minimum (960x600) and a normal desktop size
 * (1400x900), in engine mode on the web engine fake (`src/platform/host.web.ts`, the engine-seam
 * pattern: an init script sets `window.__lfEngineFake`, the probe scripts the feed through `__lf.native`
 * and answers the selection commands the UI sends as the engine would, with a `Selected` frame):
 *
 * - access and canvas identity: with a stopped one-bar loop on the feed, every control and source label
 *   is reachable, and a lane's canvas survives a keyboard-placement/FX-drawer change (never remounts),
 *   across five keyboard placement/FX-drawer combinations; then the drum-pad ribbon (each pad's label on
 *   one line inside its pad, in one scheme: words or a one-letter prefix, never bare initials), the
 *   two-row command-bar cap and the muted-lane readout with the loop present;
 * - the command bar with a playing loop, with idle modes and with every record mode on (CLICK, FIXED and its
 *   bars, RETAKE, AUTO REC, END STOP), and with every mode on before the first take, from 960 to 1920 px
 *   wide (1105: just above the narrow rung): two rows at most, every control inside the bar and under its
 *   own centre point, Tab walking it as drawn (row 1 left to right, then row 2), and FIXED's and FADE's bar
 *   steppers both with their unit or both bare;
 * - the command bar's fit on a fresh page (fonts loaded) equals its fit after an unrelated DOM change, at
 *   1040 to 1280 px (the brand once unfolded on a fresh page and jumped ~99 px at the first take);
 * - at 960x600, 1000x700, 1280x820 (the Tauri default; the bar stacks into two rows at these three) and
 *   1920x1080 the open Help and Audio Settings popovers do not overlap the command bar's rendered box,
 *   stay inside the window and keep at least the right edge's margin on their left (a hint's one-line
 *   width once pushed Audio Settings to the window's left edge), also with a spacer one window tall
 *   planted in the tab panel that shows (Audio Settings' page, Help's panel under its tabs; it stands in
 *   for native-only rows this tier never renders), whose lowest row (stepping into a block taller than
 *   the panel) must then scroll into reach, for Audio Settings also with an error toast up, where neither
 *   the panel nor that row may reach under the toast stack; without the spacer, Help's reference (the
 *   tab it opens on) scrolls not at all; the scripted open (no pointer input) focuses the panel and draws
 *   no outline around it, while a control inside keeps the focus ring;
 * - at 1000x700 and 1440x900 Tab from an open Help or Audio Settings walks its stops and wraps to the
 *   first without leaving the panel, Shift+Tab from the first or the panel lands on the last, and Help's
 *   Manual panel, focused from its tab, draws its ring inside Help's box (sides and top);
 * - at 960x600 (reduced motion) and 1000x700 with lane 1's FX drawer open and its FILTER engaged (its
 *   params grow the drawer past one key row), so the lane stack scrolls:
 *   each ArrowDown and digit sends its command, and the engine's `Selected` brings the selected lane
 *   wholly into the stack's view from the nearer edge and leaves a lane already in view unscrolled
 *   (under reduced motion inside the frame's own dispatch); a pointer press on a half-hidden lane's PLAY
 *   lands (sends `PlayStop`) and scrolls nothing. At 1000x700 (smooth) a second select arriving inside a
 *   reveal's first frames ends in view, and a pointer press on a lane stops a reveal still running where
 *   the press found it.
 *
 * "Unreachable" means clipped below 98% visible OR the element under its own centre point is not itself
 * (something else intercepts the click). `--plugin-source` serves `src/platform/host.web.ts` with the
 * plugin host available (the native chrome) and a plugin in slot A instead of the built-in synths.
 * `--label=<name>` tags screenshots and `logs/layout/<name>.json`, for comparing two runs (e.g.
 * before/after a layout change). Measures the DOM only: no native chrome beyond the stand-in, no
 * WebView2, no engine (every lane state and selection here was scripted).
 * Run: pnpm probe layout-reachability [--label=<name>] [--plugin-source]
 */
import { mkdir, writeFile } from 'node:fs/promises';
import { arg, flag, probe } from '../harness/probe.ts';

const label = arg('label') ?? 'current';
const pluginSource = flag('plugin-source');

await probe(async ({ open }) => {
  await mkdir('logs/layout', { recursive: true });
  const results = [];
  const failures = [];
  // Every page boots on the engine fake; `--plugin-source` also serves the plugin host available.
  const init = async (page) => {
    await page.addInitScript(() => void (window.__lfEngineFake = true));
    if (!pluginSource) return;
    await page.route('**/src/platform/host.web.ts*', async route => {
      const response = await route.fetch();
      const body = await response.text();
      if (!body.includes('available: false')) throw new Error('Could not enable simulated native chrome');
      await route.fulfill({ response, body: body.replace('available: false', 'available: true') });
    });
  };
  const RATE = 48000, BAR = 2 * RATE; // one 4/4 bar at 120 BPM
  const laneInfo = (state, length = 0) => ({ state, length, armed: false, autoArmed: false, canUndo: false,
    canReverse: length > 0, reversed: false, stopAt: null, fading: false, retakePass: 0 });
  let seq = 0;
  const emit = (page, frame) => page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], ...frame });
  /** Wait for the device the app opens, then the engine's reset frame: `states[i]` on lane i over a
   * master of `master` frames, lane 1 selected. */
  const boot = async (page, states, master = BAR, settings = []) => {
    await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });
    const loop = states.some((s) => s !== 'Empty');
    await emit(page, { reset: true, settings, events: [
      { Transport: { frame: 0, master: loop ? master : 0, bpm: 120, locked: loop } },
      ...states.map((state, lane) => ({ Lane: { frame: 0, lane, info: laneInfo(state, state === 'Empty' ? 0 : master) } })),
      { Selected: { frame: 0, lane: 0 } }],
      anchor: { frame: 0, atMs: Date.now(), rate: RATE, grid: 0 }, meter: { peak: [0, 0], clip: [false, false] },
      peaks: states.flatMap((state, lane) => state === 'Empty' ? [] : [{ lane, start: 0, count: 4, min: [-0.2, -0.5, -0.3, -0.1], max: [0.2, 0.5, 0.3, 0.1] }]) });
  };
  for (const [width, height] of [[960, 600], [1400, 900]]) {
    const { page } = await open({ viewport: { width, height }, init });
    if (pluginSource) {
      await page.waitForFunction(() => document.querySelector('[aria-label="Rescan plugins"]')?.disabled === false);
      await page.evaluate(async () => {
      const slots = await import('/src/ui/state/instrument-slots.ts');
      slots.setSlotPlugins([{ id: 'layout-fixture', name: 'Archetype Petrucci', format: 'vst3', path: 'layout-fixture', isEffect: true }, null]);
      if (window.__lf.slotPlugins()[0]?.id !== 'layout-fixture') throw new Error('Restart Vite to avoid duplicate HMR module state');
      });
      await page.getByRole('button', { name: 'Live input for slot A', exact: true, pressed: false }).waitFor();
    }
    // A one-bar loop on lane 1, stopped (the engine's lanes after a load and ■ ALL).
    await boot(page, ['Stopped', 'Empty', 'Empty', 'Empty', 'Empty']);
    await page.locator('.lp-lane').first().and(page.locator('[data-state="stop"]')).waitFor({ timeout: 5000 });
    await page.evaluate(() => { window.__layoutCanvases = [...document.querySelectorAll('.lp-lane canvas')]; });
    for (const [placement, drawer] of [['bottom', false], ['hidden', false], ['top', false], ['bottom', true], ['hidden', true]]) {
      await page.evaluate(value => window.__lf.layoutStore.setKeyboardPlacement(value), placement);
      const fx = page.getByRole('button', { name: 'Track 1 FX', exact: true });
      if ((await fx.getAttribute('aria-pressed') === 'true') !== drawer) await fx.click();
      await page.waitForTimeout(150);
      const measurement = await page.evaluate(() => {
        const bounds = el => {
          const r = el.getBoundingClientRect();
          return { left: r.left, top: r.top, right: r.right, bottom: r.bottom, width: r.width, height: r.height };
        };
        const clipped = [];
        for (const el of document.querySelectorAll('button, input, select, .slot__k, .slot__name, .kb__title, .kb__hint, .kb__key, .kb__pad')) {
          if (getComputedStyle(el).visibility === 'hidden') continue;
          const r = el.getBoundingClientRect();
          if (!r.width || !r.height) continue;
          const visible = { left: Math.max(0, r.left), top: Math.max(0, r.top), right: Math.min(innerWidth, r.right), bottom: Math.min(innerHeight, r.bottom) };
          for (let ancestor = el.parentElement; ancestor; ancestor = ancestor.parentElement) {
            const style = getComputedStyle(ancestor);
            const a = ancestor.getBoundingClientRect();
            if (style.overflowX !== 'visible') { visible.left = Math.max(visible.left, a.left); visible.right = Math.min(visible.right, a.right); }
            if (style.overflowY !== 'visible') { visible.top = Math.max(visible.top, a.top); visible.bottom = Math.min(visible.bottom, a.bottom); }
          }
          const fraction = Math.max(0, visible.right - visible.left) * Math.max(0, visible.bottom - visible.top) / (r.width * r.height);
          if (fraction < 0.98) clipped.push({ name: el.getAttribute('aria-label') ?? el.textContent.trim(), fraction, bounds: bounds(el) });
        }
        return { lanes: [...document.querySelectorAll('.lp-lane')].map(bounds), clipped,
          canvasesPreserved: [...document.querySelectorAll('.lp-lane canvas')].every((el, i) => el === window.__layoutCanvases[i]) };
      });
      const name = `${label}-${width}x${height}-${placement}${drawer ? '-fx' : ''}`;
      await page.evaluate(() => document.activeElement?.blur());
      await page.screenshot({ path: `logs/layout/${name}.png` });
      const unreachable = await page.evaluate(() => {
        const failures = [];
        for (const el of document.querySelectorAll('button, input, select, .slot__k, .slot__name, .kb__title, .kb__hint, .kb__key, .kb__pad')) {
          if (getComputedStyle(el).visibility === 'hidden') continue;
          // Not rendered (display: none and the like) is fine; rendered but squashed to nothing is not.
          if (!el.checkVisibility()) continue;
          const box = el.getBoundingClientRect();
          if (!box.width || !box.height) {
            failures.push({ name: el.getAttribute('aria-label') ?? el.textContent.trim(), fraction: 0 });
            continue;
          }
          el.scrollIntoView({ block: 'nearest', inline: 'nearest' });
          if (!el.disabled) el.focus();
          const r = el.getBoundingClientRect();
          let left = Math.max(0, r.left), right = Math.min(innerWidth, r.right);
          let top = Math.max(0, r.top), bottom = Math.min(innerHeight, r.bottom);
          for (let ancestor = el.parentElement; ancestor; ancestor = ancestor.parentElement) {
            const style = getComputedStyle(ancestor), a = ancestor.getBoundingClientRect();
            if (style.overflowX !== 'visible') { left = Math.max(left, a.left); right = Math.min(right, a.right); }
            if (style.overflowY !== 'visible') { top = Math.max(top, a.top); bottom = Math.min(bottom, a.bottom); }
          }
          const fraction = Math.max(0, right - left) * Math.max(0, bottom - top) / (r.width * r.height);
          const hit = document.elementFromPoint((left + right) / 2, (top + bottom) / 2);
          const interactive = el.matches('button, input, select');
          if (fraction < 0.98 || (interactive && (!hit || !(hit === el || el.contains(hit))))) {
            failures.push({ name: el.getAttribute('aria-label') ?? el.textContent.trim(), fraction });
          }
        }
        document.querySelector('.lp__lanes').scrollTop = 0;
        document.querySelector('.splitstack--v').scrollTop = 0;
        return failures;
      });
      if (unreachable.length || !measurement.canvasesPreserved) failures.push(name);
      results.push({ name, ...measurement, unreachable });
      console.log(JSON.stringify({ name, laneHeight: measurement.lanes[0]?.height,
        clippedCount: measurement.clipped.length, unreachable }));
    }
    // Loop present + drum pads in the bottom ribbon: the command bar must stay within two rows (the loop
    // readout used to push the tool icons onto a third row at 960 px and the ribbon off-screen) and every
    // pad must be inside the viewport. A muted lane must also say so (data-muted + the MUTED word).
    await page.evaluate(() => { window.__lf.layoutStore.setKeyboardPlacement('bottom'); window.__lf.selectSynth(0, 'drum'); window.__lf.looper.setMute(0, true); });
    await page.waitForTimeout(250);
    const drums = await page.evaluate(() => {
      const bar = document.querySelector('.cmd');
      // Rows = distinct vertical centres of the bar's visible children (a row is ~34 px; clusters centre on it).
      // The absolute 1px screen-reader announcement is not a rendered row.
      const rows = new Set([...bar.children].filter(el => !el.classList.contains('cmd__sr')).map(el => el.getBoundingClientRect())
        .filter(r => r.width > 0 && r.height > 0).map(r => Math.round((r.top + r.bottom) / 2 / 14))).size;
      const pads = [...document.querySelectorAll('.kb__pad')].map(el => el.getBoundingClientRect());
      // Each pad's label on one line inside its pad.
      const labels = [...document.querySelectorAll('.kb__pad')].map((pad) => {
        // The text's own boxes (a range), one per line, not the label's block.
        const label = pad.querySelector('.kb__pad-label');
        const range = document.createRange();
        range.selectNodeContents(label);
        const p = pad.getBoundingClientRect(), lines = range.getClientRects(), r = range.getBoundingClientRect();
        const fits = lines.length === 1 && r.left >= p.left + 1 && r.right <= p.right - 1;
        return { text: label.textContent.trim(), fits };
      });
      const lane = document.querySelector('.lp-lane');
      return { padCount: pads.length, padLabels: labels.map(l => l.text), labelsUnfit: labels.filter(l => !l.fits).map(l => l.text), padsOffscreen: pads.filter(r => r.bottom > innerHeight + 0.5 || r.top < 0).length,
        cmdHeight: bar.getBoundingClientRect().height, cmdRows: rows,
        mutedAttr: lane.getAttribute('data-muted'), mutedWord: lane.querySelector('.lp-lane__state').textContent.trim() };
    });
    await page.evaluate(() => { window.__lf.looper.setMute(0, false); window.__lf.selectSynth(0, 'lead'); });
    const drumName = `${label}-${width}x${height}-drums-muted`;
    // One label scheme: words, a variant as a one-letter prefix (C-Hat, E-Snr), never bare initials (CH).
    drums.labelsOffScheme = drums.padLabels.filter(t => !/^(?:[A-Z]-)?[A-Z][a-z]+(?: (?:[A-Z][a-z]+|\d))?$/.test(t));
    const drumOk = drums.padCount === 16 && drums.padsOffscreen === 0 && drums.labelsUnfit.length === 0 && drums.labelsOffScheme.length === 0 && drums.cmdRows <= 2 && drums.mutedAttr === 'true' && drums.mutedWord === 'MUTED';
    if (!drumOk) failures.push(drumName);
    results.push({ name: drumName, ...drums });
    console.log(JSON.stringify({ name: drumName, ...drums, ok: drumOk }));
    await page.close();
  }
  // FADE and IN FX widen row 1, which once put the master volume on a third row at 960 and 1100 px, and
  // every mode on (FIXED's bars beside it) once put row 2 on a third row under 1100 px. With a loop
  // present the bar must stay within two rows and every control in it must be reachable, at the Tauri
  // minimum, the default and the widths between, with the modes idle and all on; all on also before the
  // first take (`empty-modes-on`), where TAP, BPM and FIXED's bars are live and stay in the Tab walk.
  const ALL_MODES = [{ SetMetronome: true }, { SetFixedLength: true }, { SetFixedBars: 4 }, { SetRetake: true },
    { SetAutoRecord: true }, { SetLoopEndStop: true }];
  for (const [width, height, modes] of [[960, 600], [1000, 700], [1050, 700], [1100, 700], [1105, 700], [1110, 700], [1280, 820], [1440, 900], [1600, 900], [1920, 1080]]
    .flatMap(([w, h]) => [[w, h, 'idle'], [w, h, 'modes-on'], [w, h, 'empty-modes-on']])) {
    const { page } = await open({ viewport: { width, height }, init });
    const empty = modes === 'empty-modes-on';
    await boot(page, [empty ? 'Empty' : 'Playing', 'Empty', 'Empty', 'Empty', 'Empty'], 2 * BAR, modes === 'idle' ? [] : ALL_MODES);
    await page.waitForTimeout(250);
    const bar = await page.evaluate(() => {
      const cmd = document.querySelector('.cmd');
      const rows = new Set([...cmd.children].filter(el => !el.classList.contains('cmd__sr')).map(el => el.getBoundingClientRect())
        .filter(r => r.width > 0 && r.height > 0).map(r => Math.round((r.top + r.bottom) / 2 / 14))).size;
      const unreachable = [];
      for (const el of cmd.querySelectorAll('button, input, select')) {
        const r = el.getBoundingClientRect();
        if (!r.width || getComputedStyle(el).visibility === 'hidden') continue;
        const c = cmd.getBoundingClientRect();
        const inside = r.left >= c.left - 0.5 && r.right <= c.right + 0.5 && r.top >= c.top - 0.5 && r.bottom <= c.bottom + 0.5;
        const hit = document.elementFromPoint((r.left + r.right) / 2, (r.top + r.bottom) / 2);
        if (!inside || !hit || !(hit === el || el.contains(hit))) unreachable.push(el.getAttribute('aria-label') ?? el.textContent.trim());
      }
      const on = ['Metronome click', 'Fixed take length', 'Retake', 'Auto record', 'Stop playing loops at loop end']
        .filter((name) => cmd.querySelector(`button[aria-label="${name}"]`)?.getAttribute('aria-pressed') === 'true').length;
      // FIXED's and FADE's bar steppers read alike at every width: both with their unit or both bare
      // (FIXED once dropped "bars" at 1110 px and below while FADE kept "2 BARS").
      const units = [...cmd.querySelectorAll('.transport__bars-unit')].map((el) => el.checkVisibility());
      // Row 2's free width (stacked with the tools): the room a wider mode could take before row 2 wraps.
      const tools = cmd.querySelector('.tools').getBoundingClientRect(), row2 = cmd.querySelector('.transport__modes').getBoundingClientRect();
      const row2Slack = cmd.classList.contains('cmd--stack-tools') ? Math.round(tools.left - row2.right - parseFloat(getComputedStyle(cmd).columnGap)) : null;
      return { rows, cmdHeight: cmd.getBoundingClientRect().height, classes: cmd.className, on, unreachable, units, row2Slack };
    });
    // Tab walks the bar in its visual order, row 1 left to right, then row 2: the stacked rows are a
    // visual `order` (transport.css), which once sent Tab through row 2's modes before row 1's ■ ALL.
    await page.evaluate(() => { document.activeElement?.blur(); document.body.focus(); });
    const walk = [];
    for (let i = 0; i < 80; i++) {
      await page.keyboard.press('Tab');
      const stop = await page.evaluate(() => {
        const el = document.activeElement, cmd = document.querySelector('.cmd');
        if (!el || !cmd.contains(el)) return null;
        const r = el.getBoundingClientRect(), c = cmd.getBoundingClientRect();
        return { name: el.getAttribute('aria-label') ?? el.textContent.trim(), x: r.left,
          row: cmd.classList.contains('cmd--stack') && (r.top + r.bottom) / 2 > (c.top + c.bottom) / 2 ? 2 : 1 };
      });
      if (!stop) { if (walk.length) break; continue; }
      walk.push(stop);
    }
    const visual = [...walk].sort((a, b) => a.row - b.row || a.x - b.x).map((s) => s.name);
    bar.tabOrder = walk.map((s) => `${s.row}:${s.name}`);
    bar.tabInVisualOrder = walk.length > 10 && walk.every((s, i) => s.name === visual[i]);
    const name = `${label}-${width}x${height}-engine${modes === 'idle' ? '' : `-${modes}`}`;
    await page.locator('.cmd').screenshot({ path: `logs/layout/${name}.png` });
    const ok = bar.rows <= 2 && !bar.unreachable.length && bar.on === (modes === 'idle' ? 0 : 5) && bar.tabInVisualOrder
      && bar.units.length === (modes === 'idle' ? 1 : 2) && new Set(bar.units).size === 1;
    if (!ok) failures.push(name);
    results.push({ name, ok, ...bar });
    console.log(JSON.stringify({ name, ok, ...bar }));
    await page.close();
  }
  // A fresh page's bar fit is final: the web font loads after the first measure (Geist is requested
  // once the bar has text) and moves every child's width, so a bar measured only before it can show the
  // brand unfolded while row 1 lacks its spacer minimum, then jump ~99 px at the next DOM change (the
  // first take). The sizes straddle the fold edge (1260 px caught it); an unrelated DOM change must
  // leave the bar's fit flags where the fresh page put them.
  for (const width of [1040, 1050, 1240, 1260, 1280]) {
    const { page } = await open({ viewport: { width, height: 700 }, init });
    await page.locator('.cmd .brand').waitFor();
    await page.evaluate(() => document.fonts.ready);
    await page.waitForTimeout(250);
    const fit = () => page.evaluate(() => document.querySelector('.cmd').className);
    const fresh = await fit();
    await page.evaluate(() => { const probe = document.createElement('span'); document.querySelector('.cmd').append(probe); probe.remove(); });
    await page.waitForTimeout(250);
    const forced = await fit();
    const name = `${label}-${width}-fresh-fit`;
    const ok = fresh === forced;
    if (!ok) failures.push(name);
    results.push({ name, ok, fresh, forced });
    console.log(JSON.stringify({ name, ok, fresh, forced }));
    await page.close();
  }
  // Popovers hang under the command bar's real bottom edge (a fixed offset once covered the bar's
  // second row whenever it stacked) and never run past the window's: a live native slot adds rows to
  // Audio Settings that this tier cannot render, so a planted spacer stands in for them.
  for (const [width, height] of [[960, 600], [1000, 700], [1280, 820], [1920, 1080]]) {
    const { page } = await open({ viewport: { width, height }, init });
    await boot(page, ['Empty', 'Empty', 'Empty', 'Empty', 'Empty']);
    for (const [id, show, hide] of [['lf-help-popover', 'openHelp', 'closeHelp'], ['lf-audio-popover', 'openSettings', 'closeSettings']]) {
      await page.evaluate(fn => window.__lf.ui[fn](), show);
      await page.locator(`#${id}`).waitFor();
      await page.waitForTimeout(150);
      // The real rows; a spacer planted; Audio Settings again with an error toast up (a toast never
      // covers a page: the panel ends above the toast stack and its lowest row scrolls clear of it).
      for (const mode of id === 'lf-audio-popover' ? ['rows', 'tall', 'toast'] : ['rows', 'tall']) {
        const tall = mode !== 'rows', toast = mode === 'toast';
        if (toast) {
          await page.evaluate(async () => {
            const notify = await import('/src/notify.ts');
            notify.setAutoDismissMsForProbe(60_000);
            notify.notifyError('Could not open the audio device.', 'probe: the device is busy', { label: 'Audio Settings → DEVICE', run() {} });
          });
          await page.locator('.toast').waitFor();
          await page.waitForTimeout(150);
        }
        const m = await page.evaluate(({ id, tall }) => {
          const box = el => { const r = el.getBoundingClientRect(); return { top: r.top, bottom: r.bottom, left: r.left, right: r.right }; };
          const bar = document.querySelector('.cmd'), dialog = document.getElementById(id), body = dialog.firstElementChild;
          // A scripted open (no pointer input) focuses the panel: no ring around the panel itself, while a
          // control inside keeps the app's focus ring.
          const focused = document.activeElement === dialog, ring = getComputedStyle(dialog).outlineStyle;
          const control = body.querySelector('button:not(:disabled), select:not(:disabled)');
          control.focus();
          const innerRing = getComputedStyle(control).outlineStyle;
          dialog.focus();
          // What scrolls: the tab panel that shows (Audio Settings' page beside its rail, Help's panel under
          // its tabs), never the panel body.
          const scroller = body.querySelector(':scope > [role="tabpanel"]:not([hidden])');
          // How far the real rows scroll, before any spacer: Help's reference (what `?` opens on) must not.
          const scroll = scroller.scrollHeight - scroller.clientHeight;
          // Under the title (a page's first block), above every real row.
          if (tall) scroller.firstElementChild.after(Object.assign(document.createElement('div'), { id: 'layout-spacer', style: `height: ${innerHeight}px; flex: none` }));
          // The lowest row: from the body's lowest child, step into a block taller than the scroller (a
          // tab panel, a column of Help's two-column reference) until a row fits it.
          const lowest = el => [...el.children].reduce((a, b) => (b.getBoundingClientRect().bottom >= a.getBoundingClientRect().bottom ? b : a));
          let last = lowest(scroller);
          while (last.getBoundingClientRect().height > scroller.clientHeight && last.firstElementChild) last = lowest(last);
          last.scrollIntoView({ block: 'nearest' });
          const r = box(last);
          const hit = document.elementFromPoint((r.left + r.right) / 2, (r.top + r.bottom) / 2);
          const toasts = document.querySelector('.toast') ? box(document.querySelector('.toasts')) : null;
          return { focused, ring, innerRing, stacked: bar.classList.contains('cmd--stack'), scroll, bar: box(bar), dialog: box(dialog), panel: box(body),
            lastRow: r, lastRowHit: !!hit && last.contains(hit), toasts, view: { width: innerWidth, height: innerHeight } };
        }, { id, tall });
        const name = `${label}-${width}x${height}-${id}${tall ? '-tall-content' : ''}${toast ? '-toast' : ''}`;
        await page.screenshot({ path: `logs/layout/${name}.png` });
        await page.evaluate(id => {
          document.getElementById('layout-spacer')?.remove();
          const body = document.getElementById(id).firstElementChild;
          body.scrollTop = 0;
          for (const panel of body.querySelectorAll(':scope > [role="tabpanel"]')) panel.scrollTop = 0;
          for (const t of window.__lf.notify.toasts()) window.__lf.notify.dismissToast(t.id);
        }, id);
        const overlaps = m.panel.top < m.bar.bottom && m.panel.bottom > m.bar.top && m.panel.left < m.bar.right && m.panel.right > m.bar.left;
        const inside = [m.dialog, m.panel, m.lastRow].every(b => b.top >= 0 && b.left >= 0 && b.bottom <= m.view.height + 0.5 && b.right <= m.view.width + 0.5);
        const fits = tall || id !== 'lf-help-popover' || m.scroll === 0;
        // The panel hangs from the bar's right edge and sizes itself: its left margin is never thinner.
        const margins = { left: m.panel.left, right: m.view.width - m.panel.right };
        const anchored = margins.left >= margins.right - 0.5;
        const ringless = m.focused && m.ring === 'none' && m.innerRing === 'solid';
        // With a toast up, neither the panel nor its lowest row reaches under the toast stack.
        const under = (b) => !!m.toasts && b.top < m.toasts.bottom && b.bottom > m.toasts.top + 0.5 && b.left < m.toasts.right && b.right > m.toasts.left;
        const clearOfToast = !toast || (!!m.toasts && !under(m.panel) && !under(m.lastRow));
        const ok = !overlaps && inside && anchored && m.lastRowHit && fits && ringless && clearOfToast;
        console.log(JSON.stringify({ name, ok, overlaps, inside, anchored, margins, fits, ringless, clearOfToast, gap: m.panel.top - m.bar.bottom, ...m }));
        if (!ok) failures.push(name);
        results.push({ name, ok, overlaps, inside, anchored, fits, ringless, clearOfToast, ...m });
      }
      await page.evaluate(fn => window.__lf.ui[fn](), hide);
    }
    await page.close();
  }
  // Tab and Shift+Tab wrap inside an open popover: it is non-modal, and the controls past its edges lie
  // hidden under it. From the panel (where an open puts focus) Tab walks every stop and comes back to
  // the first without ever leaving the panel; Shift+Tab from the first stop and from the panel itself
  // lands on the last. Help's Manual tab panel, a Tab stop and its own scroller under the tabs, draws its
  // focus ring inside Help's box (an outside ring was cut off along the top).
  for (const [width, height] of [[1000, 700], [1440, 900]]) {
    const { page } = await open({ viewport: { width, height }, init });
    await boot(page, ['Stopped', 'Empty', 'Empty', 'Empty', 'Empty']);
    for (const [id, show, hide] of [['lf-help-popover', 'openHelp', 'closeHelp'], ['lf-audio-popover', 'openSettings', 'closeSettings']]) {
      await page.evaluate(fn => window.__lf.ui[fn](), show);
      await page.waitForFunction(id => document.activeElement === document.getElementById(id), id);
      // Where focus is: a stable tag per element, and whether it sits inside the panel.
      const at = () => page.evaluate(id => {
        const a = document.activeElement, d = document.getElementById(id);
        a.dataset.wrapProbe ??= String((window.__wrapProbe = (window.__wrapProbe ?? 0) + 1));
        return { tag: a.dataset.wrapProbe, panel: a === d, inside: d.contains(a), name: a.getAttribute('aria-label') || a.id || a.textContent.trim().slice(0, 30) || a.tagName };
      }, id);
      const walk = [];
      for (let i = 0; i < 120; i++) {
        await page.keyboard.press('Tab');
        const now = await at();
        if (walk.length && now.tag === walk[0].tag) { walk.push(now); break; }
        walk.push(now);
        if (!now.inside) break;
      }
      const stops = walk.slice(0, -1), wrapped = walk.length > 2 && walk.at(-1).tag === walk[0].tag;
      await page.keyboard.press('Shift+Tab');
      const back = await at();
      await page.evaluate(id => document.getElementById(id).focus(), id);
      await page.keyboard.press('Shift+Tab');
      const fromPanel = await at();
      const last = stops.at(-1);
      const name = `${label}-${width}x${height}-${id}-tab-wraps`;
      const ok = wrapped && walk.every(w => w.inside && !w.panel) && back.tag === last?.tag && fromPanel.tag === last?.tag;
      console.log(JSON.stringify({ name, ok, wrapped, stops: stops.map(w => w.name), left: walk.find(w => !w.inside)?.name, back: back.name, fromPanel: fromPanel.name }));
      if (!ok) failures.push(name);
      results.push({ name, ok, wrapped, stops: stops.length, back, fromPanel });
      if (id === 'lf-help-popover') {
        await page.getByRole('tab', { name: 'Manual', exact: true }).focus();
        await page.keyboard.press('Enter');
        await page.keyboard.press('Tab');
        const ring = await page.evaluate(() => {
          const panel = document.activeElement, scroller = panel.closest('.help');
          const cs = getComputedStyle(panel), grow = parseFloat(cs.outlineOffset) + parseFloat(cs.outlineWidth);
          const p = panel.getBoundingClientRect(), s = scroller.getBoundingClientRect();
          const box = { left: s.left + scroller.clientLeft, top: s.top + scroller.clientTop };
          Object.assign(box, { right: box.left + scroller.clientWidth, bottom: box.top + scroller.clientHeight });
          return { id: panel.id, ring: cs.outlineStyle, outer: { left: p.left - grow, top: p.top - grow, right: p.right + grow, bottom: p.bottom + grow }, box };
        });
        // The panel is taller than the scroller, so its foot is out of view by design; the sides and the
        // top it scrolls to must show.
        const ringName = `${label}-${width}x${height}-help-manual-ring`;
        const ringOk = ring.id === 'lf-help-manual' && ring.ring === 'solid' && ring.outer.left >= ring.box.left - 0.5
          && ring.outer.right <= ring.box.right + 0.5 && ring.outer.top >= ring.box.top - 0.5;
        console.log(JSON.stringify({ name: ringName, ok: ringOk, ...ring }));
        if (!ringOk) failures.push(ringName);
        results.push({ name: ringName, ok: ringOk, ...ring });
      }
      await page.evaluate(fn => window.__lf.ui[fn](), hide);
    }
    await page.close();
  }
  // The selected lane follows the transport keys into view. With lane 1's FX drawer open and its FILTER
  // engaged the lane stack scrolls at both sizes (the drawer at rest is one key row, and five lanes fit
  // beside it at 1000x700), and a lane the arrows or a digit selected below its fold stayed hidden, its
  // refusal cue with it. A key sends its command; the engine's `Selected` (answered here, as the engine
  // would) moves the selection. After each answer the selected lane must lie wholly inside the stack's
  // visible rect, scrolled there from the nearer edge and not at all when it already showed; under
  // reduced motion already inside the frame's own dispatch (no smooth scroll). A pointer press on a
  // half-hidden lane's PLAY must land and scroll nothing: the lane is under the pointer.
  for (const [width, height, motion] of [[960, 600, 'reduce'], [1000, 700, 'no-preference']]) {
    const { page } = await open({ viewport: { width, height }, init });
    await page.emulateMedia({ reducedMotion: motion });
    await boot(page, ['Stopped', 'Stopped', 'Stopped', 'Stopped', 'Stopped']);
    await page.getByRole('button', { name: 'Track 1 FX', exact: true }).click();
    await page.getByRole('button', { name: 'Filter, track 1', exact: true }).click();
    await page.locator('.lp-drawer .fxp-mod--on .fxp-param').first().waitFor();
    await page.evaluate(() => {
      document.activeElement?.blur();
      const stack = document.querySelector('.lp__lanes');
      // The stack's client box: its fractional border box less the borders and a horizontal scrollbar
      // (clientHeight alone is rounded).
      window.__stackView = () => {
        const s = stack.getBoundingClientRect();
        return { top: s.top + stack.clientTop, bottom: s.bottom - (stack.offsetHeight - stack.clientTop - stack.clientHeight) };
      };
      // Lane i against that box: `top`/`bottom` are how far it sits inside each edge.
      window.__laneView = (i) => {
        const v = window.__stackView(), r = document.querySelectorAll('.lp-lane')[i].getBoundingClientRect();
        return { selected: window.__lf.looper.selectedTrack(), scrollTop: stack.scrollTop, viewHeight: v.bottom - v.top,
          overflow: stack.scrollHeight - stack.clientHeight, top: r.top - v.top, bottom: v.bottom - r.bottom };
      };
      // Capture phase, so it samples the stack before the lane's own handler runs.
      window.addEventListener('pointerdown', (e) => {
        const lane = e.target.closest?.('.lp-lane');
        window.__atPointer = { lane: [...document.querySelectorAll('.lp-lane')].indexOf(lane), scrollTop: stack.scrollTop };
      }, true);
    });
    const sentCount = () => page.evaluate(() => window.__lf.native.sent.length);
    /** Wait for the commands sent after `from`, then answer them as the engine does: `SelectTrack` and
     * `NextTrack` move the selection, `PlayStop` plays the lane. Each answer is its own feed frame; the
     * lane the selection lands on is sampled inside that frame's dispatch (`at`). */
    const answer = async (from, count = 1) => {
      await page.waitForFunction(([n, c]) => window.__lf.native.sent.length >= n + c, [from, count], { timeout: 5000 });
      return page.evaluate(([n, s]) => {
        const commands = window.__lf.native.sent.slice(n);
        let at = null, seq = s;
        for (const c of commands) {
          const sel = window.__lf.looper.selectedTrack();
          const events = c.SelectTrack !== undefined ? [{ Selected: { frame: 0, lane: c.SelectTrack } }]
            : c.Action === 'NextTrack' ? [{ Selected: { frame: 0, lane: (sel + 1) % 5 } }]
            : c.PlayStop !== undefined ? [{ Lane: { frame: 0, lane: c.PlayStop, info: { state: 'Playing', length: 96000, armed: false,
              autoArmed: false, canUndo: false, canReverse: true, reversed: false, stopAt: null, fading: false, retakePass: 0 } } }]
            : [];
          if (!events.length) continue;
          window.__lf.native.emit({ seq: ++seq, reset: false, events });
          at = window.__laneView(window.__lf.looper.selectedTrack());
        }
        return { commands, at, seq };
      }, [from, seq]).then((r) => { seq = r.seq; return r; });
    };
    // Smooth scrolling ends when scrollTop holds still for 8 frames.
    const settle = () => page.evaluate(() => new Promise(resolve => {
      const stack = document.querySelector('.lp__lanes');
      let last = stack.scrollTop, still = 0;
      const tick = () => {
        if (stack.scrollTop !== last) { last = stack.scrollTop; still = 0; } else if (++still >= 8) return resolve();
        requestAnimationFrame(tick);
      };
      requestAnimationFrame(tick);
    }));
    // Wholly inside the stack's view. A revealed lane may stop under a pixel short: the scroll range is
    // whole pixels, so at a fractional stack height the last lane cannot reach the edge (0.5 px measured
    // at 1000x700 with --plugin-source).
    const inside = (v, slack = 0.01) => v.top > -slack && v.bottom > -slack;
    const name = `${label}-${width}x${height}-select-reveal-${motion}`;
    const steps = [];
    let ok = (await page.evaluate(() => window.__laneView(0))).overflow > 0;
    let scrolled = 0;
    let selected = 0;
    for (const key of ['ArrowDown', 'ArrowDown', 'ArrowDown', 'ArrowDown', 'ArrowDown', '5', '3', '1']) {
      const target = key === 'ArrowDown' ? (selected + 1) % 5 : Number(key) - 1;
      const before = await page.evaluate(i => window.__laneView(i), target);
      const from = await sentCount();
      await page.keyboard.press(key);
      const { commands, at } = await answer(from);
      if (motion !== 'reduce') await settle();
      const after = await page.evaluate(() => window.__laneView(window.__lf.looper.selectedTrack()));
      const moved = after.scrollTop - before.scrollTop;
      const command = key === 'ArrowDown' ? { Action: 'NextTrack' } : { SelectTrack: target };
      // Nearer edge: the lane ends flush with the edge it came in from, give or take the whole pixel the
      // reveal rounds past it; the far edge would leave the stack's height minus the lane's.
      const stepOk = JSON.stringify(commands) === JSON.stringify([command])
        && after.selected === target && inside(after, 1)
        && (inside(before) ? moved === 0 : true)
        && (moved > 0 ? after.bottom < 1 : moved < 0 ? after.top < 1 : true)
        && (motion === 'reduce' ? inside(at, 1) && at.scrollTop === after.scrollTop : true);
      if (moved !== 0) scrolled++;
      ok &&= stepOk;
      selected = target;
      steps.push({ key, target: target + 1, stepOk, commands, before, after, at });
    }
    ok &&= scrolled >= 3;
    // A reveal still running stops at the next select. Two digit keydowns in one task (a footswitch or
    // MIDI double-send), answered by two feed frames a frame apart: lane 1 still shows when the second
    // is measured, and the reveal of lane 5 must not carry it off. Then '5' and a real press on lane 1's
    // waveform (no handler of its own: the press only selects) while that reveal runs: the stack must
    // stop where the press found it, before the reveal landed at the bottom.
    let rapid = null;
    if (motion !== 'reduce') {
      // Each case starts from the top, lane 1 selected (the steps above ended on '1').
      const toTop = async () => {
        await page.evaluate(() => { document.querySelector('.lp__lanes').scrollTop = 0; });
        await settle();
      };
      await toTop();
      let from = await sentCount();
      await page.evaluate(() => {
        for (const key of ['5', '1']) window.dispatchEvent(new KeyboardEvent('keydown', { key, bubbles: true }));
      });
      await page.waitForFunction((n) => window.__lf.native.sent.length >= n + 2, from, { timeout: 5000 });
      const doubleSent = await page.evaluate((n) => window.__lf.native.sent.slice(n), from);
      seq = await page.evaluate(async (s) => {
        window.__lf.native.emit({ seq: ++s, reset: false, events: [{ Selected: { frame: 0, lane: 4 } }] });
        await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
        window.__lf.native.emit({ seq: ++s, reset: false, events: [{ Selected: { frame: 0, lane: 0 } }] });
        return s;
      }, seq);
      await settle();
      const doubled = await page.evaluate(() => window.__laneView(0));
      await toTop();
      const aim = await page.evaluate(() => {
        const r = document.querySelectorAll('.lp-lane')[0].querySelector('canvas').getBoundingClientRect();
        return { x: (r.left + r.right) / 2, y: (r.top + r.bottom) / 2 };
      });
      from = await sentCount();
      await page.keyboard.press('5');
      await answer(from);
      from = await sentCount();
      await page.mouse.click(aim.x, aim.y);
      const pointerSent = (await answer(from)).commands;
      await settle();
      const held = await page.evaluate(() => ({ at: window.__atPointer, after: window.__laneView(0) }));
      const rapidOk = JSON.stringify(doubleSent) === JSON.stringify([{ SelectTrack: 4 }, { SelectTrack: 0 }])
        && doubled.selected === 0 && inside(doubled, 1)
        && JSON.stringify(pointerSent) === JSON.stringify([{ SelectTrack: 0 }])
        && held.at.lane === 0 && held.at.scrollTop < held.after.overflow
        && held.after.selected === 0 && held.after.scrollTop === held.at.scrollTop;
      ok &&= rapidOk;
      rapid = { rapidOk, doubleSent, doubled, aim, pointerSent, held };
    }
    // Half of lane 5 below the fold, then a real pointer click on its PLAY (page.mouse: a locator click
    // would scroll the button into view itself).
    const press = await page.evaluate(() => {
      const stack = document.querySelector('.lp__lanes'), lane = document.querySelectorAll('.lp-lane')[4];
      stack.scrollTop = 0;
      stack.scrollTop = Math.round(-window.__laneView(4).bottom - lane.offsetHeight / 2);
      const play = lane.querySelector('.lp-pb--play').getBoundingClientRect();
      return { scrollTop: stack.scrollTop, x: (play.left + play.right) / 2, y: (play.top + play.bottom) / 2,
        playVisible: play.bottom <= window.__stackView().bottom, lane: window.__laneView(4) };
    });
    const from = await sentCount();
    await page.mouse.click(press.x, press.y);
    const pressSent = (await answer(from, 2)).commands;
    if (motion !== 'reduce') await settle();
    await page.waitForTimeout(150);
    const pressed = await page.evaluate(() => ({ state: window.__lf.looper.stateOf(4), view: window.__laneView(4) }));
    const pointerOk = press.playVisible && !inside(press.lane)
      && JSON.stringify(pressSent) === JSON.stringify([{ SelectTrack: 4 }, { PlayStop: 4 }])
      && pressed.view.selected === 4 && pressed.state === 'PLAYING' && pressed.view.scrollTop === press.scrollTop;
    ok &&= pointerOk;
    await page.screenshot({ path: `logs/layout/${name}.png` });
    console.log(JSON.stringify({ name, ok, scrolled, rapid, pointerOk, press, pressSent, pressed, steps }));
    if (!ok) failures.push(name);
    results.push({ name, ok, scrolled, rapid, pointerOk, press, pressSent, pressed, steps });
    await page.close();
  }
  await writeFile(`logs/layout/${label}.json`, JSON.stringify(results, null, 2));
  if (failures.length) throw new Error(`Inaccessible layout: ${failures.join(', ')}`);
});
