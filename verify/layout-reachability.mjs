/** Rendered controls and source labels at the native minimum and a normal desktop size. */
import { chromium } from 'playwright';
import { mkdir, writeFile } from 'node:fs/promises';

const url = process.argv.find(arg => arg.startsWith('--url='))?.slice(6) ?? 'http://localhost:1420';
const label = process.argv.find(arg => arg.startsWith('--label='))?.slice(8) ?? 'current';
const pluginSource = process.argv.includes('--plugin-source');
await mkdir('logs/layout', { recursive: true });
const browser = await chromium.launch({ args: ['--autoplay-policy=no-user-gesture-required'] });
const results = [];
const failures = [];
try {
  for (const [width, height] of [[960, 600], [1400, 900]]) {
    const page = await browser.newPage({ viewport: { width, height } });
    if (pluginSource) await page.route('**/src/platform/host.web.ts*', async route => {
      const response = await route.fetch();
      const body = await response.text();
      if (!body.includes('available: false')) throw new Error('Could not enable simulated native chrome');
      await route.fulfill({ response, body: body.replace('available: false', 'available: true') });
    });
    await page.goto(url);
    await page.waitForFunction(() => !!window.__lf);
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
      const rows = new Set([...bar.children].map(el => el.getBoundingClientRect())
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
  await writeFile(`logs/layout/${label}.json`, JSON.stringify(results, null, 2));
  if (failures.length) throw new Error(`Inaccessible layout: ${failures.join(', ')}`);
} finally { await browser.close(); }
