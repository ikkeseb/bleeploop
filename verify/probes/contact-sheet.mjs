/** Screenshot contact sheet: fixed looper scenes at three viewports (1280x820, 1920x1080, 1000x700),
 * keyboard bottom and hidden, each in its own fresh browser context. Scenes 1-7 are the web tier (synth
 * pills, no plugin host). Scenes 8-10 are the Windows app's guitar-first screen, in a second fresh
 * context whose platform host is substituted (the plugin-slot-pending pattern): `available` on, a scan
 * that finds one amp-sim (an effect whose input arm succeeds, standing in for an input bus), and stubbed
 * load/arm/editor replies. 8-native-first-launch = the host booted, nothing loaded (slot A offers
 * "Load amp / plugin…"); 9-amp-sim-live = the amp-sim picked in slot A's dropdown, which auto-starts
 * GO LIVE and the editor (INPUT LIVE); 10-amp-sim-idle = the same after stopping live input. Those
 * scenes prove the frontend's rendering of a native host's answers, never the native side.
 * 11-engine-in-fx is engine mode on the web engine fake (the engine-seam pattern: `__lfEngineFake` set
 * before the app loads, a scripted reset frame with both input sends on) with the IN FX popover open;
 * it proves the rendering only, never the engine. 12-stage-view = the stage view (src/ui/stage/) over
 * five loaded lanes (playing, stopped, one muted), in the web-tier context, opened by its command-bar
 * cap. Writes
 * logs/contact-sheet/<viewport>-<kbd>-<scene>.png plus a tiling index.html unconditionally, before any
 * FAIL is raised, so a red run still leaves the sheet for the eye lap. Asserts: with FX open every
 * lane's clear button is fully visible (audit A4); an ARMED later take draws no rec-red in its canvas
 * while waiting (audit A3); no scene logs console.error or an uncaught page error (tagged with the
 * scene it happened in). Sees only the rendered DOM/canvas; a human still judges the screenshots.
 * Run: pnpm probe contact-sheet
 */
import { mkdir, writeFile } from 'node:fs/promises';
import { probe } from '../harness/probe.ts';

const outDir = 'logs/contact-sheet';
const viewports = [[1280, 820], [1920, 1080], [1000, 700]];
const placements = ['bottom', 'hidden'];
const scenes = ['1-empty', '2-first-take-recording', '3-armed-waiting', '4-count-in', '5-fx-five-lanes', '6-help', '7-audio-settings',
  '8-native-first-launch', '9-amp-sim-live', '10-amp-sim-idle', '11-engine-in-fx', '12-stage-view'];
const REC_PIXEL_LIMIT = 20; // anti-aliasing slack; a red playhead or tape is hundreds of pixels

/** Five one-bar lanes at 120 BPM with a visible wave; `playing` lanes start PLAYING. */
async function loadLanes(page, { count, playing, bpm = 120, bars = 1 }) {
  await page.evaluate(async ({ count, playing, bpm, bars }) => {
    const lf = window.__lf;
    const { defaultFxStates } = await import('/src/audio/fx/fx.ts');
    lf.looper.clearAll();
    const frames = Math.round(lf.engine.ctx.sampleRate * (60 / bpm) * 4 * bars);
    const tracks = Array.from({ length: count }, (_, index) => {
      const pcm = new Float32Array(frames);
      for (let f = 0; f < frames; f++) pcm[f] = 0.4 * Math.sin(f / (40 + index * 9)) * Math.abs(Math.sin(f / 9000));
      return { index, pcm, volume: 1, muted: false, reversed: false,
        state: playing.includes(index) ? 'PLAYING' : 'STOPPED', fx: defaultFxStates() };
    });
    lf.looper.setFixedLengthEnabled(false);
    lf.looper.setLoopEndStopEnabled(false);
    await lf.looper.loadSession({ bpm, bars, masterLengthFrames: frames, tracks });
  }, { count, playing, bpm, bars });
}

