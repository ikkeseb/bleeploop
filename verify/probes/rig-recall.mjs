/**
 * Rig recall across launches (`src/ui/state/rig-recall.ts`), on the web engine fake
 * (`src/platform/host.web.ts`, the engine-seam pattern): each launch reloads the page, whose mount runs
 * the production boot chain (`bootEngine`) over a substituted plugin host (installed before any app
 * module runs) and the real close guard with only its native close capabilities substituted, so the
 * record and the in-flight marker live in the page's real localStorage between launches. GO LIVE is the
 * engine's `SetSlotLive`, read from `__lf.native.sent`. Proves: two plugins picked in the slot dropdowns
 * (the effect's automatic GO LIVE included) come back at the next launch through the load path, slot 0
 * then slot 1, without GO LIVE or an editor call and with GO LIVE unpressed, the restored instrument
 * taking the MIDI slot; an unloaded slot stays empty; a slot whose plugin is missing from the scan is
 * skipped with one `[rig-recall]` log line and no toast, and returns once the plugin is back; a load
 * that fails at recall shows one toast and is not retried, unless the host failed it, not the plugin
 * (the slot still clearing, the setup timed out, no owner thread, the command's task failed): then the
 * next launch retries it; a pick made while slot 0 restores wins slot 1; a launch stopped during a hanging load (a close through the close button included), or after the
 * loads but inside the settle window, makes the next launch skip the recall with one log line and one
 * toast and forget the record, so the launch after that is clean; a close through the close button once
 * the loads are back keeps the rig for the next launch; a synth picked, or a slot activated, while the
 * scan still runs keeps its slot and the MIDI slot, and the synth's slot stays a synth at the launch
 * after; the recall runs on the scan of the saved plugins' own paths (`scanPluginPaths`), so a saved
 * plugin loads while the full scan still runs, and a failed scan of the paths recalls after the full
 * scan instead. The native load, the scan, the OS close and a real crash are out of reach here:
 * `pnpm native:recall` covers them on the PC.
 * Run: pnpm probe rig-recall
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

const FX = { id: 'probe.fx', name: 'Probe Amp', format: 'clap', path: 'C:\\probe\\amp.clap', isEffect: true };
const SYN = { id: 'probe.syn', name: 'Probe Synth', format: 'vst3', path: 'C:\\probe\\synth.vst3', isEffect: false };
const ALT = { id: 'probe.alt', name: 'Probe Drive', format: 'vst3', path: 'C:\\probe\\drive.vst3', isEffect: true };
const ALL = [FX, SYN, ALT];
const loadOf = (slot, d) => ({ slot, path: d.path, id: d.id });
const keyOf = (d) => JSON.stringify([d.format, d.path, d.id]);

/**
 * Runs before the app on every load: this launch's substituted host, configured by the `recallProbe`
 * sessionStorage entry `launch()` writes. `loads` maps a slot to how its native load answers:
 * 'resolve' (default), 'reject' (the plugin failed), a key of `hostErrors` (the host failed, in the
 * native host's words, as Tauri rejects: a string), 'hang' (never answers) or 'defer' (answers on
 * `release()`);
 * `holdScan` keeps the full scan running until `releaseScan()`, `holdPaths` the scan of the rig's
 * paths until `releasePaths()`; `failPaths` fails the latter. The scan of paths answers the descriptors
 * of `scan` at those paths.
 */
