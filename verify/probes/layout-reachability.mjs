/** Rendered controls and source labels at the native minimum (960x600) and a normal desktop size
 * (1400x900): measures rendered control access and canvas identity (a lane's canvas must survive a
 * keyboard-placement/FX-drawer change, not remount) across five keyboard placement/FX-drawer
 * combinations, plus the drum-pad ribbon, the two-row command-bar cap and the muted-lane readout with
 * a loop present. At 960x600, 1000x700 and 1280x820 (the Tauri default; the bar stacks into two rows
 * at all three) the open Help and Audio Settings popovers must not overlap the command bar's rendered
 * box and must stay inside the window, also with a spacer one window tall planted in the panel body
 * (native-only rows the browser tier never renders), whose last row must then scroll into reach.
 * At 960x600 (reduced motion) and 1000x700 with lane 1's FX drawer open, so the lane stack scrolls,
 * each ArrowDown and digit must bring the selected lane wholly into the stack's view from the nearer
 * edge and leave a lane already in view unscrolled; a pointer press on a half-hidden lane must land
 * and scroll nothing. At 1000x700 (smooth) a second select inside a reveal's first frames must end in
 * view, and a pointer press on a lane must stop a reveal still running where the press found it.
 * "Unreachable" means clipped below 98% visible OR the element under its own centre
 * point is not itself (something else intercepts the click). `--plugin-source` substitutes
 * `src/platform/host.web.ts` to simulate a live native plugin slot instead of the browser-tier
 * fallback. `--label=<name>` tags screenshots and `logs/layout/<name>.json`, for comparing two runs
 * (e.g. before/after a layout change). Measures the DOM only: no native chrome, no WebView2.
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
  const init = pluginSource ? async (page) => {
    await page.route('**/src/platform/host.web.ts*', async route => {
      const response = await route.fetch();
      const body = await response.text();
      if (!body.includes('available: false')) throw new Error('Could not enable simulated native chrome');
      await route.fulfill({ response, body: body.replace('available: false', 'available: true') });
    });
  } : undefined;
  for (const [width, height] of [[960, 600], [1400, 900]]) {
    const { page } = await open({ viewport: { width, height }, init });
    if (pluginSource) {
      await page.waitForFunction(() => document.querySelector('[aria-label="Rescan plugins"]')?.disabled === false);
      await page.evaluate(async () => {
      const slots = await import('/src/audio/instrument-slots.ts');
      slots.setSlotPlugins([{ id: 'layout-fixture', name: 'Archetype Petrucci', format: 'VST3', path: 'layout-fixture', isEffect: true }, null]);
      if (window.__lf.slotPlugins()[0]?.id !== 'layout-fixture') throw new Error('Restart Vite to avoid duplicate HMR module state');
      });
      await page.getByRole('button', { name: 'Go live for slot 1', exact: true }).waitFor();
    }
    await page.evaluate(async () => {
      const lf = window.__lf;
      const { defaultFxStates } = await import('/src/audio/fx/fx.ts');
      await lf.looper.init();
      const frames = lf.engine.ctx.sampleRate * 2;
      await lf.looper.loadSession({ bpm: 120, bars: 1, masterLengthFrames: frames,
        tracks: [{ index: 0, pcm: new Float32Array(frames), volume: 1, muted: false,
          reversed: false, fx: defaultFxStates() }] });
      lf.looper.stopAll();
      window.__layoutCanvases = [...document.querySelectorAll('.lp-lane canvas')];
    });
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
          if (!el.getBoundingClientRect().width) continue;
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
      const lane = document.querySelector('.lp-lane');
      return { padCount: pads.length, padsOffscreen: pads.filter(r => r.bottom > innerHeight + 0.5 || r.top < 0).length,
        cmdHeight: bar.getBoundingClientRect().height, cmdRows: rows,
        mutedAttr: lane.getAttribute('data-muted'), mutedWord: lane.querySelector('.lp-lane__state').textContent.trim() };
    });
    await page.evaluate(() => { window.__lf.looper.setMute(0, false); window.__lf.selectSynth(0, 'lead'); });
    const drumName = `${label}-${width}x${height}-drums-muted`;
    const drumOk = drums.padCount === 16 && drums.padsOffscreen === 0 && drums.cmdRows <= 2 && drums.mutedAttr === 'true' && drums.mutedWord === 'MUTED';
    if (!drumOk) failures.push(drumName);
    results.push({ name: drumName, ...drums });
    console.log(JSON.stringify({ name: drumName, ...drums, ok: drumOk }));
    await page.close();
  }
  // Popovers hang under the command bar's real bottom edge (a fixed offset once covered the bar's
  // second row whenever it stacked) and never run past the window's: a live native slot adds rows to
  // Audio Settings that this tier cannot render, so a planted spacer stands in for them.
  for (const [width, height] of [[960, 600], [1000, 700], [1280, 820]]) {
    const { page } = await open({ viewport: { width, height }, init });
    for (const [id, show, hide] of [['lf-help-popover', 'openHelp', 'closeHelp'], ['lf-audio-popover', 'openSettings', 'closeSettings']]) {
      await page.evaluate(fn => window.__lf.ui[fn](), show);
      await page.locator(`#${id}`).waitFor();
      await page.waitForTimeout(150);
      for (const tall of [false, true]) {
        const m = await page.evaluate(({ id, tall }) => {
          const box = el => { const r = el.getBoundingClientRect(); return { top: r.top, bottom: r.bottom, left: r.left, right: r.right }; };
          const bar = document.querySelector('.cmd'), dialog = document.getElementById(id), body = dialog.firstElementChild;
          // Under the title, above every real row.
          if (tall) body.firstElementChild.after(Object.assign(document.createElement('div'), { id: 'layout-spacer', style: `height: ${innerHeight}px; flex: none` }));
          const last = body.lastElementChild;
          last.scrollIntoView({ block: 'nearest' });
          const r = box(last);
          const hit = document.elementFromPoint((r.left + r.right) / 2, (r.top + r.bottom) / 2);
          return { stacked: bar.classList.contains('cmd--stack'), bar: box(bar), dialog: box(dialog), panel: box(body),
            lastRow: r, lastRowHit: !!hit && last.contains(hit), view: { width: innerWidth, height: innerHeight } };
        }, { id, tall });
        const name = `${label}-${width}x${height}-${id}${tall ? '-tall-content' : ''}`;
        await page.screenshot({ path: `logs/layout/${name}.png` });
        await page.evaluate(id => { document.getElementById('layout-spacer')?.remove(); document.getElementById(id).firstElementChild.scrollTop = 0; }, id);
        const overlaps = m.panel.top < m.bar.bottom && m.panel.bottom > m.bar.top && m.panel.left < m.bar.right && m.panel.right > m.bar.left;
        const inside = [m.dialog, m.panel, m.lastRow].every(b => b.top >= 0 && b.left >= 0 && b.bottom <= m.view.height + 0.5 && b.right <= m.view.width + 0.5);
        const ok = !overlaps && inside && m.lastRowHit;
        console.log(JSON.stringify({ name, ok, overlaps, inside, gap: m.panel.top - m.bar.bottom, ...m }));
        if (!ok) failures.push(name);
        results.push({ name, ok, overlaps, inside, ...m });
      }
      await page.evaluate(fn => window.__lf.ui[fn](), hide);
    }
    await page.close();
  }
  // The selected lane follows the transport keys into view. With lane 1's FX drawer open the lane stack
  // scrolls at both sizes, and a lane the arrows or a digit selected below its fold stayed hidden, its
  // refusal cue with it. After each press the selected lane must lie wholly inside the stack's visible
  // rect, scrolled there from the nearer edge and not at all when it already showed; under reduced
  // motion already inside the key's own dispatch (no smooth scroll). A pointer press on a half-hidden
  // lane's PLAY must land and scroll nothing: the lane is under the pointer.
  for (const [width, height, motion] of [[960, 600, 'reduce'], [1000, 700, 'no-preference']]) {
    const { page } = await open({ viewport: { width, height }, init });
    await page.emulateMedia({ reducedMotion: motion });
    await page.evaluate(async () => {
      const lf = window.__lf;
      const { defaultFxStates } = await import('/src/audio/fx/fx.ts');
      await lf.looper.init();
      const frames = lf.engine.ctx.sampleRate * 2;
      await lf.looper.loadSession({ bpm: 120, bars: 1, masterLengthFrames: frames,
        tracks: [0, 1, 2, 3, 4].map(index => ({ index, pcm: new Float32Array(frames), volume: 1, muted: false,
          reversed: false, fx: defaultFxStates() })) });
      lf.looper.stopAll();
    });
    await page.getByRole('button', { name: 'Track 1 FX', exact: true }).click();
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
      // Registered after the app's transport handler, so it samples the lane right after that handler ran.
      window.addEventListener('keydown', () => { window.__atKey = window.__laneView(window.__lf.looper.selectedTrack()); });
      // Capture phase, so it samples the stack before the lane's own handler runs.
      window.addEventListener('pointerdown', (e) => {
        const lane = e.target.closest?.('.lp-lane');
        window.__atPointer = { lane: [...document.querySelectorAll('.lp-lane')].indexOf(lane), scrollTop: stack.scrollTop };
      }, true);
    });
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
      await page.keyboard.press(key);
      if (motion !== 'reduce') await settle();
      const { at, after } = await page.evaluate(() => ({ at: window.__atKey, after: window.__laneView(window.__lf.looper.selectedTrack()) }));
      const moved = after.scrollTop - before.scrollTop;
      // Nearer edge: the lane ends flush with the edge it came in from, give or take the whole pixel the
      // reveal rounds past it; the far edge would leave the stack's height minus the lane's.
      const stepOk = after.selected === target && inside(after, 1)
        && (inside(before) ? moved === 0 : true)
        && (moved > 0 ? after.bottom < 1 : moved < 0 ? after.top < 1 : true)
        && (motion === 'reduce' ? inside(at, 1) && at.scrollTop === after.scrollTop : true);
      if (moved !== 0) scrolled++;
      ok &&= stepOk;
      selected = target;
      steps.push({ key, target: target + 1, stepOk, before, after, at });
    }
    ok &&= scrolled >= 3;
    // A reveal still running stops at the next select. Two digit keydowns in one task (a footswitch or
    // MIDI double-send): lane 1 still shows when the second is measured, and the reveal of lane 5 must
    // not carry it off. Then '5' and a real press on lane 1's waveform (no handler of its own: the press
    // only selects) while that reveal runs: the stack must stop where the press found it, before the
    // reveal landed at the bottom.
    let rapid = null;
    if (motion !== 'reduce') {
      // Each case starts from the top, lane 1 selected (the steps above ended on '1').
      const toTop = async () => {
        await page.evaluate(() => { document.querySelector('.lp__lanes').scrollTop = 0; });
        await settle();
      };
      await toTop();
      await page.evaluate(() => {
        for (const key of ['5', '1']) window.dispatchEvent(new KeyboardEvent('keydown', { key, bubbles: true }));
      });
      await settle();
      const doubled = await page.evaluate(() => window.__laneView(0));
      await toTop();
      const aim = await page.evaluate(() => {
        const r = document.querySelectorAll('.lp-lane')[0].querySelector('canvas').getBoundingClientRect();
        return { x: (r.left + r.right) / 2, y: (r.top + r.bottom) / 2 };
      });
      await page.keyboard.press('5');
      await page.mouse.click(aim.x, aim.y);
      await settle();
      const held = await page.evaluate(() => ({ at: window.__atPointer, after: window.__laneView(0) }));
      const rapidOk = doubled.selected === 0 && inside(doubled, 1)
        && held.at.lane === 0 && held.at.scrollTop < held.after.overflow
        && held.after.selected === 0 && held.after.scrollTop === held.at.scrollTop;
      ok &&= rapidOk;
      rapid = { rapidOk, doubled, aim, held };
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
    await page.mouse.click(press.x, press.y);
    if (motion !== 'reduce') await settle();
    await page.waitForTimeout(150);
    const pressed = await page.evaluate(() => ({ state: window.__lf.looper.stateOf(4), view: window.__laneView(4) }));
    const pointerOk = press.playVisible && !inside(press.lane) && pressed.view.selected === 4
      && pressed.state === 'PLAYING' && pressed.view.scrollTop === press.scrollTop;
    ok &&= pointerOk;
    await page.screenshot({ path: `logs/layout/${name}.png` });
    console.log(JSON.stringify({ name, ok, scrolled, rapid, pointerOk, press, pressed, steps }));
    if (!ok) failures.push(name);
    results.push({ name, ok, scrolled, rapid, pointerOk, press, pressed, steps });
    await page.close();
  }
  await writeFile(`logs/layout/${label}.json`, JSON.stringify(results, null, 2));
  if (failures.length) throw new Error(`Inaccessible layout: ${failures.join(', ')}`);
});
