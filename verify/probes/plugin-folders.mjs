/**
 * The plugin folders section of Audio Settings, on the web engine fake (`src/platform/host.web.ts`,
 * the engine-seam pattern) with a stand-in plugin host whose folder list, folder dialog and scan the
 * probe scripts. Drives the real section and asserts what it shows and what it asks the host for:
 *
 * - opened, it lists the host's built-in folders (read-only, no remove button) and the player's own
 *   (each with `Remove folder <path>`); a folder that is gone says `missing`; a long path is cut with
 *   an ellipsis, carries the whole path as its title and does not widen the panel (measured against
 *   the panel with the section hidden);
 * - `Add folder…` asks the host (which opens the dialog: no path leaves the page), shows the list it
 *   answers and runs one scan, not forced; the picker lists what that scan found;
 * - a remove sends the folder's exact path, shows the answered list and scans again;
 * - a cancelled dialog (the host answers null) changes nothing and scans nothing;
 * - a refused add shows a toast and logs the error; the list stays;
 * - while a scan runs, `Add folder…` is disabled; an add that still completes during a scan held open
 *   (the scan walked the folders it started with) is followed by ONE more scan, and the picker ends
 *   on that scan's result.
 *
 * Cannot see the native side: the folder dialog, `plugin-folders.json`, the walk (those are `cargo
 * test`, `src-tauri/src/host/folders.rs` and `scan.rs`; the dialog itself has no test). Saves
 * logs/plugin-folders/section.png. Run: pnpm probe plugin-folders
 */
import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { probe } from '../harness/probe.ts';

const RATE = 48000;
const AMP = { id: 'probe.amp', name: 'Probe Amp Sim', format: 'vst3', isEffect: true, path: 'C:\\Program Files\\Common Files\\VST3\\amp.vst3' };
const FOUND = { id: 'probe.found', name: 'Found In Folder', format: 'clap', isEffect: false, path: 'E:\\Amp Sims\\found.clap' };
const LATE = { id: 'probe.late', name: 'Late Arrival', format: 'vst3', isEffect: true, path: 'F:\\Late\\late.vst3' };
const BUILTIN = [
  { path: 'C:\\Program Files\\Common Files\\CLAP', exists: true },
  { path: 'C:\\Users\\probe\\AppData\\Local\\Programs\\Common\\CLAP', exists: false },
  { path: 'C:\\Program Files\\Common Files\\VST3', exists: true },
];
const OWN = { path: 'D:\\Plugins', exists: true };
const LONG = { path: `D:\\${'A very long folder name\\'.repeat(14)}Plugins`, exists: false };
const ADDED = { path: 'E:\\Amp Sims', exists: true };
const LATE_FOLDER = { path: 'F:\\Late', exists: true };
const STALE_FOLDER = { path: 'G:\\Stale', exists: true };
const empty = { state: 'Empty', length: 0, armed: false, autoArmed: false, canUndo: false, canReverse: false, reversed: false, stopAt: null, fading: false, retakePass: 0 };

/** The engine fake on, and a plugin host that is there (its replies scripted in `boot`). */
async function engineInit(page) {
  await page.route('**/src/platform/host.web.ts', async (route) => {
    const response = await route.fetch();
    await route.fulfill({ response, body: (await response.text()).replace('available: false', 'available: true') });
  });
  await page.addInitScript(() => {
    window.__lfEngineFake = true;
  });
}

/**
 * Wait for the engine boot, then script the host (`window.__folders`): `lists` is what it holds,
 * `plugins` what a scan finds (read when the scan STARTS), `nextAdd` what the dialog's add does (a
 * folder, null for cancel, a string for a refusal), `gate` a promise a scan waits on before it
 * answers, `calls` everything the page asked for.
 */
