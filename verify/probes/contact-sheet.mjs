/** Screenshot contact sheet: fixed looper scenes at three viewports (1280x820, 1920x1080, 1000x700),
 * keyboard bottom and hidden, in engine mode on the web engine fake (`src/platform/host.web.ts`, the
 * `engine-seam` pattern: `window.__lfEngineFake` set before the app loads, every lane, the clock and the
 * waveforms scripted on the feed through `__lf.native`). Scenes 1-7, 11 and 12 run in one fresh browser
 * context: 1-empty, 4-count-in (lane 1 ARMED behind the count-in), 2-first-take-recording (a first take
 * one second in), 3-armed-waiting (a two-bar loop at 60 BPM playing, lane 2 an ARMED later take waiting
 * for the boundary), 5-fx-five-lanes (five stopped one-bar lanes, lane 1's FX drawer open), 6-help,
 * 7-audio-settings, 12-stage-<look> (the stage view opened by its command-bar cap over five lanes, three
 * playing and one muted, once per look of `src/ui/stage/views.ts`, stepped with V) and 11-engine-in-fx (a reset frame with every input send on, the IN FX popover open). Scenes 8-10 are
 * the Windows app's guitar-first screen, in a second fresh context whose plugin host is served with
 * `available: true` (host.web.ts routed, the slot-sources pattern) and stubbed: a scan that finds one
 * amp-sim (an effect), stubbed load/editor replies. 8-native-first-launch = the host booted, nothing
 * loaded (slot A's source picker offers the amp-sim under Plugins); 9-amp-sim-live = the amp-sim picked in
 * slot A's picker, which auto-starts GO LIVE and the editor (INPUT LIVE); 10-amp-sim-idle = the same after
 * stopping live input. Writes logs/contact-sheet/<viewport>-<kbd>-<scene>.png plus a tiling index.html
 * unconditionally, before any FAIL is raised, so a red run still leaves the sheet for the eye lap.
 * Asserts: with FX open every lane's clear button is present, and fully visible wherever five lanes fit
 * (audit A4; at 1000x700 with the keyboard shown the lanes scroll by design); an ARMED later take draws
 * no rec-red in its canvas while waiting (audit A3); no scene logs console.error or an uncaught page error
 * (tagged with the scene it happened in). Sees only the rendered DOM/canvas of scripted states: never the
 * native engine, a real plugin host or WebView2; a human still judges the screenshots.
 * Run: pnpm probe contact-sheet
 */
import { mkdir, writeFile } from 'node:fs/promises';
import { probe } from '../harness/probe.ts';

const outDir = 'logs/contact-sheet';
const viewports = [[1280, 820], [1920, 1080], [1000, 700]];
const placements = ['bottom', 'hidden'];
const scenes = ['1-empty', '2-first-take-recording', '3-armed-waiting', '4-count-in', '5-fx-five-lanes', '6-help', '7-audio-settings',
  '8-native-first-launch', '9-amp-sim-live', '10-amp-sim-idle', '11-engine-in-fx'];
const REC_PIXEL_LIMIT = 20; // anti-aliasing slack; a red playhead or tape is hundreds of pixels
const RATE = 48000;
const PEAK_FRAMES = 1024; // the engine's waveform bin (`lf-engine/src/overview.rs`)
const AMP_SIM = { id: 'probe.amp-sim', name: 'Probe Amp Sim', format: 'vst3', isEffect: true,
  path: 'C:\\Program Files\\Common Files\\VST3\\Probe Amp Sim.vst3' };

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
/** `bins` waveform bins of lane `i` from bin 0, a visible wave a little different per lane, in a lane view
 * of `count` bins. */
const wave = (i, bins, count = bins) => {
  const max = Array.from({ length: bins }, (_, b) => 0.05 + 0.4 * Math.abs(Math.sin((b * PEAK_FRAMES) / 9000 + i)));
  return { lane: i, start: 0, count, min: max.map((v) => -v), max };
};
/** The looper's lanes at `bars` bars of `bpm`: `playing` lanes PLAYING, the other `count` lanes STOPPED,
 * the rest EMPTY, each with its waveform. */