await probe(async ({ browser, open }) => {
  await mkdir(outDir, { recursive: true });
  const failures = [];
  const shots = [];
  const fail = (msg) => { failures.push(msg); console.log(`FAIL ${msg}`); };

  for (const [width, height] of viewports) {
    for (const kbd of placements) {
      let scene = 'load';
      const errors = [];
      const init = (page) => {
        // `[rec-comp] snapshot` is record-latency.ts's DEV-only going-live log line, not a fault.
        page.on('console', (m) => { if (m.type() === 'error' && !m.text().startsWith('[rec-comp] snapshot')) errors.push({ scene, text: m.text() }); });
        page.on('pageerror', (e) => errors.push({ scene, text: String(e) }));
      };
      // `native` serves host.web.ts with `available: true`, so the native-only chrome renders; `engine`
      // boots engine mode on the web engine fake instead.
      const openPage = async (mode) => {
        const ctx = await browser.newContext();
        const opened = await open({ context: ctx, viewport: { width, height }, allowPageErrors: true, init: (p) => {
          init(p);
          if (mode === 'engine') return p.addInitScript(() => { window.__lfEngineFake = true; });
          if (mode === 'native') return p.route('**/src/platform/host.web.ts', async (route) => {
            const response = await route.fetch();
            await route.fulfill({ response, body: (await response.text()).replace('available: false', 'available: true') });
          });
        } });
        await opened.page.evaluate((v) => window.__lf.layoutStore.setKeyboardPlacement(v), kbd);
        return [ctx, opened.page];
      };
      let [context, page] = await openPage('web');
      const shoot = async (name) => {
        await page.evaluate(() => document.activeElement?.blur());
        const file = `${width}x${height}-${kbd}-${name}.png`;
        await page.screenshot({ path: `${outDir}/${file}` });
        shots.push({ file, viewport: `${width}x${height}`, kbd, scene: name });
      };
      const tag = `${width}x${height}-${kbd}`;

      scene = '1-empty';
      await page.evaluate(() => window.__lf.looper.init());
      await page.waitForTimeout(200);
      await shoot(scene);

      // The first take counts in one bar (4 beats), then records from the counted downbeat.
      scene = '4-count-in';
      await page.evaluate(() => window.__lf.looper.recDub(0));
      await page.waitForFunction(() => window.__lf.looper.trackInfo(0).armed);
      await page.waitForTimeout(400);
      await shoot(scene);

      scene = '2-first-take-recording';
      await page.waitForFunction(() => !window.__lf.looper.trackInfo(0).armed && window.__lf.looper.stateOf(0) === 'RECORDING', undefined, { timeout: 10000 });
      await page.waitForTimeout(1000);
      await shoot(scene);

      // Lane 1 plays a two-bar loop at 60 BPM (8 s); lane 2 arms a later take that waits ~7 s for the next boundary.
      scene = '3-armed-waiting';
      await page.evaluate(() => window.__lf.looper.stop(0));
      await page.waitForFunction(() => window.__lf.looper.stateOf(0) !== 'RECORDING');
      await loadLanes(page, { count: 1, playing: [0], bpm: 60, bars: 2 });
      // Loading starts playback on a boundary; arming at once would catch it, so let the loop run a second.
      await page.waitForTimeout(1000);
      await page.evaluate(() => window.__lf.looper.recDub(1));
      await page.waitForFunction(() => window.__lf.looper.trackInfo(1).armed);
      await page.waitForTimeout(700);
      await shoot(scene);
      const red = await page.evaluate(() => {
        // --rec read from the live stylesheet, so a palette retune never turns this check vacuous
        const probe = document.createElement('i');
        probe.style.color = 'var(--rec)';
        document.body.append(probe);
        const REC = getComputedStyle(probe).color.match(/\d+/g).slice(0, 3).map(Number);
        probe.remove();
        const near = (canvas) => {
          const c = document.createElement('canvas');
          c.width = canvas.width; c.height = canvas.height;
          const g = c.getContext('2d');
          g.drawImage(canvas, 0, 0);
          const d = g.getImageData(0, 0, c.width, c.height).data;
          let n = 0;
          for (let p = 0; p < d.length; p += 4) {
            if (d[p + 3] > 128 && Math.abs(d[p] - REC[0]) < 40 && Math.abs(d[p + 1] - REC[1]) < 50 && Math.abs(d[p + 2] - REC[2]) < 50) n++;
          }
          return n;
        };
        const canvases = document.querySelectorAll('.lp-lane canvas');
        return { armed: window.__lf.looper.trackInfo(1).armed, rec: REC.join(','), lane2RecPixels: near(canvases[1]), lane3RecPixels: near(canvases[2]) };
      });
      console.log(JSON.stringify({ tag, scene, ...red }));
      if (!red.armed) fail(`${tag} ${scene}: lane 2 was no longer armed at measurement`);
      else if (red.lane2RecPixels > REC_PIXEL_LIMIT) fail(`${tag} ${scene}: A3 armed lane 2 canvas has ${red.lane2RecPixels} rec-red pixels while waiting (limit ${REC_PIXEL_LIMIT}; empty lane 3 has ${red.lane3RecPixels})`);
      await page.evaluate(() => window.__lf.looper.stop(1));
      await page.waitForFunction(() => window.__lf.looper.stateOf(1) === 'EMPTY');

      scene = '5-fx-five-lanes';
      await loadLanes(page, { count: 5, playing: [] });
      await page.getByRole('button', { name: 'Track 1 FX', exact: true }).click();
      await page.getByRole('group', { name: 'FX, Track 1', exact: true }).waitFor();
      await page.waitForTimeout(250);
      await shoot(scene);
      const clr = await page.evaluate(() => [1, 2, 3, 4, 5].map((n) => {
        const el = [...document.querySelectorAll('button')].find((b) => b.getAttribute('aria-label') === `Track ${n} clear`);
        if (!el) return { lane: n, fraction: 0, missing: true };
        const r = el.getBoundingClientRect();
        let left = Math.max(0, r.left), right = Math.min(innerWidth, r.right);
        let top = Math.max(0, r.top), bottom = Math.min(innerHeight, r.bottom);
        for (let a = el.parentElement; a; a = a.parentElement) {
          const s = getComputedStyle(a), b = a.getBoundingClientRect();
          if (s.overflowX !== 'visible') { left = Math.max(left, b.left); right = Math.min(right, b.right); }
          if (s.overflowY !== 'visible') { top = Math.max(top, b.top); bottom = Math.min(bottom, b.bottom); }
        }
        const fraction = Math.max(0, right - left) * Math.max(0, bottom - top) / (r.width * r.height);
        return { lane: n, fraction: Math.round(fraction * 1000) / 1000, top: Math.round(r.top), bottom: Math.round(r.bottom), visibleBottom: Math.round(bottom) };
      }));
      console.log(JSON.stringify({ tag, scene, clr }));
      // Five 57 px lanes plus the drawer do not fit a ~370 px container (1000x700 with the keyboard
      // shown): that corner scrolls by design, so the five-full-lanes rule holds only above it.
      const fiveLanesMustFit = height >= 820 || kbd === 'hidden';
      for (const c of clr) {
        if (fiveLanesMustFit && c.fraction < 0.999) fail(`${tag} ${scene}: A4 Track ${c.lane} clear is ${Math.round(c.fraction * 100)}% visible (button ${c.top}..${c.bottom}px, clipped at ${c.visibleBottom}px)`);
      }
      await page.getByRole('button', { name: 'Track 1 FX', exact: true }).click();

      scene = '6-help';
      await page.evaluate(() => window.__lf.ui.openHelp());
      await page.waitForTimeout(250);
      await shoot(scene);
      await page.evaluate(() => window.__lf.ui.closeHelp());

      scene = '7-audio-settings';
      await page.evaluate(() => window.__lf.ui.openSettings());
      await page.getByRole('group', { name: 'Audio settings', exact: true }).waitFor();
      await page.waitForTimeout(250);
      await shoot(scene);
      await page.evaluate(() => window.__lf.ui.closeSettings());

      scene = '12-stage-view';
      await loadLanes(page, { count: 5, playing: [0, 2, 3] });
      await page.evaluate(() => window.__lf.looper.setMute(3, true));
      await page.getByRole('button', { name: 'Stage view', exact: true }).click();
      await page.getByRole('dialog', { name: 'Stage view', exact: true }).waitFor();
      await page.waitForTimeout(400);
      await shoot(scene);
      await page.keyboard.press('Escape');
      await context.close();

      scene = '8-native-first-launch';
      [context, page] = await openPage('native');
      await page.evaluate(async () => {
        const { platform } = await import('/src/platform/index.ts');
        const instrument = await import('/src/audio/instrument.ts');
        const host = platform.pluginHost;
        // Let the app's own boot chain finish (its scan found nothing), then rescan with the amp-sim.
        const deadline = Date.now() + 10000;
        while (!instrument.nativeHostReady() || instrument.scanning()) {
          if (Date.now() > deadline) throw new Error('native host boot did not finish');
          await new Promise((r) => setTimeout(r, 50));
        }
        const ampSim = { id: 'probe.amp-sim', name: 'Probe Amp Sim', format: 'vst3', isEffect: true,
          path: 'C:\\Program Files\\Common Files\\VST3\\Probe Amp Sim.vst3' };
        host.scanPlugins = async () => [ampSim];
        host.loadPlugin = async (slot) => ({ slot, descriptor: ampSim });
        host.armInput = async () => {};
        host.armMonitor = async () => {};
        host.openEditor = async () => {};
        await instrument.scanForPlugins();
      });
      await page.getByRole('combobox', { name: 'Native plugin for slot 1', exact: true }).waitFor();
      await page.waitForTimeout(200);
      await shoot(scene);

      scene = '9-amp-sim-live';
      await page.getByRole('combobox', { name: 'Native plugin for slot 1', exact: true }).selectOption({ label: 'Probe Amp Sim (vst3)' });
      await page.getByRole('button', { name: 'Stop live input for slot 1', exact: true }).waitFor();
      await page.getByRole('button', { name: 'Close editor for slot 1', exact: true }).waitFor();
      await page.waitForTimeout(250);
      await shoot(scene);

      scene = '10-amp-sim-idle';
      await page.getByRole('button', { name: 'Stop live input for slot 1', exact: true }).click();
      await page.getByRole('button', { name: 'Go live for slot 1', exact: true }).waitFor();
      await page.waitForTimeout(250);
      await shoot(scene);

      await context.close();

      scene = '11-engine-in-fx';
      [context, page] = await openPage('engine');
      await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });
      await page.evaluate(() => {
        const info = { state: 'Empty', length: 0, armed: false, autoArmed: false, canUndo: false, canReverse: false, reversed: false, stopAt: null, retakePass: 0 };
        window.__lf.native.emit({
          seq: 1,
          reset: true,
          settings: [{ SetInputSend: ['echo', true] }, { SetInputSend: ['reverb', true] }],
          events: [...[0, 1, 2, 3, 4].map((lane) => ({ Lane: { frame: 0, lane, info } })),
            { Transport: { frame: 0, master: 0, bpm: 120, locked: false } }, { Selected: { frame: 0, lane: 0 } }],
          device: [],
          anchor: { frame: 0, atMs: Date.now(), rate: 48000, grid: 0 },
          meter: { peak: 0, clip: false },
          peaks: [],
        });
      });
      await page.getByRole('button', { name: 'Input effects', exact: true }).click();
      await page.getByRole('dialog', { name: 'Input effects', exact: true }).waitFor();
      await page.waitForTimeout(250);
      await shoot(scene);

      for (const e of errors) fail(`${tag} ${e.scene}: console error: ${e.text.slice(0, 300)}`);
      await context.close();
    }
  }

  const order = (s) => scenes.indexOf(s.scene);
  const rows = viewports.flatMap(([w, h]) => placements.map((kbd) => {
    const cells = shots.filter((s) => s.viewport === `${w}x${h}` && s.kbd === kbd).sort((a, b) => order(a) - order(b))
      .map((s) => `<figure><a href="${s.file}"><img src="${s.file}" loading="lazy"></a><figcaption>${s.scene}</figcaption></figure>`).join('');
    return `<h2>${w}x${h}, keyboard ${kbd}</h2><div class="row">${cells}</div>`;
  })).join('\n');
  await writeFile(`${outDir}/index.html`, `<!doctype html><meta charset="utf-8"><title>BleepLoop contact sheet</title>
<style>body{background:#111;color:#ddd;font:13px system-ui;margin:16px}.row{display:flex;gap:8px;overflow-x:auto}
figure{margin:0;flex:0 0 auto}img{width:320px;border:1px solid #333;display:block}h2{font-size:14px;margin:18px 0 6px}</style>
<h1>BleepLoop contact sheet</h1><p>${new Date().toISOString()} · ${failures.length} FAIL</p>
${failures.length ? `<pre>${failures.map((f) => `FAIL ${f}`).join('\n').replace(/</g, '&lt;')}</pre>` : ''}
${rows}
`);
  console.log(`${shots.length} screenshots + index.html in ${outDir}`);
  if (failures.length) throw new Error(`${failures.length} FAIL`);
});