async function boot(page, script) {
  await page.waitForFunction(() => window.__lf.native.opened.length >= 1, undefined, { timeout: 5000 });
  await page.evaluate(async ({ script, RATE, empty }) => {
    const { platform } = await import('/src/platform/index.ts');
    const instrument = await import('/src/ui/state/instrument.ts');
    const deadline = Date.now() + 10000;
    while (!instrument.nativeHostReady() || instrument.scanning()) {
      if (Date.now() > deadline) throw new Error('native host boot did not finish');
      await new Promise((r) => setTimeout(r, 20));
    }
    const state = (window.__folders = { calls: [], gate: null, nextAdd: null, ...script });
    const host = platform.pluginHost;
    host.pluginFolders = async () => {
      state.calls.push('folders');
      return state.lists;
    };
    host.addPluginFolder = async (...args) => {
      state.calls.push(`add(${args.length})`);
      const next = state.nextAdd;
      if (typeof next === 'string') throw new Error(next);
      if (next === null) return null;
      state.lists = { ...state.lists, user: [...state.lists.user, next.folder] };
      state.plugins = next.plugins;
      return state.lists;
    };
    host.removePluginFolder = async (path) => {
      state.calls.push(`remove:${path}`);
      state.lists = { ...state.lists, user: state.lists.user.filter((f) => f.path !== path) };
      state.plugins = state.plugins.filter((p) => !p.path.startsWith(path));
      return state.lists;
    };
    host.scanPlugins = async (force) => {
      state.calls.push(`scan:${force}`);
      const found = [...state.plugins]; // what the folders held when this scan started
      if (state.gate) await state.gate;
      return found;
    };
    await instrument.scanForPlugins();
    state.calls.length = 0;
    window.__lf.native.emit({
      seq: 1,
      reset: true,
      settings: [],
      events: [...[0, 1, 2, 3, 4].map((lane) => ({ Lane: { frame: 0, lane, info: empty } })),
        { Transport: { frame: 0, master: 0, bpm: 120, locked: false } }, { Selected: { frame: 0, lane: 0 } }],
      anchor: { frame: 0, atMs: Date.now(), rate: RATE, grid: 0 },
      meter: { peak: 0, clip: false },
    });
  }, { script, RATE, empty });
}