const loopFrame = ({ count, playing, bpm = 120, bars = 1 }) => {
  const master = Math.round(RATE * (60 / bpm) * 4 * bars);
  const bins = Math.ceil(master / PEAK_FRAMES);
  return {
    events: [transport(master, true, bpm), ...[0, 1, 2, 3, 4].map((i) =>
      laneEvent(i, i >= count ? lane('Empty') : lane(playing.includes(i) ? 'Playing' : 'Stopped', { length: master, canReverse: true })))],
    anchor: anchorAt(RATE / 2),
    peaks: Array.from({ length: count }, (_, i) => wave(i, bins)),
  };
};

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
        page.on('console', (m) => { if (m.type() === 'error') errors.push({ scene, text: m.text() }); });
        page.on('pageerror', (e) => errors.push({ scene, text: String(e) }));
      };
      // Engine mode on the fake; `native` also serves host.web.ts with `available: true`, so the native-only
      // chrome (the plugin host, GO LIVE, the editor) renders.
      const openPage = async (native) => {
        const ctx = await browser.newContext();
        const opened = await open({ context: ctx, viewport: { width, height }, allowPageErrors: true, init: async (p) => {
          init(p);
          if (native) await p.route('**/src/platform/host.web.ts', async (route) => {
            const response = await route.fetch();
            await route.fulfill({ response, body: (await response.text()).replace('available: false', 'available: true') });
          });
          await p.addInitScript(() => { window.__lfEngineFake = true; });
        } });
        await opened.page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });
        await opened.page.evaluate((v) => window.__lf.layoutStore.setKeyboardPlacement(v), kbd);
        return [ctx, opened.page];
      };
      let [context, page] = await openPage(false);
      let seq = 0;
      const emit = (frame) => page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], ...frame });
      /** A reset frame: a fresh engine with every lane EMPTY and `settings` remembered. */
      const reset = (settings = []) => emit({
        reset: true,
        settings,
        events: [...[0, 1, 2, 3, 4].map((i) => laneEvent(i, lane('Empty'))), transport(0, false, 120), { Selected: { frame: 0, lane: 0 } }],
        anchor: anchorAt(0),
        meter: { peak: 0, clip: false },
      });
      const shoot = async (name) => {
        await page.evaluate(() => document.activeElement?.blur());
        const file = `${width}x${height}-${kbd}-${name}.png`;
        await page.screenshot({ path: `${outDir}/${file}` });
        shots.push({ file, viewport: `${width}x${height}`, kbd, scene: name });
      };
      const tag = `${width}x${height}-${kbd}`;

      scene = '1-empty';
      await reset();
      await page.waitForTimeout(200);
      await shoot(scene);

      // The first take counts in one bar (4 beats), then records from the counted downbeat.
      scene = '4-count-in';
      await emit({
        events: [laneEvent(0, lane('Recording', { armed: true })), transport(0, true, 120), { Beat: { frame: 0, beatInBar: 0, countLeft: 4, clicked: true } }],
        anchor: anchorAt(0),
      });
      await page.waitForTimeout(400);
      await shoot(scene);

      // One second into the take (it started on the counted downbeat, a bar after the press).
      scene = '2-first-take-recording';
      const bar120 = 2 * RATE;
      await emit({
        events: [laneEvent(0, lane('Recording'), bar120), { Beat: { frame: bar120, beatInBar: 0, countLeft: 0, clicked: true } }],
        anchor: anchorAt(bar120 + RATE),
        peaks: [wave(0, Math.ceil(RATE / PEAK_FRAMES))],
      });
      await page.waitForTimeout(400);
      await shoot(scene);

      // Lane 1 plays a two-bar loop at 60 BPM (8 s), a second in; lane 2 is a later take ARMED for the next
      // boundary, ~7 s away.
      scene = '3-armed-waiting';
      await emit(loopFrame({ count: 1, playing: [0], bpm: 60, bars: 2 }));
      await emit({ events: [laneEvent(1, lane('Recording', { armed: true }))] });
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
      await emit({ events: [laneEvent(1, lane('Empty'))] });

      scene = '5-fx-five-lanes';
      await emit(loopFrame({ count: 5, playing: [] }));
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
        if (c.missing) fail(`${tag} ${scene}: A4 Track ${c.lane} clear is missing`);
        else if (fiveLanesMustFit && c.fraction < 0.999) fail(`${tag} ${scene}: A4 Track ${c.lane} clear is ${Math.round(c.fraction * 100)}% visible (button ${c.top}..${c.bottom}px, clipped at ${c.visibleBottom}px)`);
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

      scene = '12-stage';
      await emit(loopFrame({ count: 5, playing: [0, 2, 3] }));
      await page.evaluate(() => window.__lf.looper.setMute(3, true));
      await page.getByRole('button', { name: 'Stage view', exact: true }).click();
      await page.getByRole('dialog', { name: 'Stage view', exact: true }).waitFor();
      // One scene per look, in the order V steps them (a fresh context opens on the first).
      const looks = await page.evaluate(() => import('/src/ui/stage/views.ts').then((m) => m.STAGE_VIEWS.map((v) => v.id)));
      for (const look of looks) {
        scene = `12-stage-${look}`;
        await page.locator(`.sv[data-view="${look}"]`).waitFor();
        await page.waitForTimeout(400);
        await shoot(scene);
        await page.keyboard.press('v');
      }
      await page.keyboard.press('Escape');
      await page.getByRole('dialog', { name: 'Stage view', exact: true }).waitFor({ state: 'detached' });

      // A reload's reset frame from an engine that remembers every input send on.
      scene = '11-engine-in-fx';
      await reset([{ SetInputSend: ['echo', true] }, { SetInputSend: ['reverb', true] }, { SetInputSend: ['ring', true] }]);
      await page.getByRole('button', { name: 'Input effects', exact: true }).click();
      await page.getByRole('dialog', { name: 'Input effects', exact: true }).waitFor();
      await page.waitForTimeout(250);
      await shoot(scene);
      await context.close();

      scene = '8-native-first-launch';
      [context, page] = await openPage(true);
      seq = 0;
      await page.evaluate(async (ampSim) => {
        const { platform } = await import('/src/platform/index.ts');
        const instrument = await import('/src/ui/state/instrument.ts');
        const host = platform.pluginHost;
        // Let the app's own boot chain finish (its scan found nothing), then rescan with the amp-sim.
        const deadline = Date.now() + 10000;
        while (!instrument.nativeHostReady() || instrument.scanning()) {
          if (Date.now() > deadline) throw new Error('native host boot did not finish');
          await new Promise((r) => setTimeout(r, 50));
        }
        host.scanPlugins = async () => [ampSim];
        host.loadPlugin = async (slot) => ({ slot, descriptor: ampSim });
        host.unloadPlugin = async () => {};
        host.openEditor = async () => {};
        host.listParams = async () => [];
        await instrument.scanForPlugins();
      }, AMP_SIM);
      await reset();
      await page.getByRole('combobox', { name: 'Source for slot 1', exact: true }).locator('option', { hasText: 'Probe Amp Sim' }).waitFor({ state: 'attached' });
      await page.waitForTimeout(200);
      await shoot(scene);

      scene = '9-amp-sim-live';
      await page.getByRole('combobox', { name: 'Source for slot 1', exact: true }).selectOption({ label: 'Probe Amp Sim (vst3)' });
      await page.getByRole('button', { name: 'Live input for slot 1', exact: true, pressed: true }).waitFor();
      await page.getByRole('button', { name: 'Editor for slot 1', exact: true, pressed: true }).waitFor();
      await page.waitForTimeout(250);
      await shoot(scene);

      scene = '10-amp-sim-idle';
      await page.getByRole('button', { name: 'Live input for slot 1', exact: true, pressed: true }).click();
      await page.getByRole('button', { name: 'Live input for slot 1', exact: true, pressed: false }).waitFor();
      await page.waitForTimeout(250);
      await shoot(scene);

      for (const e of errors) fail(`${tag} ${e.scene}: console error: ${e.text.slice(0, 300)}`);
      await context.close();
    }
  }

  // The stage looks come last, in the order they were shot.
  const order = (s) => (scenes.includes(s.scene) ? scenes.indexOf(s.scene) : scenes.length);
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