function installFakeHost() {
  const { scan = [], loads = {}, holdScan = false, holdPaths = false, failPaths = false } = JSON.parse(sessionStorage.getItem('recallProbe') ?? '{}');
  const calls = { load: [], editor: [], paths: [] };
  const pending = [];
  const hostErrors = {
    busy: (slot) => `slot ${slot} is still clearing the plugin before it`,
    timeout: () => 'plugin setup timed out: timed out waiting on channel',
    'no-thread': () => 'spawn owner thread: The paging file is too small for this operation to complete. (os error 1455)',
    task: () => 'plugin_load task: task 41 panicked',
  };
  let releaseScan = () => {};
  let releasePaths = () => {};
  const scanned = holdScan ? new Promise((resolve) => (releaseScan = () => resolve(scan))) : Promise.resolve(scan);
  window.__recallProbe = { calls, release: () => pending.shift()?.(), releaseScan: () => releaseScan(), releasePaths: () => releasePaths() };
  window.__recallHost = {
    available: true,
    scanPlugins: () => scanned,
    scanPluginPaths: (paths) => {
      calls.paths.push([...paths]);
      if (failPaths) return Promise.reject('plugin scan task: injected failure');
      const found = scan.filter((d) => paths.includes(d.path));
      return holdPaths ? new Promise((resolve) => (releasePaths = () => resolve(found))) : Promise.resolve(found);
    },
    loadPlugin: (slot, path, id) => {
      calls.load.push({ slot, path, id });
      const reply = { slot, descriptor: scan.find((d) => d.path === path && d.id === id) };
      const how = loads[slot] ?? 'resolve';
      if (how === 'reject') return Promise.reject(new Error('injected load failure'));
      if (how in hostErrors) return Promise.reject(hostErrors[how](slot));
      if (how === 'hang') return new Promise(() => {});
      if (how === 'defer') return new Promise((resolve) => pending.push(() => resolve(reply)));
      return Promise.resolve(reply);
    },
    unloadPlugin: async () => {},
    listParams: async () => [],
    openEditor: async (slot) => { calls.editor.push(slot); },
  };
}