await probe(async ({ open }) => {
  const { page, consoleErrors } = await open({ viewport: { width: 1280, height: 800 }, init: engineInit });
  await boot(page, { lists: { builtin: BUILTIN, user: [OWN, LONG] }, plugins: [AMP] });

  const section = page.getByRole('group', { name: 'Plugin folders', exact: true });
  const addButton = section.getByRole('button', { name: 'Add folder…', exact: true });
  const calls = () => page.evaluate(() => [...window.__folders.calls]);
  const clearCalls = () => page.evaluate(() => void (window.__folders.calls.length = 0));
  /** Each line as shown: its path, whether it is a built-in, its `missing` word, its remove button. */
  const rows = () => section.locator('.audio-settings__folder').evaluateAll((lines) => lines.map((li) => ({
    path: li.querySelector('.audio-settings__folder-path').textContent,
    title: li.title,
    builtin: li.classList.contains('audio-settings__folder--builtin'),
    missing: li.querySelector('.audio-settings__folder-missing')?.textContent ?? null,
    remove: li.querySelector('button')?.getAttribute('aria-label') ?? null,
  })));
  const row = (folder, builtin) => ({
    path: folder.path,
    title: folder.path,
    builtin,
    missing: folder.exists ? null : 'missing',
    remove: builtin ? null : `Remove folder ${folder.path}`,
  });
  const listed = (user) => [...BUILTIN.map((f) => row(f, true)), ...user.map((f) => row(f, false))];
  /** The Plugins group of slot 1's source picker. */
  const picker = () => page.locator('[aria-label="Source for slot 1"] optgroup[label="Plugins"] option').allTextContents();
  const idle = () => page.waitForFunction(async () => {
    const instrument = await import('/src/ui/state/instrument.ts');
    const folders = await import('/src/ui/state/plugin-folders.ts');
    return !instrument.scanning() && !folders.pluginFoldersBusy();
  });

  // ── Opened: the built-in folders and the player's own ───────────────────────────────────────────────
  await page.evaluate(() => window.__lf.ui.openSettings());
  await section.waitFor();
  await page.waitForFunction(() => document.querySelectorAll('.audio-settings__folder').length === 5);
  console.log('rows', JSON.stringify(await rows()));
  assert.deepEqual(await rows(), listed([OWN, LONG]), 'the built-in folders, then the player\'s own');
  assert.deepEqual(await calls(), ['folders'], 'opening the panel reads the folders, and scans nothing');
  assert.ok(await addButton.isEnabled(), 'Add folder is there to press');
  const fit = await section.evaluate((group) => {
    const panel = group.closest('.audio-settings');
    const width = () => Math.round(panel.getBoundingClientRect().width);
    const long = [...group.querySelectorAll('.audio-settings__folder')].at(-1);
    const path = long.querySelector('.audio-settings__folder-path');
    const button = long.querySelector('button').getBoundingClientRect();
    const box = panel.getBoundingClientRect();
    const shown = { cut: path.scrollWidth > path.clientWidth, panel: width(), inside: button.right <= box.right && button.left >= box.left };
    group.style.display = 'none';
    const without = width();
    group.style.display = '';
    return { ...shown, without };
  });
  console.log('long path', JSON.stringify(fit));
  assert.ok(fit.cut, 'a long path is cut (its title has it whole)');
  assert.ok(fit.inside, 'its remove button stays inside the panel');
  assert.equal(fit.panel, fit.without, 'a long path does not widen the panel');
  await mkdir('logs/plugin-folders', { recursive: true });
  await section.screenshot({ path: 'logs/plugin-folders/section.png' });
  assert.deepEqual(await picker(), ['Probe Amp Sim (vst3)']);

  // ── Add: the host's dialog answers a folder ─────────────────────────────────────────────────────────
  await clearCalls();
  await page.evaluate(({ ADDED, AMP, FOUND }) => (window.__folders.nextAdd = { folder: ADDED, plugins: [AMP, FOUND] }), { ADDED, AMP, FOUND });
  await addButton.click();
  await page.waitForFunction(() => document.querySelectorAll('.audio-settings__folder').length === 6);
  await idle();
  console.log('add', JSON.stringify(await calls()));
  assert.deepEqual(await rows(), listed([OWN, LONG, ADDED]), 'the added folder is listed');
  assert.deepEqual(await calls(), ['add(0)', 'scan:false'], 'the add passes no path, and one scan (not forced) follows');
  assert.deepEqual(await picker(), ['Probe Amp Sim (vst3)', 'Found In Folder (clap)'], 'the picker lists what the scan found there');

  // ── Remove ──────────────────────────────────────────────────────────────────────────────────────────
  await clearCalls();
  await section.getByRole('button', { name: `Remove folder ${ADDED.path}`, exact: true }).click();
  await page.waitForFunction(() => document.querySelectorAll('.audio-settings__folder').length === 5);
  await idle();
  console.log('remove', JSON.stringify(await calls()));
  assert.deepEqual(await rows(), listed([OWN, LONG]), 'the removed folder is gone from the list');
  assert.deepEqual(await calls(), [`remove:${ADDED.path}`, 'scan:false'], 'the remove names the exact path, and one scan follows');
  assert.deepEqual(await picker(), ['Probe Amp Sim (vst3)'], 'the picker drops what was only in that folder');

  // ── Cancel: the dialog answers nothing ──────────────────────────────────────────────────────────────
  await clearCalls();
  await page.evaluate(() => (window.__folders.nextAdd = null));
  await addButton.click();
  await page.waitForFunction(() => window.__folders.calls.length >= 1);
  await idle();
  assert.deepEqual(await calls(), ['add(0)'], 'a cancelled dialog scans nothing');
  assert.deepEqual(await rows(), listed([OWN, LONG]), 'and changes nothing');

  // ── A refused add: a toast beside the logged error ──────────────────────────────────────────────────
  await clearCalls();
  assert.deepEqual(consoleErrors, [], 'no console errors so far');
  await page.evaluate(() => (window.__folders.nextAdd = 'plugin-folders.json: expected value at line 1'));
  await addButton.click();
  const toast = page.locator('.toast', { hasText: 'Could not add the plugin folder' });
  await toast.waitFor();
  await idle();
  console.log('refused', JSON.stringify({ toast: await toast.locator('.toast__body').textContent(), consoleErrors }));
  assert.match(await toast.textContent(), /plugin-folders\.json: expected value/, 'the toast carries the host\'s reason');
  assert.equal(consoleErrors.length, 1, 'the refusal is logged once');
  assert.match(consoleErrors[0], /\[plugin-folders\] Could not add the plugin folder/);
  assert.deepEqual(await calls(), ['add(0)'], 'a refused add scans nothing');
  assert.deepEqual(await rows(), listed([OWN, LONG]), 'and the list stays');

  // ── An add that completes while a scan is still running ─────────────────────────────────────────────
  await clearCalls();
  await page.evaluate(async () => {
    const state = window.__folders;
    state.gate = new Promise((resolve) => (state.release = resolve));
    const instrument = await import('/src/ui/state/instrument.ts');
    state.held = instrument.scanForPlugins();
  });
  await page.waitForFunction(() => window.__folders.calls.includes('scan:false'));
  assert.ok(await addButton.isDisabled(), 'Add folder is disabled while a scan runs');
  // The button is closed, so the add comes in through the state module (as a dialog opened just
  // before the scan started would): the held scan started without the folder.
  const during = await page.evaluate(async ({ LATE_FOLDER, AMP, LATE }) => {
    const state = window.__folders;
    const instrument = await import('/src/ui/state/instrument.ts');
    const folders = await import('/src/ui/state/plugin-folders.ts');
    state.nextAdd = { folder: LATE_FOLDER, plugins: [AMP, LATE] };
    const added = folders.addPluginFolder();
    while (!state.calls.includes('add(0)')) await new Promise((r) => setTimeout(r, 5));
    await new Promise((r) => setTimeout(r, 50));
    const waiting = { calls: [...state.calls], scanning: instrument.scanning(), listed: folders.pluginFolders().user.map((f) => f.path) };
    state.gate = null;
    state.release();
    await Promise.all([state.held, added]);
    return { waiting, calls: [...state.calls], scanning: instrument.scanning(), found: instrument.availablePlugins().map((p) => p.name) };
  }, { LATE_FOLDER, AMP, LATE });
  console.log('add during a scan', JSON.stringify(during));
  assert.deepEqual(during.waiting.calls, ['scan:false', 'add(0)'], 'no second scan starts while the first runs');
  assert.ok(during.waiting.scanning);
  assert.ok(during.waiting.listed.includes(LATE_FOLDER.path), 'the folder is listed as soon as the host answers');
  assert.deepEqual(during.calls, ['scan:false', 'add(0)', 'scan:false'], 'ONE follow-up scan runs once the held one ends');
  assert.equal(during.scanning, false);
  assert.deepEqual(during.found, ['Probe Amp Sim', 'Late Arrival'], 'the result is the scan that started after the add');
  assert.deepEqual(await picker(), ['Probe Amp Sim (vst3)', 'Late Arrival (vst3)'], 'and the picker shows it');
  assert.deepEqual(await rows(), listed([OWN, LONG, LATE_FOLDER]));
  assert.ok(await addButton.isEnabled(), 'Add folder opens again after the scans');

  // ── A read the host answers late never lands on the list a later change published ────────────────
  const stale = await page.evaluate(async ({ STALE_FOLDER }) => {
    const state = window.__folders;
    const { platform } = await import('/src/platform/index.ts');
    const folders = await import('/src/ui/state/plugin-folders.ts');
    const read = platform.pluginHost.pluginFolders;
    let answer;
    platform.pluginHost.pluginFolders = async () => {
      const old = state.lists; // what the list was when the read was made
      await new Promise((resolve) => (answer = resolve));
      return old;
    };
    const reading = folders.refreshPluginFolders();
    state.nextAdd = { folder: STALE_FOLDER, plugins: state.plugins };
    await folders.addPluginFolder();
    answer();
    await reading;
    platform.pluginHost.pluginFolders = read;
    return folders.pluginFolders().user.map((f) => f.path);
  }, { STALE_FOLDER });
  console.log('stale read', JSON.stringify(stale));
  assert.ok(stale.includes(STALE_FOLDER.path), 'the folder just added stays listed when an older read answers after it');
  assert.deepEqual(await rows(), listed([OWN, LONG, LATE_FOLDER, STALE_FOLDER]));

  // Several requests during one scan still make one follow-up, forced if any of them was.
  await clearCalls();
  const burst = await page.evaluate(async () => {
    const state = window.__folders;
    const instrument = await import('/src/ui/state/instrument.ts');
    state.gate = new Promise((resolve) => (state.release = resolve));
    const first = instrument.scanForPlugins();
    const more = [instrument.scanForPlugins(), instrument.scanForPlugins({ force: true }), instrument.scanForPlugins()];
    state.gate = null;
    state.release();
    await Promise.all([first, ...more]);
    return [...state.calls];
  });
  console.log('burst', JSON.stringify(burst));
  assert.deepEqual(burst, ['scan:false', 'scan:true'], 'three requests during a scan: one follow-up, forced');

  assert.equal(consoleErrors.length, 1, 'no console errors beyond the refused add');
});
