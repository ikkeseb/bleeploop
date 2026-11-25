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
 * scan instead; one plugin file remembered in both slots comes back with its editor owned by slot A,
 * whose load lands before slot B's starts (B's EDITOR is refused, A's opens), and the same file picked
 * in slot B while slot A's recalled load is still in flight refuses both slots' EDITOR, since which load
 * reached the module first is the host's.
 *
 * Chain mode (DEV; `docs/unit-contract.md` §1, §6): the handover to the native rig record (H cases), in a
 * browser context of its own with the chain fake (`__lfChainFake`) and the rig fake (`__lfRigFake`, scripted
 * per launch as that launch's process holds the record) on, over the same substituted plugin host, on a
 * stored v0.8.9 profile (the amp-sim in slot A at a saved level, a pad in slot B, RING MOD and ECHO on, REVERB
 * off at a changed level, no OTT key); its replies go through the page's strict reading of D13's answer, a
 * scripted raw reply included, and the Tauri adapter's source is checked to read both answers so. Proves:
 * on `absent` one import, equal to the profile's upgrade (written out here), after the device's open answered
 * and `host_init`, and no add or slot level from the page; `present`, `unreadable` and `unavailable` import
 * nothing, the last two with one log line; a reload opens again and imports nothing; the fader shows the reset
 * frame's slot gain over open's volumes, and the upgrade's level (the plugin's kind unknown before the scan,
 * known after it) until the import answers; a gesture is one plain `SetSlotGain` that outranks a later older
 * reset frame and the import's answer, and stays shown when refused, whole or partly, going again alone until
 * taken; a notice is one toast and one log line per page, the toast before `seen`, shown again by a reload
 * before `seen` answered; a refused or stale import, a refused, unanswered (2 s, no device before) or invalid
 * open, and an import answered other than `present` are one log line each, the device opening anyway, and a
 * late open answer still brings its levels and notice; `lf.rig` and `lf.rigInFlight` are deleted before the
 * open, and no device opens before the open settled, one Audio Settings or an ASIO driver switch asks for
 * included; a refused or partly refused reset send-off goes again in batches of its own until taken (an owed
 * blur never among them), while other commands queue, one log line at 10 s; a device that stops during the
 * scan or the bindings wait gets no import, and an import with no answer in 5 s ends the handover (a late
 * answer still shows its levels and notice, a late rejection is one log line).
 *
 * The native load, the scan, the OS close and a real crash are out of reach here: `pnpm native:recall`
 * covers them on the PC.
 * Run: pnpm probe rig-recall
 */
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
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

/**
 * Chain mode's per-launch seams, run before the app on every load of the chain phase's page (its config the
 * `chainProbe` sessionStorage entry `chainLaunch()` writes): the chain fake on; a one-shot `chainSeed`
 * replacing the whole localStorage (a fresh profile); `rig` the rig fake's script (`__lfRigFake`); `refuse`
 * (engine command names) refuses every batch carrying one of them, as a full command ring would, until
 * `window.__refuse` is cleared, and `refusePartly` takes the rest of such a batch and answers that it dropped
 * some, until `window.__refusePartly` is cleared; `sendDelay` makes every batch take that many ms (the chain
 * context's route of `host.web.ts` gates `InputHost.send`); `holdDevice` holds each device open's answer.
 * `install(chain)`, called once `__lf` is up: `mint` counts from `mintFrom`, and every chain add is logged
 * (the page makes none: the restore is native's).
 */
function installChainProbe() {
  window.__lfChainFake = true;
  const cfg = JSON.parse(sessionStorage.getItem('chainProbe') ?? '{}');
  const seed = sessionStorage.getItem('chainSeed');
  if (seed !== null) {
    sessionStorage.removeItem('chainSeed');
    localStorage.clear();
    for (const [k, v] of Object.entries(JSON.parse(seed))) localStorage.setItem(k, v);
  }
  window.__refuse = cfg.refuse ?? null;
  window.__refusePartly = cfg.refusePartly ?? null;
  window.__sendDelay = cfg.sendDelay ?? 0;
  // `holdDevice`: every device open's answer waits until `__releaseDevice()` (the device itself runs at once).
  if (cfg.holdDevice) window.__deviceHold = new Promise((resolve) => (window.__releaseDevice = resolve));
  // The native record's handover on (`host.web.ts` `RigFake`), scripted as this launch's process holds it.
  if (cfg.rig) window.__lfRigFake = cfg.rig;
  const probe = { adds: [], minted: [] };
  probe.install = (chain) => {
    let next = cfg.mintFrom ?? 1001;
    chain.mint = () => {
      probe.minted.push(next);
      return next++;
    };
    const real = chain.host.add;
    chain.host.add = (slot, unit, spec, at, toneToken) => {
      probe.adds.push({ slot, unit });
      return real(slot, unit, spec, at, toneToken);
    };
  };
  window.__chainProbe = probe;
}