await probe(async ({ open }) => {
  const app = await open({
    viewport: { width: 1280, height: 820 },
    init: (p) => Promise.all([
      p.addInitScript(() => void (window.__lfEngineFake = true)),
      p.addInitScript(installFakeHost),
      p.route('**/src/platform/host.web.ts', async (route) => {
        const response = await route.fetch();
        const body = (await response.text()) + '\nObject.assign(webPluginHost, window.__recallHost ?? {});\n';
        await route.fulfill({ response, body });
      }),
      // The real close guard on its native path, as in recovery-close.mjs: the OS close request is a
      // call to `window.__closeRequest`, an approved close counts in `window.__closeApprovals`.
      p.route('**/src/app/close-guard.ts*', async (route) => {
        const response = await route.fetch();
        const source = await response.text();
        const capabilities = /import\s*\{[^}]*confirmNativeClose[^}]*\}\s*from\s*["'][^"']+["'];?/;
        if (!capabilities.test(source)) throw new Error('Cannot locate close-guard platform import');
        await route.fulfill({ response, body: source.replace(capabilities, `
          const platform = { kind: 'tauri' };
          const onNativeCloseRequested = (callback) => { window.__closeRequest = callback; };
          const confirmNativeClose = async () => { window.__closeApprovals = (window.__closeApprovals ?? 0) + 1; };
        `) });
      }),
    ]),
  });
  const { page } = app;

  /** One app launch: reload with this launch's host answers; the app's mount runs the boot chain. */
  async function launch(cfg) {
    await page.evaluate((c) => sessionStorage.setItem('recallProbe', JSON.stringify(c)), cfg);
    await page.reload();
    await page.waitForFunction(() => '__lf' in window && !!window.__closeRequest);
    await page.evaluate(async () => {
      Object.assign(window.__recallProbe, {
        instrument: await import('/src/ui/state/instrument.ts'),
        slots: await import('/src/ui/state/instrument-slots.ts'),
        recall: await import('/src/ui/state/rig-recall.ts'),
      });
    });
  }

  /**
   * Wait for this launch's recall to finish, then read what it did. Its `[rig-recall]` lines must match
   * `recall` one for one, in order, and exactly `loadFailed` plugin-load failure lines must be logged:
   * any other line of either kind fails here, and a line of any other kind stays for the harness.
   */
  async function settle({ recall = [], loadFailed = 0 } = {}) {
    await page.waitForFunction(() => window.__recallProbe.recall.rigRecallDone(), undefined, { timeout: 20_000 });
    const state = await page.evaluate(async () => {
      const { calls, instrument, recall } = window.__recallProbe;
      const io = await import('/src/ui/state/native-io.ts');
      const { pluginDescriptorKey } = await import('/src/ui/state/plugin-descriptor.ts');
      return {
        loads: calls.load,
        arms: window.__lf.native.sent.filter((c) => c.SetSlotLive?.[1] === true).map((c) => `live ${c.SetSlotLive[0]}`),
        editors: calls.editor,
        slots: instrument.slotPlugins().map((d) => (d ? pluginDescriptorKey(d) : null)),
        synths: instrument.slotIds(),
        active: instrument.activeSlot(),
        armed: [...io.inputArmed()],
        live: [...document.querySelectorAll('.slot')].map((el) => el.querySelector('.tgl.live')?.textContent?.trim() ?? null),
        inFlight: recall.recallInFlight(),
        toasts: window.__lf.notify.toasts().map((t) => `${t.message} x${t.count}`),
      };
    });
    const recallLogs = app.consumeConsoleErrors(/^\[rig-recall\]/);
    const loadFailedLogs = app.consumeConsoleErrors(/^\[instrument\] plugin load failed/);
    console.log(JSON.stringify({ ...state, recallLogs, loadFailedLogs }));
    assert.equal(recallLogs.length, recall.length, `[rig-recall] lines: ${JSON.stringify(recallLogs)}`);
    recall.forEach((re, i) => assert.match(recallLogs[i], re));
    assert.equal(loadFailedLogs.length, loadFailed, `plugin load failure lines: ${JSON.stringify(loadFailedLogs)}`);
    return state;
  }

  const pick = async (slot, desc) => {
    // No plugin: the slot's default synth (the picker's built-in entry).
    await page.getByRole('combobox', { name: `Source for slot ${'AB'[slot]}`, exact: true }).selectOption(desc ? keyOf(desc) : ['lead', 'bass'][slot]);
    await page.waitForFunction(() => window.__recallProbe.slots.slotPendingCounts().every((n) => n === 0));
  };
  /** The close button, as the OS sends it: the close guard runs and approves (an empty jam asks nothing). */
  const closeApp = async () => {
    await page.evaluate(() => window.__closeRequest());
    await page.waitForFunction(() => window.__closeApprovals === 1);
    return page.evaluate(() => ({ inFlight: window.__recallProbe.recall.recallInFlight(), done: window.__recallProbe.recall.rigRecallDone() }));
  };
  const expectIdle = (s, what) => assert.deepEqual(
    { arms: s.arms, editors: s.editors, armed: s.armed, inFlight: s.inFlight },
    { arms: [], editors: [], armed: [false, false], inFlight: false },
    `${what}: nothing armed, no editor, no marker`,
  );

  // 1. First launch: nothing remembered. The owner picks the amp (its automatic GO LIVE and editor
  // run, as on the rig) and a synth.
  await launch({ scan: ALL });
  let s = await settle();
  assert.deepEqual(s.loads, [], 'a first launch restores nothing');
  await pick(0, FX);
  await pick(1, SYN);
  await page.waitForFunction(() => window.__recallProbe.calls.editor.length === 2);
  const picked = await page.evaluate(() => window.__lf.native.sent.filter((c) => c.SetSlotLive?.[1] === true));
  assert.deepEqual(picked, [{ SetSlotLive: [0, true] }], 'the dropdown pick of an effect goes live on its own (baseline)');

  // 2. Restart: both come back in slot order, nothing armed, GO LIVE unpressed.
  await launch({ scan: ALL });
  s = await settle();
  assert.deepEqual(s.loads, [loadOf(0, FX), loadOf(1, SYN)]);
  assert.deepEqual(s.slots, [keyOf(FX), keyOf(SYN)]);
  expectIdle(s, 'restored rig');
  // An instrument plugin takes no input, so slot 1 shows no GO LIVE cap at all (`slotTakesInput`).
  assert.deepEqual(s.live, ['GO LIVE', null], 'the restored effect\'s GO LIVE is unpressed');
  assert.deepEqual(s.toasts, []);
  assert.equal(s.active, 1, 'the restored instrument takes the MIDI slot when the player chose nothing');

  // 3. The owner unloads slot 1: the next launch restores slot 0 only.
  await pick(1, null);
  await launch({ scan: ALL });
  s = await settle();
  assert.deepEqual(s.loads, [loadOf(0, FX)], 'an unloaded slot is forgotten');
  assert.deepEqual(s.slots, [keyOf(FX), null]);
  expectIdle(s, 'after an unload');

  // 4. Swap slot 0 to another plugin, then launch with that plugin missing from the scan.
  await pick(0, ALT);
  await launch({ scan: [FX, SYN] });
  // One log line for the missing plugin.
  s = await settle({ recall: [/^\[rig-recall\] slot 1: Probe Drive \(VST3\) at C:\\probe\\drive\.vst3 is not in this launch's plugin scan/] });
  assert.deepEqual(s.loads, [], 'a plugin missing from the scan is not loaded');
  assert.deepEqual(s.toasts, [], 'a missing plugin raises no toast');
  expectIdle(s, 'missing plugin');

  // 5. The plugin is back, but its load fails: one toast, and the next launch does not retry it.
  await launch({ scan: ALL, loads: { 0: 'reject' } });
  s = await settle({ loadFailed: 1 });
  assert.deepEqual(s.loads, [loadOf(0, ALT)], 'a skipped slot keeps its record for a launch where the plugin is back');
  assert.deepEqual(s.toasts, ['Could not load the plugin. Pick it again, or another source. x1']);
  assert.deepEqual(s.slots, [null, null]);
  await launch({ scan: ALL });
  s = await settle();
  assert.deepEqual(s.loads, [], 'a load that failed at recall is not retried');
  assert.deepEqual(s.toasts, []);

  // 6. A pick made while slot 0 is still restoring wins slot 1.
  await pick(0, FX);
  await pick(1, SYN);
  await launch({ scan: ALL, loads: { 0: 'defer' } });
  await page.waitForFunction(() => window.__recallProbe.calls.load.length === 1);
  await page.evaluate((alt) => window.__recallProbe.instrument.selectPlugin(1, alt), ALT);
  await page.evaluate(() => window.__recallProbe.release());
  s = await settle();
  assert.deepEqual(s.loads, [loadOf(0, FX), loadOf(1, ALT)], 'the recall does not load over the pick');
  assert.deepEqual(s.slots, [keyOf(FX), keyOf(ALT)]);

  // 7. Loads the host failed, not the plugin (the slot still clearing, the setup timed out, no owner
  // thread, the command's task failed): a toast each, the rig is kept, and the next launch retries it.
  await launch({ scan: ALL, loads: { 0: 'busy', 1: 'timeout' } });
  s = await settle({ loadFailed: 2 });
  assert.deepEqual(s.loads, [loadOf(0, FX), loadOf(1, ALT)]);
  assert.deepEqual({ slots: s.slots, toasts: s.toasts }, { slots: [null, null], toasts: ['Could not load the plugin. Pick it again, or another source. x2'] });
  await launch({ scan: ALL, loads: { 0: 'no-thread', 1: 'task' } });
  s = await settle({ loadFailed: 2 });
  assert.deepEqual(s.loads, [loadOf(0, FX), loadOf(1, ALT)], 'a busy slot and a timed-out setup are retried at the next launch');
  assert.deepEqual({ slots: s.slots, toasts: s.toasts }, { slots: [null, null], toasts: ['Could not load the plugin. Pick it again, or another source. x2'] });
  await launch({ scan: ALL });
  s = await settle();
  assert.deepEqual(s.loads, [loadOf(0, FX), loadOf(1, ALT)], 'a missing owner thread and a failed task are retried at the next launch');
  assert.deepEqual(s.slots, [keyOf(FX), keyOf(ALT)]);
  expectIdle(s, 'retried after host failures');

  // 8. A launch that dies inside a hanging load, closed through the close button: the marker stays, so
  // the next launch restores nothing, says so once and forgets the rig; the launch after that is clean.
  await launch({ scan: ALL, loads: { 0: 'hang' } });
  await page.waitForFunction(() => window.__recallProbe.calls.load.length === 1 && window.__recallProbe.recall.recallInFlight());
  assert.deepEqual(await closeApp(), { inFlight: true, done: false }, 'a close while a recalled load hangs keeps the marker');
  await launch({ scan: ALL });
  s = await settle({ recall: [/^\[rig-recall\] the last launch stopped while restoring Probe Amp \(CLAP\), Probe Drive \(VST3\)/] });
  assert.deepEqual(s.loads, [], 'no recall after a launch died restoring');
  assert.deepEqual(s.toasts, ['Could not restore the plugins. Pick a plugin again to load it. x1']);
  expectIdle(s, 'skipped recall');
  await launch({ scan: ALL });
  s = await settle();
  assert.deepEqual({ loads: s.loads, toasts: s.toasts }, { loads: [], toasts: [] }, 'the launch after a skipped recall is clean');

  // 9. The same when the loads finished but the launch died inside the settle window.
  await pick(0, FX);
  await pick(1, SYN);
  await launch({ scan: ALL });
  await page.waitForFunction(() => {
    const { instrument, recall } = window.__recallProbe;
    return instrument.slotPlugins().every(Boolean) && recall.recallInFlight() && !recall.rigRecallDone();
  });
  await launch({ scan: ALL });
  s = await settle({ recall: [/^\[rig-recall\] the last launch stopped while restoring Probe Amp \(CLAP\), Probe Synth \(VST3\)/] });
  assert.deepEqual(s.loads, [], 'a launch that died right after its loads also skips the next recall');
  assert.deepEqual(s.toasts, ['Could not restore the plugins. Pick a plugin again to load it. x1']);
  await launch({ scan: ALL });
  s = await settle();
  assert.deepEqual({ loads: s.loads, toasts: s.toasts }, { loads: [], toasts: [] });

  // 10. A close through the close button once the loads are back, inside the settle window: the marker
  // goes with the close, and the next launch restores the rig without a toast.
  await pick(0, FX);
  await pick(1, SYN);
  await launch({ scan: ALL });
  await page.waitForFunction(() => {
    const { instrument, recall } = window.__recallProbe;
    return instrument.slotPlugins().every(Boolean) && recall.recallInFlight() && !recall.rigRecallDone();
  });
  assert.deepEqual(await closeApp(), { inFlight: false, done: false }, 'a clean close inside the settle window clears the marker');
  await launch({ scan: ALL });
  s = await settle();
  assert.deepEqual(s.loads, [loadOf(0, FX), loadOf(1, SYN)], 'a close right after a restore keeps the rig');
  assert.deepEqual(s.toasts, []);

  // 11. A synth picked while the scan of the rig's paths still runs: its slot keeps the synth and the
  // MIDI slot stays with it; the other slot's instrument is restored without taking MIDI. The next
  // launch restores only that instrument, which then takes MIDI again.
  await launch({ scan: ALL, holdPaths: true });
  await page.getByRole('combobox', { name: 'Source for slot A', exact: true }).selectOption('pad');
  await page.waitForFunction(() => window.__recallProbe.slots.slotPendingCounts().every((n) => n === 0));
  await page.evaluate(() => window.__recallProbe.releasePaths());
  s = await settle();
  assert.deepEqual(s.loads, [loadOf(1, SYN)], 'the recall does not load over a synth picked during boot');
  assert.deepEqual({ slots: s.slots, synth: s.synths[0], active: s.active }, { slots: [null, keyOf(SYN)], synth: 'pad', active: 0 },
    'the picked synth keeps its slot and the MIDI slot');
  expectIdle(s, 'synth picked during boot');
  await launch({ scan: ALL });
  s = await settle();
  assert.deepEqual({ loads: s.loads, active: s.active }, { loads: [loadOf(1, SYN)], active: 1 },
    'a slot the player turned into a synth stays one at the next launch');

  // 12. A slot activated while the scan of the rig's paths still runs keeps the MIDI slot over a
  // restored instrument.
  await launch({ scan: ALL, holdPaths: true });
  await page.getByRole('button', { name: 'Activate slot A' }).click();
  await page.evaluate(() => window.__recallProbe.releasePaths());
  s = await settle();
  assert.deepEqual({ loads: s.loads, active: s.active }, { loads: [loadOf(1, SYN)], active: 0 },
    'a restored instrument does not move a MIDI slot the player chose');

  // 13. The full scan held (a new plugin's scan child running out its 20 s): the saved plugin loads
  // from the scan of its own path before the full scan answers.
  await launch({ scan: ALL, holdScan: true });
  await page.waitForFunction(() => window.__recallProbe.instrument.slotPlugins()[1] !== null);
  const early = await page.evaluate(() => ({ paths: window.__recallProbe.calls.paths, picker: window.__recallProbe.instrument.availablePlugins().length }));
  assert.deepEqual(early, { paths: [[SYN.path]], picker: 0 }, "the rig's plugin is in before the full scan answered; only its path was asked for");
  await page.evaluate(() => window.__recallProbe.releaseScan());
  s = await settle();
  assert.deepEqual(s.loads, [loadOf(1, SYN)], 'loaded once');

  // 14. The scan of the paths fails: the recall waits for the full scan and restores from it.
  await launch({ scan: ALL, failPaths: true });
  s = await settle();
  assert.deepEqual(s.loads, [loadOf(1, SYN)], 'restored after the full scan');
  assert.equal(app.consumeConsoleErrors(/^\[app\] rig plugin scan failed/).length, 1, 'the failed scan of the paths is logged once');
  assert.deepEqual(s.toasts, [], 'no toast: the recall still ran');
});