await probe(async ({ browser, open }) => {
  const app = await open({
    // 1920: both slots holding a live plugin show both lines whole (narrower, the slot row folds one
    // slot to a plate, `src/ui/instrument/row-fit.ts`), so both slots' EDITOR stay on screen
    viewport: { width: 1920, height: 1080 },
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
    await page.waitForFunction(() => window.__recallProbe.recall.rigRecallFinished(), undefined, { timeout: 20_000 });
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
        live: [...document.querySelectorAll('.slot')].map((el) => { const k = el.querySelector('.slot__live'); return k ? (k.getAttribute('aria-pressed') === 'true' ? 'live' : 'off') : null; }),
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
    return page.evaluate(() => ({ inFlight: window.__recallProbe.recall.recallInFlight(), done: window.__recallProbe.recall.rigRecallFinished() }));
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
  assert.deepEqual(s.live, ['off', null], 'the restored effect\'s LIVE is unpressed');
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
    return instrument.slotPlugins().every(Boolean) && recall.recallInFlight() && !recall.rigRecallFinished();
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
    return instrument.slotPlugins().every(Boolean) && recall.recallInFlight() && !recall.rigRecallFinished();
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

  // 15. One plugin file remembered in both slots (D29): the recall loads slot A, then slot B once A's
  // load landed, so A's load came first and owns the file's editor. B's EDITOR, pressed first, is
  // refused and reaches no host call; A's opens.
  const editor = (letter, pressed) => page.getByRole('button', { name: `Editor for slot ${letter}`, exact: true, ...(pressed === undefined ? {} : { pressed }) });
  const refused = async (letter) => {
    await page.evaluate(() => window.__lf.notify.toasts().forEach((t) => window.__lf.notify.dismissToast(t.id)));
    await editor(letter, false).click();
    await page.locator('.toast', { hasText: 'Could not open a second editor for this plugin.' }).waitFor({ timeout: 5000 });
    assert.equal(await editor(letter).getAttribute('aria-pressed'), 'false', `slot ${letter}'s EDITOR is refused`);
  };
  await page.evaluate(async (fx) => {
    const { rememberSlotPlugin } = await import('/src/ui/state/rig-recall.ts');
    rememberSlotPlugin(0, fx);
    rememberSlotPlugin(1, fx);
  }, FX);
  await launch({ scan: ALL, loads: { 0: 'defer' } });
  await page.waitForFunction(() => window.__recallProbe.calls.load.length === 1);
  await page.waitForTimeout(200);
  assert.deepEqual(await page.evaluate(() => window.__recallProbe.calls.load), [loadOf(0, FX)], "slot B's recalled load waits for slot A's to land");
  await page.evaluate(() => window.__recallProbe.release());
  s = await settle();
  assert.deepEqual({ loads: s.loads, slots: s.slots }, { loads: [loadOf(0, FX), loadOf(1, FX)], slots: [keyOf(FX), keyOf(FX)] });
  await refused('B');
  await editor('A', false).click();
  await editor('A', true).waitFor();
  assert.deepEqual(await page.evaluate(() => window.__recallProbe.calls.editor), [0], "only slot A's editor reached the host");

  // 16. The same file picked in slot B while slot A's recalled load is in flight: two loads of one file
  // overlapping, which the frontend cannot order, so both slots are refused.
  await page.evaluate(async () => {
    const { forgetSlotPlugin } = await import('/src/ui/state/rig-recall.ts');
    forgetSlotPlugin(1);
  });
  await launch({ scan: ALL, loads: { 0: 'defer' } });
  await page.waitForFunction(() => window.__recallProbe.calls.load.length === 1);
  await page.evaluate((fx) => {
    window.__pickB = window.__recallProbe.instrument.selectPlugin(1, fx);
  }, FX);
  await page.waitForFunction(() => window.__recallProbe.calls.load.length === 2);
  await page.evaluate(async () => {
    window.__recallProbe.release();
    await window.__pickB;
  });
  s = await settle();
  assert.deepEqual(s.slots, [keyOf(FX), keyOf(FX)], 'both slots hold the file');
  await refused('A');
  await refused('B');
  assert.deepEqual(await page.evaluate(() => window.__recallProbe.calls.editor), [], 'no editor open reached the host');

  // ── Chain mode (DEV): the handover to the native rig record (D12, D13; `rig-recall.ts` `openRig`, `importRig`) ──
  const chainContext = await browser.newContext();
  const chainApp = await open({
    context: chainContext,
    viewport: { width: 1920, height: 1080 },
    init: (p) => Promise.all([
      p.addInitScript(() => void (window.__lfEngineFake = true)),
      p.addInitScript(installFakeHost),
      p.addInitScript(installChainProbe),
      p.route('**/src/platform/host.web.ts', async (route) => {
        const response = await route.fetch();
        const body = (await response.text())
          + '\nObject.assign(webPluginHost, window.__recallHost ?? {});\n'
          // `refuse`: a batch carrying a refused command (named `Command`, or `Command:arg` by its first
          // argument) is rejected whole before the fake takes any of it; `refusePartly`: the fake takes the rest
          // of such a batch and answers that it dropped some (`full`);
          // `sendDelay`: every batch takes that many ms (a busy native call, so later sends queue behind it).
          + '\n{ const take = webInput.send; webInput.send = async (epoch, items) => {'
          + ' if (window.__sendDelay) await new Promise((resolve) => setTimeout(resolve, window.__sendDelay));'
          + ' const named = (list) => (it) => { if (!(\'engine\' in it) || typeof it.engine !== \'object\') return false;'
          + ' const [name] = Object.keys(it.engine); return list.includes(name) || list.includes(`${name}:${it.engine[name][0]}`); };'
          + ' const refused = window.__refuse; const partly = window.__refusePartly;'
          + ' if (refused && items.map(decodeInputItem).some(named(refused)))'
          + ' { (window.__refusedBatches ??= []).push(items.map(decodeInputItem));'
          + ' throw new Error(\'injected: the command ring is full\'); }'
          + ' if (partly && items.map(decodeInputItem).some(named(partly)))'
          + ' { (window.__refusedBatches ??= []).push(items.map(decodeInputItem));'
          + ' await take(epoch, items.filter((item) => !named(partly)(decodeInputItem(item)))); return \'full\'; }'
          + ' return take(epoch, items); }; }\n'
          // A device open counted when it answers (`__devicesAnswered`), its answer held by `holdDevice`.
          + '\n{ const open = webEngineFake.open; webEngineFake.open = async (...args) => {'
          + ' const status = await open.apply(webEngineFake, args); await window.__deviceHold;'
          + ' window.__devicesAnswered = (window.__devicesAnswered ?? 0) + 1; return status; }; }\n'
          // `host_init` (the plugin host's `init`), counted.
          + '\n{ const init = webPluginHost.init; webPluginHost.init = async () => { await init.call(webPluginHost); window.__hostInits = (window.__hostInits ?? 0) + 1; }; }\n'
          // The native record's calls (`__lfRigFake`): what the page had kept of the WebView's record and how many
          // devices it had opened when it called open, and how many when the open settled; the devices and the
          // host inits at the import; the toasts at `seen`.
          + '\n{ const rig = webEngineFake.rig; const open = rig.open; rig.open = () => {'
          + ' window.__rigOpenSaw = { rig: localStorage.getItem(\'lf.rig\'), inFlight: localStorage.getItem(\'lf.rigInFlight\'), devices: webEngineFake.opened.length, at: performance.now() };'
          + ' const answer = open(); const settled = () => void (window.__rigOpenSettled = webEngineFake.opened.length);'
          + ' answer.then(settled, settled); return answer; };'
          + ' const imp = rig.import; rig.import = (text) => {'
          + ' window.__rigImportSaw = { devices: window.__devicesAnswered ?? 0, hostInits: window.__hostInits ?? 0, at: performance.now() };'
          + ' return imp(text); };'
          + ' const seen = rig.seen; rig.seen = (id) => {'
          + ' window.__rigSeenSaw = window.__lf?.notify.toasts().length ?? null;'
          + ' return seen(id); }; }\n';
        await route.fulfill({ response, body });
      }),
    ]),
  });
  const cp = chainApp.page;

  // The chain fake's canned plugins (`src/platform/chain-fake.ts` CANNED_PLUGINS): what it can activate.
  const AMP = { id: 'fake.amp', name: 'Fake Amp', format: 'vst3', path: 'C:\\Fake\\FakeAmp.vst3', isEffect: true };
  const FSYN = { id: 'fake.synth', name: 'Fake Synth', format: 'clap', path: 'C:\\Fake\\FakeSynth.clap', isEffect: false };
  const LOOK = { id: 'fake.lookahead', name: 'Fake Lookahead', format: 'vst2', path: 'C:\\Fake\\FakeLookahead.dll', isEffect: true };
  const CANNED = [AMP, FSYN, LOOK];
  const saved = ({ format, path, id, name }) => ({ format, path, id, name });
  /** A v0.8.9 profile, each key as v0.8.9 writes it: the amp-sim in slot A at a saved level, a pad in slot B,
   * RING MOD and ECHO on, REVERB off at a changed level, no OTT key (v0.8.9 has none). */
  const V089 = {
    'lf.rigRecall': JSON.stringify([saved(AMP), null]),
    [`lf.pluginGain.0.${keyOf(AMP)}`]: '0.8',
    'lf.slotSource.1': 'pad',
    'lf.synthGain.pad': '0.6',
    'lf.inputSend.ring': '1',
    'lf.inputSend.ringFreq': '220',
    'lf.inputSend.ringLevel': '0.3',
    'lf.inputSend.echo': '1',
    'lf.inputSend.echoTime': '2',
    'lf.inputSend.echoFeedback': '0.55',
    'lf.inputSend.echoLevel': '0.35',
    'lf.inputSend.reverb': '0',
    'lf.inputSend.reverbLevel': '0.6',
  };
  /** Native MIDI's first `bindings` report: one REC/DUB pedal, no IN FX pedal (so an OTT off at its
   * defaults is not migrated; the fake's subscribe sends none). */
  const BINDINGS = { bindings: { revision: 1, bindings: [{
    binding: { portId: 'winmm:0:FS-6 Pedal', portName: 'FS-6 Pedal', channel: 0, kind: 'cc', number: 64, action: 'recDub', target: null, pressHigh: true, momentary: true, hold: false },
    origin: 'native', ordinal: false, blocked: null, displayName: 'FS-6 Pedal', state: 'live',
  }] } };
  const emptyLane = { state: 'Empty', length: 0, armed: false, autoArmed: false, canUndo: false, canReverse: false, reversed: false, stopAt: null, fading: false, retakePass: 0 };
  /** A fresh engine's reset frame; `chains` only for a WebView reload (the native host kept them), `settings`
   * what the engine replays (a slot gain it kept). */
  const resetFrame = (chains, settings = []) => ({
    seq: 1,
    reset: true,
    settings,
    events: [{ Transport: { frame: 0, master: 0, bpm: 120, locked: false } }, ...[0, 1, 2, 3, 4].map((lane) => ({ Lane: { frame: 0, lane, info: emptyLane } })), { Selected: { frame: 0, lane: 0 } }],
    anchor: { frame: 0, atMs: Date.now(), rate: 48000, grid: 0 },
    meter: { peak: [0, 0], clip: [false, false] },
    ...(chains ? { chains } : {}),
  });

  /**
   * One chain-mode launch: reload with this launch's seams (`installChainProbe`; `seed` replaces the whole
   * storage first), wait for the engine and the MIDI subscription, install the seams, then the first
   * `bindings` report (unless `bindings` is false: the probe emits it later) and the reset frame (`chains`: a
   * WebView reload's; `settings`: what it replays); `holdPaths` holds the restore's scan of its plugins' paths
   * until `window.__recallProbe.releasePaths()`. `whileBoot` runs right after the reload, before the wait for
   * `devices` device opens (the boot's one by default).
   */
  async function chainLaunch({ scan = CANNED, seed = null, chains = null, settings = [], bindings = true, holdPaths = false, whileBoot = null, devices = 1, ...cfg } = {}) {
    await cp.evaluate(([r, c, sd]) => {
      sessionStorage.setItem('recallProbe', JSON.stringify(r));
      sessionStorage.setItem('chainProbe', JSON.stringify(c));
      if (sd) sessionStorage.setItem('chainSeed', JSON.stringify(sd));
    }, [{ scan, holdPaths }, cfg, seed]);
    await cp.reload();
    await whileBoot?.();
    await cp.waitForFunction(
      (n) => '__lf' in window && window.__lf.native.opened.length === n && window.__lf.native.midiCalls.some(([c]) => c === 'subscribe'),
      devices,
      { timeout: 30_000 },
    );
    await cp.evaluate(async () => {
      window.__chainProbe.install(window.__lf.native.chain);
      window.__chainProbe.recall = await import('/src/ui/state/rig-recall.ts');
    });
    if (bindings) await cp.evaluate((b) => window.__lf.native.midiEmit(b), BINDINGS);
    await cp.evaluate((f) => window.__lf.native.emit(f), resetFrame(chains, settings));
  }

  /** The upgrade of V089 as the import hands it over: ids minted from 1001 (slot A's units, its pending, then
   * slot B's), the sends' params as stored, REVERB bypassed, no OTT (off at its defaults and unbound). */
  const SYNTH_LEVELS = { gainLead: 1, gainBass: 1, gainPad: 0.6, gainPiano: 1, gainOrgan: 1, gainDrum: 1 };
  const UPGRADED_V089 = { version: 1, slots: [
    { volume: 0.8, units: [{ id: 1001, kind: 'plugin', plugin: saved(AMP), bypass: false }], pending: [
      { id: 1002, kind: 'ring', params: { ringFreq: 220, ringLevel: 0.3 }, bypass: false },
      { id: 1003, kind: 'echo', params: { echoTime: 2, echoFeedback: 0.55, echoLevel: 0.35 }, bypass: false },
      { id: 1004, kind: 'reverb', params: { reverbLevel: 0.6 }, bypass: true },
    ] },
    { volume: 1, units: [{ id: 1005, kind: 'synth', params: { instrument: 2, ...SYNTH_LEVELS }, bypass: false }] },
  ] };
  /** A WebView chain record and its crash marker from before the native record (D12 deletes both). */
  const OLD_DEV_RIG = JSON.stringify({ version: 1, slots: [{ volume: 1, units: [{ id: 1001, kind: 'plugin', plugin: saved(AMP), bypass: false }] }, { volume: 1, units: [] }] });

  // ── The native record's handover (D12, D13; `rig-recall.ts` `openRig`, `importRig`) ───────────────────
  // The rig fake on (`__lfRigFake`, scripted per launch as that launch's process holds the record): the page
  // opens the record before the device and imports the upgrade only on `absent`.

  /** Both slots' faders as shown. */
  const faderValues = () => Promise.all(['A', 'B'].map(async (letter) => {
    const fader = cp.getByRole('slider', { name: `Volume for slot ${letter}`, exact: true });
    await fader.waitFor({ timeout: 5000 });
    return fader.inputValue();
  }));
  /** What the handover did: the rig calls in order (an import's text parsed), what the page still kept of the
   * WebView's record when it opened, the fake's state, the slot gains sent, the adds, the toasts, the keys. */
  const rigRead = async () => ({
    ...(await cp.evaluate(() => {
      const rig = window.__lf.native.rig;
      const ls = (k) => localStorage.getItem(k);
      const untimed = (saw) => saw && Object.fromEntries(Object.entries(saw).filter(([k]) => k !== 'at'));
      return {
        calls: rig.calls.map(([name, ...args]) => (name === 'import' ? [name, JSON.parse(args[0])] : [name, ...args])),
        saw: untimed(window.__rigOpenSaw ?? null),
        openSettled: window.__rigOpenSettled ?? null,
        importSaw: untimed(window.__rigImportSaw ?? null),
        seenSaw: window.__rigSeenSaw ?? null,
        state: { status: rig.status, volumes: rig.volumes, notice: rig.notice },
        gains: window.__lf.native.sent.filter((c) => 'SetSlotGain' in c).map((c) => c.SetSlotGain),
        adds: window.__chainProbe.adds.length,
        toasts: window.__lf.notify.toasts().map((t) => `${t.message} x${t.count}`),
        keys: { rig: ls('lf.rig'), inFlight: ls('lf.rigInFlight'), rigRecall: ls('lf.rigRecall') },
      };
    })),
    faders: await faderValues(),
  });
  /** Wait for the handover (`rigRecallDone`), then read it; its `[rig-recall]` lines must match `recall`. */
  async function rigSettle({ recall = [] } = {}) {
    await cp.waitForFunction(() => window.__chainProbe.recall.rigRecallFinished(), undefined, { timeout: 20_000 });
    await cp.waitForTimeout(300);
    const r = await rigRead();
    const recallLogs = chainApp.consumeConsoleErrors(/^\[rig-recall\]/);
    console.log(JSON.stringify({ ...r, calls: r.calls.map(([name]) => name), recallLogs }));
    assert.equal(recallLogs.length, recall.length, `[rig-recall] lines: ${JSON.stringify(recallLogs)}`);
    recall.forEach((re, i) => assert.match(recallLogs[i], re));
    return r;
  }
  /** The fake's record as this launch left it: the next page of the same process (a WebView reload) opens it. */
  const held = (r) => ({ status: r.state.status, volumes: r.state.volumes, notice: r.state.notice });
  const NOTICE = { id: 7, forgotten: ['Fake Amp (VST3)'], pluginsHeld: false };
  const FORGOT = 'Could not restore the plugins. Add a plugin again to load it. x1';
  let r;
  /** A slot's fader. */
  const fader = (letter) => cp.getByRole('slider', { name: `Volume for slot ${letter}`, exact: true });
  /** Wait until the faders show `values`. */
  const fadersAt = (values) => cp.waitForFunction((want) => ['A', 'B'].every((letter, i) => document.querySelector(`input[aria-label="Volume for slot ${letter}"]`)?.value === want[i]), values, { timeout: 5000 });
  /** The four send-offs of the legacy sends as a refused batch reads, and a batch's engine commands so. */
  const alone = ['echo', 'reverb', 'ring', 'ott'].map((id) => JSON.stringify({ SetInputSend: [id, false] })).join(' ');
  const offs = (items) => items.map((it) => JSON.stringify(it.engine)).join(' ');
  /** A slot level as a batch reads, and the slot levels the engine took. */
  const gainOf = (slot, v) => JSON.stringify({ SetSlotGain: [slot, v] });
  const gainsSent = () => cp.evaluate(() => window.__lf.native.sent.filter((c) => 'SetSlotGain' in c).map((c) => c.SetSlotGain));
  /** Take the refused batches' log lines (`refused`), or native MIDI's dropped-input line. */
  const consumeRefused = (refused = true) => {
    assert.ok(chainApp.consumeConsoleErrors(refused ? /^\[platform\] input batch failed/ : /^\[platform\] native MIDI dropped input: full/).length > 0, 'the refused batches are logged');
  };

  // H1. Absent: one import, equal to the v0.8.9 profile's upgrade (`UPGRADED_V089`; no reported chains),
  // after the device's open answered and `host_init`; no add, no slot level sent; the faders show the import's
  // answer. The open's answer is held a while after the device runs, its status already on the feed and the
  // bindings in (a feed may report the device before `engine_open` answers): nothing is imported meanwhile.
  await chainLaunch({
    seed: V089,
    rig: {},
    holdDevice: true,
    bindings: false,
    whileBoot: async () => {
      await cp.waitForFunction(() => '__lf' in window && window.__lf.native.opened.length === 1 && window.__lf.native.midiCalls.some(([c]) => c === 'subscribe'));
      const status = await cp.evaluate(() => window.__lf.native.status());
      await cp.evaluate(([f, b]) => {
        window.__lf.native.emit(f);
        window.__lf.native.midiEmit(b);
      }, [{ ...resetFrame(null), status }, BINDINGS]);
    },
  });
  await cp.waitForTimeout(800);
  await cp.evaluate(() => window.__releaseDevice());
  r = await rigSettle();
  assert.deepEqual(r.importSaw, { devices: 1, hostInits: 1 }, 'the import comes after the device\'s open answered and host_init');
  assert.deepEqual(r.calls, [['open'], ['import', UPGRADED_V089]], 'open, then one import of the upgrade');
  assert.deepEqual({ adds: r.adds, gains: r.gains, toasts: r.toasts, faders: r.faders }, { adds: 0, gains: [], toasts: [], faders: ['0.8', '1'] });
  const imported = r;

  // H3. A WebView reload of that process: open again, no import; the faders show open's volumes, but where the
  // reset frame replays a slot gain (the engine's settings memory), that one.
  await chainLaunch({ mintFrom: 2001, rig: held(imported) });
  r = await rigSettle();
  assert.deepEqual({ calls: r.calls, faders: r.faders, gains: r.gains }, { calls: [['open']], faders: ['0.8', '1'], gains: [] }, 'a reload imports nothing');
  await chainLaunch({ mintFrom: 2001, rig: held(imported), settings: [{ SetSlotGain: [1, 0.6] }] });
  r = await rigSettle();
  assert.deepEqual({ calls: r.calls, faders: r.faders }, { calls: [['open']], faders: ['0.8', '0.6'] }, 'the reset frame\'s slot gain over open\'s');

  // H2. Present, unreadable, unavailable: no import; one log line for the last two (C20), none for present.
  for (const [status, recall, faders] of [
    ['present', [], ['0.7', '1.2']],
    ['unreadable', [/^\[rig-recall\] the native rig record cannot be read/], ['1', '1']],
    ['unavailable', [/^\[rig-recall\] the native rig record is unavailable/], ['1', '1']],
  ]) {
    await chainLaunch({ seed: V089, rig: { status, volumes: status === 'present' ? [0.7, 1.2] : null } });
    r = await rigSettle({ recall });
    assert.deepEqual({ calls: r.calls, faders: r.faders, gains: r.gains, adds: r.adds }, { calls: [['open']], faders, gains: [], adds: 0 }, status);
  }

  // H4. A notice: one toast and one log line on the page, the toast before `seen(id)`; its answer held, a reload
  // shows it again (every open answers it until `seen`); confirmed, the next page shows nothing. An unreadable
  // marker's notice tells its own.
  await chainLaunch({ seed: V089, rig: { status: 'present', volumes: [1, 1], notice: NOTICE, hold: ['seen'] } });
  r = await rigSettle({ recall: [/^\[rig-recall\] the last launch stopped while restoring Fake Amp \(VST3\); its plugins are forgotten/] });
  assert.deepEqual({ calls: r.calls, toasts: r.toasts, seenSaw: r.seenSaw }, { calls: [['open'], ['seen', 7]], toasts: [FORGOT], seenSaw: 1 }, 'one toast, then seen');
  await chainLaunch({ rig: held(r) });
  r = await rigSettle({ recall: [/^\[rig-recall\] the last launch stopped while restoring Fake Amp/] });
  assert.deepEqual({ calls: r.calls, toasts: r.toasts, notice: r.state.notice, seenSaw: r.seenSaw }, { calls: [['open'], ['seen', 7]], toasts: [FORGOT], notice: null, seenSaw: 1 },
    'a reload before seen shows it again, then seen; confirmed now');
  await chainLaunch({ rig: held(r) });
  r = await rigSettle();
  assert.deepEqual({ calls: r.calls, toasts: r.toasts }, { calls: [['open']], toasts: [] }, 'seen: no notice on the next page');
  await chainLaunch({ seed: V089, rig: { notice: { id: 8, forgotten: [], pluginsHeld: true }, hold: ['seen'] } });
  r = await rigSettle({ recall: [/^\[rig-recall\] the last restore's crash marker could not be read/] });
  assert.deepEqual({ calls: r.calls.map(([name, x]) => (name === 'import' ? name : [name, x].filter((v) => v !== undefined).join(' '))), toasts: r.toasts, seenSaw: r.seenSaw },
    { calls: ['open', 'seen 8', 'import'], toasts: ['Could not restore the plugins this time. Restart BleepLoop to bring them back. x1'], seenSaw: 1 },
    'told once, though the import answers it again while seen is on its way');

  // H5. The fader's sources. The amp-sim with no stored level: before the scan the upgrade's level for a plugin
  // of unknown kind (0.1), after it the scanned effect's (0.9), until the import answers (its own levels,
  // 0.75 and 0.4). A gesture on slot B sends one plain `SetSlotGain` and outranks every earlier source: a reset
  // frame after it replaying an older level (0.3), and the import's answer, leave it shown.
  const { [`lf.pluginGain.0.${keyOf(AMP)}`]: _gain, ...V089_NO_LEVEL } = V089;
  await chainLaunch({
    seed: V089_NO_LEVEL,
    holdPaths: true,
    rig: { hold: ['import'], reply: { import: { status: 'present', volumes: [0.75, 0.4], notice: null } } },
  });
  await cp.waitForFunction(() => window.__recallProbe.calls.paths.length === 1);
  assert.deepEqual(await faderValues(), ['0.1', '1'], 'before the scan: the upgrade\'s level, the plugin\'s kind unknown');
  await cp.evaluate(() => window.__recallProbe.releasePaths());
  await cp.waitForFunction(() => window.__lf.native.rig.calls.some(([name]) => name === 'import'));
  assert.deepEqual(await faderValues(), ['0.9', '1'], 'after the scan, before the import answers: the upgrade\'s own');
  await fader('B').fill('0.5');
  await cp.evaluate((f) => window.__lf.native.emit(f), resetFrame(null, [{ SetSlotGain: [1, 0.3] }]));
  await cp.waitForTimeout(200);
  assert.deepEqual(await faderValues(), ['0.9', '0.5'], 'a reset frame after the gesture names an older level: the gesture stays');
  await cp.evaluate(() => window.__lf.native.rig.release('import'));
  r = await rigSettle();
  assert.deepEqual({ gains: r.gains, faders: r.faders }, { gains: [[1, 0.5]], faders: ['0.75', '0.5'] }, 'one plain SetSlotGain; the gesture kept over the import\'s answer');

  // H5, refused. The fader stays where the player put it. A gesture the engine took (0.5), then a reset frame
  // replaying an older level (0.3) arrives late: the gesture stays. The next gesture (0.25) is refused while other
  // commands keep coming (each batch busy a while, so they queue): it stays shown and goes again in batches of its
  // own until taken, then stops.
  await chainLaunch({ seed: V089, rig: { status: 'present', volumes: [0.7, 0.6] }, sendDelay: 15 });
  r = await rigSettle();
  assert.deepEqual(r.faders, ['0.7', '0.6'], 'open\'s levels');
  await fader('B').fill('0.5');
  await cp.waitForFunction(() => window.__lf.native.sent.some((c) => c.SetSlotGain?.[1] === 0.5));
  await cp.evaluate((f) => window.__lf.native.emit(f), resetFrame(null, [{ SetSlotGain: [1, 0.3] }]));
  await cp.waitForTimeout(200);
  assert.deepEqual(await faderValues(), ['0.7', '0.5'], 'a late reset frame names an older level: the taken gesture stays');
  await cp.evaluate(async () => {
    const { sendEngine } = await import('/src/platform/index.ts');
    window.__refuse = ['SetSlotGain'];
    window.__busy = setInterval(() => void sendEngine({ SetMasterVolume: 0.8 }), 3);
  });
  await cp.waitForTimeout(100);
  await fader('B').fill('0.25');
  await cp.waitForFunction(() => (window.__refusedBatches ?? []).length >= 6, undefined, { timeout: 5000 });
  await cp.evaluate(() => clearInterval(window.__busy));
  assert.deepEqual(await faderValues(), ['0.7', '0.25'], 'the refused gesture stays shown');
  {
    const [first, ...rest] = (await cp.evaluate(() => window.__refusedBatches)).map(offs);
    const resends = rest.filter((b) => b !== first);
    console.log(JSON.stringify({ first, resends: resends.length, merged: resends.filter((b) => b !== gainOf(1, 0.25)).slice(0, 3) }));
    assert.ok(first.includes('SetMasterVolume'), 'the gesture\'s own send shared its batch');
    assert.ok(resends.length >= 3 && resends.every((b) => b === gainOf(1, 0.25)), `the refused gesture sent again alone: ${resends.length}`);
  }
  await cp.evaluate(() => void (window.__refuse = null));
  await cp.waitForFunction(() => window.__lf.native.sent.some((c) => c.SetSlotGain?.[1] === 0.25));
  await cp.waitForTimeout(700);
  consumeRefused();
  assert.deepEqual({ gains: await gainsSent(), faders: await faderValues() }, { gains: [[1, 0.5], [1, 0.25]], faders: ['0.7', '0.25'] }, 'taken once, then no more');

  // H5, partly. A gesture (0.45) rides a batch whose other command the engine drops (refused partly): the engine
  // took the level though the batch answers false; the fader stays, and the level goes again alone (harmless).
  await chainLaunch({ seed: V089, rig: { status: 'present', volumes: [0.7, 0.6] } });
  await rigSettle();
  await cp.evaluate(async () => {
    window.__refusePartly = ['SetMasterVolume'];
    const { sendEngine } = await import('/src/platform/index.ts');
    const { setSlotLevel } = await import('/src/ui/state/instrument.ts');
    window.__batchMark = window.__lf.native.batches.length;
    void sendEngine({ SetMasterVolume: 0.8 });
    setSlotLevel(1, 0.45);
  });
  await cp.waitForFunction(() => window.__lf.native.sent.filter((c) => c.SetSlotGain?.[1] === 0.45).length >= 2, undefined, { timeout: 5000 }).catch(() => {});
  await cp.waitForTimeout(700);
  await cp.evaluate(() => void (window.__refusePartly = null));
  {
    const shared = (await cp.evaluate(() => window.__refusedBatches)).map(offs);
    const taken = await cp.evaluate(() => window.__lf.native.batches.slice(window.__batchMark)
      .map((b) => b.items.filter((it) => 'engine' in it).map((it) => JSON.stringify(it.engine)).join(' ')).filter((b) => b !== ''));
    assert.deepEqual(shared, [`${JSON.stringify({ SetMasterVolume: 0.8 })} ${gainOf(1, 0.45)}`], 'the gesture rode the batch refused partly');
    assert.deepEqual(taken, [gainOf(1, 0.45), gainOf(1, 0.45)], 'taken in it, then sent again alone, once');
  }
  consumeRefused(false);
  assert.deepEqual({ gains: await gainsSent(), faders: await faderValues() }, { gains: [[1, 0.45], [1, 0.45]], faders: ['0.7', '0.45'] }, 'the fader stays on the gesture');

  // H6. The import refused (a failed write) or stale (a replaced page): one log line, nothing else.
  for (const [why, line] of [
    ['injected: the record could not be written', /^\[rig-recall\] could not import the rig record; nothing restored/],
    ['stale', /^\[rig-recall\] the native rig record refused this page's import: a newer page replaced it/],
  ]) {
    await chainLaunch({ seed: V089, rig: { refuse: { import: why } } });
    r = await rigSettle({ recall: [line] });
    assert.deepEqual({ calls: r.calls.map(([name]) => name), toasts: r.toasts, gains: r.gains, adds: r.adds, status: r.state.status },
      { calls: ['open', 'import'], toasts: [], gains: [], adds: 0, status: 'absent' }, why);
  }

  // H7. The open refused, unanswered for 2 s (no device opens until then), or answered in a shape D13 does not
  // allow (`decodeRigAnswer`, which the fake's replies go through as Tauri's do): one log line, the device still
  // opens (`chainLaunch` waits for it), no import, the faders at unity. An import answered other than `present`:
  // one log line too.
  /** Wait until `ms` passed since the page called `rig.open`, then say how many devices it opened. */
  const devicesAt = async (ms) => {
    await cp.waitForFunction(() => '__lf' in window && window.__rigOpenSaw !== undefined);
    await cp.waitForFunction((t) => performance.now() - window.__rigOpenSaw.at >= t, ms, { timeout: 10_000 });
    return cp.evaluate(() => window.__lf.native.opened.length);
  };
  for (const [what, rig, whileBoot] of [
    ['refused', { refuse: { open: 'rig_open: not registered' } }],
    ['unanswered', { hold: ['open'] }, async () => assert.equal(await devicesAt(1800), 0, 'no device opens before the open\'s 2 s')],
    ['absent with volumes', { reply: { open: { status: 'absent', volumes: [0.5, 0.5], notice: null } } }],
    ['present without volumes', { reply: { open: { status: 'present', volumes: null, notice: null } } }],
  ]) {
    await chainLaunch({ seed: V089, rig, whileBoot });
    r = await rigSettle({ recall: [/^\[rig-recall\] could not open the native rig record/] });
    assert.deepEqual({ calls: r.calls, adds: r.adds, gains: r.gains, faders: r.faders }, { calls: [['open']], adds: 0, gains: [], faders: ['1', '1'] }, what);
  }
  for (const reply of [{ status: 'absent', volumes: null, notice: null }, { status: 'unreadable', volumes: null, notice: null }]) {
    await chainLaunch({ seed: V089, rig: { reply: { import: reply } } });
    r = await rigSettle({ recall: [/^\[rig-recall\] could not import the rig record; nothing restored/] });
    assert.deepEqual({ calls: r.calls.map(([name]) => name), faders: r.faders }, { calls: ['open', 'import'], faders: ['1', '1'] }, `an import answered ${reply.status}`);
  }
  {
    // The Tauri adapter cannot load in the browser probe: its source must read both answers through
    // `decodeRigAnswer`, as the fake does, and invoke each call nowhere else.
    const tauri = readFileSync(new URL('../../src/platform/host.tauri.ts', import.meta.url), 'utf8');
    for (const [call, kind] of [['rig_open', 'open'], ['rig_import', 'import']]) {
      const invokes = tauri.match(new RegExp(`invoke(<[^>]*>)?\\(\\s*'${call}'`, 'g')) ?? [];
      const decoded = new RegExp(`decodeRigAnswer\\(\\s*await invoke(<[^>]*>)?\\(\\s*'${call}'[^;]*?\\)\\s*,\\s*'${kind}'\\s*\\)`);
      assert.ok(invokes.length === 1 && decoded.test(tauri), `host.tauri.ts reads ${call}'s answer through decodeRigAnswer(…, '${kind}'), its only invoke`);
    }
  }

  // H8. The DEV cutover: `lf.rig` and `lf.rigInFlight` deleted before the open; no device opens before the open
  // settled, the one Audio Settings asks for meanwhile and an ASIO driver switch's included (all open after it);
  // the legacy keys untouched.
  await chainLaunch({
    seed: { ...V089, 'lf.rig': OLD_DEV_RIG, 'lf.rigInFlight': '[1001]' },
    rig: { status: 'present', volumes: [1, 1], hold: ['open'] },
    devices: 3,
    whileBoot: async () => {
      await cp.waitForFunction(() => '__lf' in window && window.__lf.native.rig.calls.some(([name]) => name === 'open'));
      await cp.evaluate(() => window.__lf.ui.openSettings());
      const buffer = cp.getByRole('combobox', { name: 'Buffer size in frames' });
      await buffer.selectOption((await buffer.inputValue()) === '256' ? '512' : '256');
      await cp.evaluate(async () => {
        const { switchEngineAsioDriver } = await import('/src/ui/state/engine-store.ts');
        void switchEngineAsioDriver('');
      });
      await cp.waitForTimeout(300);
      assert.equal(await cp.evaluate(() => window.__lf.native.opened.length), 0, 'no device opens while the open waits, Audio Settings\' nor a driver switch\'s');
      await cp.evaluate(() => window.__lf.native.rig.release('open'));
    },
  });
  r = await rigSettle();
  assert.deepEqual(r.saw, { rig: null, inFlight: null, devices: 0 }, 'the old keys gone and no device opened at the open');
  assert.equal(r.openSettled, 0, 'no device opened before the open settled');
  assert.deepEqual(r.keys, { rig: null, inFlight: null, rigRecall: V089['lf.rigRecall'] }, 'nothing writes them back; the legacy keys kept');

  // H9. The engine refuses every batch carrying the reset's send-off of the legacy sends (a full command ring),
  // past the 10 s log point, while other commands keep coming (each batch busy for a while, so they queue): the
  // four `SetInputSend false` go again in batches of their own until taken, one log line at 10 s (none before),
  // then stop.
  const sendOffLines = () => chainApp.consumeConsoleErrors(/^\[instrument\] the legacy sends' send-off was refused/);
  await chainLaunch({ seed: V089, rig: { status: 'present', volumes: [1, 1] }, refuse: ['SetInputSend'], sendDelay: 15 });
  await cp.waitForFunction(() => (window.__refusedBatches ?? []).length > 0);
  await cp.evaluate(async () => {
    const { sendEngine } = await import('/src/platform/index.ts');
    window.__busy = setInterval(() => void sendEngine({ SetMasterVolume: 0.8 }), 3);
  });
  await cp.waitForTimeout(9_000);
  assert.deepEqual(sendOffLines(), [], 'no log line before 10 s');
  await cp.waitForTimeout(2_000);
  await cp.evaluate(() => clearInterval(window.__busy));
  {
    const refusedBatches = await cp.evaluate(() => window.__refusedBatches ?? []);
    // the reset's batch (the outbox sends a refused batch once more itself), then the resends
    const [reset, ...rest] = refusedBatches.map(offs);
    const resends = rest.filter((b) => b !== reset);
    console.log(JSON.stringify({ resends: resends.length, merged: resends.filter((b) => b !== alone).slice(0, 3) }));
    assert.ok(reset.includes(alone) && reset !== alone, 'the reset\'s batch carries the send-off among the settings');
    assert.ok(resends.length >= 4 && resends.every((b) => b === alone), `the four send-offs sent again alone while refused: ${resends.length}`);
    const lines = sendOffLines();
    assert.equal(lines.length, 1, `one log line at 10 s: ${JSON.stringify(lines)}`);
  }
  await cp.evaluate(() => void (window.__refuse = null));
  await cp.waitForFunction(() => window.__lf.native.sent.filter((c) => c.SetInputSend?.[1] === false).length === 4, undefined, { timeout: 5000 });
  await cp.waitForTimeout(1000);
  assert.equal(await cp.evaluate(() => window.__lf.native.sent.filter((c) => 'SetInputSend' in c).length), 4, 'taken once, then no more');
  consumeRefused();
  r = await rigSettle();
  assert.deepEqual(r.calls, [['open']]);

  // H9, a blur owed. A release lost with the blur after it (the next batch owes the blur) while the send-off is
  // refused: the blur goes in a call of its own, never in front of the four, whose resends stay alone.
  await chainLaunch({ seed: V089, rig: { status: 'present', volumes: [1, 1] }, refuse: ['SetInputSend'] });
  await cp.waitForFunction(() => (window.__refusedBatches ?? []).length >= 8, undefined, { timeout: 10_000 });
  await cp.evaluate(async () => {
    const { input } = await import('/src/platform/index.ts');
    const seen = window.__refusedBatches.length;
    await new Promise((resolve) => {
      const tick = () => (window.__refusedBatches.length > seen ? resolve() : setTimeout(tick, 1));
      tick();
    });
    // a resend just refused: the next waits its backoff; the release, its one retry and its blur are lost
    window.__refusedMark = window.__refusedBatches.length;
    window.__batchMark = window.__lf.native.batches.length;
    window.__lf.native.failSends = 3;
    input.note('pointer:1', 60, 0, false);
  });
  await cp.waitForFunction(() => window.__refusedBatches.length >= window.__refusedMark + 3, undefined, { timeout: 5000 });
  {
    const after = (await cp.evaluate(() => window.__refusedBatches.slice(window.__refusedMark))).map(offs);
    const taken = await cp.evaluate(() => window.__lf.native.batches.slice(window.__batchMark).map((b) => b.items.map((it) => ('engine' in it ? 'engine' : JSON.stringify(it.input)))));
    console.log(JSON.stringify({ after: after.slice(0, 4), taken: taken.slice(0, 6) }));
    assert.ok(after.every((b) => b === alone), `the resends after the lost blur carry the four alone: ${JSON.stringify(after.slice(0, 3))}`);
    assert.equal(taken.filter((b) => b.length === 1 && b[0] === '"blur"').length, 2, 'the lost blur, then the owed one in a call of its own');
    assert.equal(chainApp.consumeConsoleErrors(/^\[platform\] the blur after a lost release failed/).length, 1, 'the lost blur logged');
  }
  await cp.evaluate(() => void (window.__refuse = null));
  await cp.waitForFunction(() => window.__lf.native.sent.filter((c) => c.SetInputSend?.[1] === false).length === 4, undefined, { timeout: 5000 });
  consumeRefused();
  await rigSettle();

  // H9, partly. The engine takes the reset's batch but drops two of the four (refused partly): the four go again
  // alone, each taking the same two, until all are taken.
  const sendOffsTaken = () => cp.evaluate(() => window.__lf.native.sent.filter((c) => 'SetInputSend' in c).map((c) => c.SetInputSend[0]));
  await chainLaunch({ seed: V089, rig: { status: 'present', volumes: [1, 1] }, refusePartly: ['SetInputSend:echo', 'SetInputSend:ring'] });
  await cp.waitForTimeout(1000);
  {
    const [reset, ...resends] = (await cp.evaluate(() => window.__refusedBatches ?? [])).map(offs);
    assert.ok(reset.includes(alone) && reset !== alone, 'the reset\'s batch, taken partly');
    assert.ok(resends.length >= 2 && resends.every((b) => b === alone), `the four sent again alone while refused partly: ${resends.length}`);
    const taken = await sendOffsTaken();
    assert.ok(taken.length >= 6 && taken.every((id) => id === 'reverb' || id === 'ott'), `only the two kept are taken: ${JSON.stringify(taken)}`);
  }
  await cp.evaluate(() => void (window.__refusePartly = null));
  await cp.waitForFunction(() => window.__lf.native.sent.some((c) => c.SetInputSend?.[0] === 'echo'), undefined, { timeout: 5000 });
  await cp.waitForTimeout(500);
  {
    const taken = await sendOffsTaken();
    assert.deepEqual(taken.filter((id) => id === 'echo' || id === 'ring').sort(), ['echo', 'ring'], 'the dropped two taken once, then no more');
    await cp.waitForTimeout(500);
    assert.equal((await sendOffsTaken()).length, taken.length, 'the resend stopped');
  }
  consumeRefused(false);
  await rigSettle();

  // H10. The device stops during the import's scan, or after it while the import waits for native MIDI's first
  // `bindings` report: nothing is imported (one log line), the faders at unity.
  const deviceStops = () => cp.evaluate(() => window.__lf.native.emit({ seq: 2, reset: false, events: [], status: null }));
  await chainLaunch({ seed: V089, rig: {}, holdPaths: true });
  await cp.waitForFunction(() => window.__recallProbe.calls.paths.length === 1);
  await deviceStops();
  await cp.evaluate(() => window.__recallProbe.releasePaths());
  r = await rigSettle({ recall: [/^\[rig-recall\] no audio device runs at the import; nothing imported/] });
  assert.deepEqual({ calls: r.calls, faders: r.faders, status: r.state.status }, { calls: [['open']], faders: ['1', '1'], status: 'absent' }, 'no import with no device');
  await chainLaunch({ seed: V089, rig: {}, holdPaths: true, bindings: false });
  await cp.waitForFunction(() => window.__recallProbe.calls.paths.length === 1);
  await cp.evaluate(() => window.__recallProbe.releasePaths());
  await cp.waitForTimeout(300);
  await deviceStops();
  await cp.evaluate((b) => window.__lf.native.midiEmit(b), BINDINGS);
  r = await rigSettle({ recall: [/^\[rig-recall\] no audio device runs at the import; nothing imported/] });
  assert.deepEqual({ calls: r.calls, faders: r.faders, status: r.state.status }, { calls: [['open']], faders: ['1', '1'], status: 'absent' }, 'no import when the device stopped in the bindings wait');

  // H11. The import never answers: still waited for just before 5 s, then one log line and the handover ends (the
  // boot done), the faders on the upgrade's levels; a late answer still shows its own. A late rejection: one log
  // line, nothing else (the faders stay on the upgrade's levels, which the next launch imports again).
  const importWaits = async (ms) => {
    await cp.waitForFunction(() => window.__rigImportSaw !== undefined, undefined, { timeout: 10_000 });
    await cp.waitForFunction((t) => performance.now() - window.__rigImportSaw.at >= t, ms, { timeout: 10_000 });
    return cp.evaluate(() => !window.__chainProbe.recall.rigRecallFinished());
  };
  const IMPORT_TIMED_OUT = /^\[rig-recall\] the rig record's import got no answer in 5 s; the handover ends here/;
  await chainLaunch({ seed: V089, rig: { hold: ['import'], reply: { import: { status: 'present', volumes: [0.7, 0.9], notice: NOTICE } } } });
  assert.equal(await importWaits(4700), true, 'the import still waited for at 4.7 s');
  assert.deepEqual(chainApp.consumeConsoleErrors(/^\[rig-recall\]/), [], 'no line before 5 s');
  r = await rigSettle({ recall: [IMPORT_TIMED_OUT] });
  assert.deepEqual({ calls: r.calls.map(([name]) => name), faders: r.faders, toasts: r.toasts }, { calls: ['open', 'import'], faders: ['0.8', '1'], toasts: [] }, 'the handover ends without the answer');
  await cp.evaluate(() => window.__lf.native.rig.release('import'));
  await fadersAt(['0.7', '0.9']);
  await cp.waitForFunction(() => window.__lf.native.rig.calls.some(([name]) => name === 'seen'));
  assert.deepEqual((await rigRead()).toasts, [FORGOT], 'the late answer\'s notice');
  assert.equal(chainApp.consumeConsoleErrors(/^\[rig-recall\] the last launch stopped while restoring Fake Amp/).length, 1, 'its one log line');
  await chainLaunch({ seed: V089, rig: { hold: ['import'], refuse: { import: 'injected: the record could not be written' } } });
  r = await rigSettle({ recall: [IMPORT_TIMED_OUT] });
  await cp.evaluate(() => window.__lf.native.rig.release('import'));
  await cp.waitForTimeout(300);
  assert.equal(chainApp.consumeConsoleErrors(/^\[rig-recall\] the rig record's import failed after its timeout; nothing restored, the next launch imports it again/).length, 1, 'a late rejection: one log line');
  assert.deepEqual({ faders: await faderValues(), toasts: (await rigRead()).toasts }, { faders: ['0.8', '1'], toasts: [] }, 'nothing else');

  // H11, a late open. The open answers after its 2 s (the devices opened meanwhile): its notice is told (once, then
  // `seen`) and its levels shown at an answer's rank, though nothing is imported on it.
  await chainLaunch({ seed: V089, rig: { status: 'present', volumes: [0.7, 0.6], notice: NOTICE, hold: ['open'] } });
  r = await rigSettle({ recall: [/^\[rig-recall\] could not open the native rig record/] });
  assert.deepEqual({ calls: r.calls, faders: r.faders, toasts: r.toasts }, { calls: [['open']], faders: ['1', '1'], toasts: [] }, 'timed out: unity, nothing told');
  await cp.evaluate(() => window.__lf.native.rig.release('open'));
  await fadersAt(['0.7', '0.6']);
  await cp.waitForFunction(() => window.__lf.native.rig.calls.some(([name]) => name === 'seen'));
  r = await rigRead();
  assert.deepEqual({ calls: r.calls, toasts: r.toasts }, { calls: [['open'], ['seen', 7]], toasts: [FORGOT] }, 'the late open\'s notice, once, then seen');
  assert.equal(chainApp.consumeConsoleErrors(/^\[rig-recall\] the last launch stopped while restoring Fake Amp/).length, 1, 'its one log line');

  await chainContext.close();
});
