/**
 * Rig recall across launches: each launch reloads the page, whose mount runs the production boot
 * chain over a substituted plugin host (installed before any app module runs) and the real close
 * guard with only its native close capabilities substituted, so the record and the in-flight marker
 * live in the page's real localStorage between launches. Proves: two plugins picked in the slot
 * dropdowns (the effect's automatic GO LIVE included) come back at the next launch through the load
 * path, slot 0 then slot 1, without an arm, a monitor or an editor call and with GO LIVE unpressed, the
 * restored instrument taking the MIDI slot; an unloaded slot stays empty; a slot whose plugin is
 * missing from the scan is skipped with one `[rig-recall]` log line and no toast, and returns once the
 * plugin is back; a load that fails at recall shows one toast and is not retried; a pick made while
 * slot 0 restores wins slot 1; a launch stopped during a hanging load (a close through the close
 * button included), or after the loads but inside the settle window, makes the next launch skip the
 * recall with one log line and one toast and forget the record, so the launch after that is clean; a
 * close through the close button once the loads are back keeps the rig for the next launch; a synth
 * picked, or a slot activated, while the scan still runs keeps its slot and the MIDI slot, and the
 * synth's slot stays a synth at the launch after. The native load, the scan, the OS close and a real
 * crash are out of reach here: `pnpm native:recall` covers them on the PC.
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
 * 'resolve' (default), 'reject', 'hang' (never answers) or 'defer' (answers on `release()`);
 * `holdScan` keeps the scan running until `releaseScan()`.
 */
function installFakeHost() {
  const { scan = [], loads = {}, holdScan = false } = JSON.parse(sessionStorage.getItem('recallProbe') ?? '{}');
  const calls = { load: [], arm: [], editor: [] };
  const pending = [];
  let releaseScan = () => {};
  const scanned = holdScan ? new Promise((resolve) => (releaseScan = () => resolve(scan))) : Promise.resolve(scan);
  window.__recallProbe = { calls, release: () => pending.shift()?.(), releaseScan: () => releaseScan() };
  window.__recallHost = {
    available: true,
    scanPlugins: () => scanned,
    loadPlugin: (slot, path, id) => {
      calls.load.push({ slot, path, id });
      const reply = { slot, descriptor: scan.find((d) => d.path === path && d.id === id) };
      const how = loads[slot] ?? 'resolve';
      if (how === 'reject') return Promise.reject(new Error('injected load failure'));
      if (how === 'hang') return new Promise(() => {});
      if (how === 'defer') return new Promise((resolve) => pending.push(() => resolve(reply)));
      return Promise.resolve(reply);
    },
    unloadPlugin: async () => {},
    listParams: async () => [],
    armInput: async (slot) => { calls.arm.push(`input ${slot}`); },
    armMonitor: async (slot) => { calls.arm.push(`monitor ${slot}`); },
    openEditor: async (slot) => { calls.editor.push(slot); },
  };
}

await probe(async ({ open }) => {
  const app = await open({
    viewport: { width: 1280, height: 820 },
    init: (p) => Promise.all([
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
    const errorsBefore = app.consoleErrors.length;
    await page.evaluate((c) => sessionStorage.setItem('recallProbe', JSON.stringify(c)), cfg);
    await page.reload();
    await page.waitForFunction(() => '__lf' in window && !!window.__closeRequest);
    await page.evaluate(async () => {
      Object.assign(window.__recallProbe, {
        instrument: await import('/src/audio/instrument.ts'),
        slots: await import('/src/audio/instrument-slots.ts'),
        recall: await import('/src/audio/rig-recall.ts'),
      });
    });
    return { errorsBefore };
  }

  /** Wait for this launch's recall to finish, then read what it did. */
  async function settle({ errorsBefore }) {
    await page.waitForFunction(() => window.__recallProbe.recall.rigRecallDone(), undefined, { timeout: 20_000 });
    const state = await page.evaluate(async () => {
      const { calls, instrument, recall } = window.__recallProbe;
      const io = await import('/src/audio/native-io.ts');
      const { pluginDescriptorKey } = await import('/src/audio/plugin-descriptor.ts');
      return {
        loads: calls.load,
        arms: calls.arm,
        editors: calls.editor,
        slots: instrument.slotPlugins().map((d) => (d ? pluginDescriptorKey(d) : null)),
        synths: instrument.slotIds(),
        active: instrument.activeSlot(),
        armed: [...io.inputArmed(), ...io.monitorArmed()],
        live: [...document.querySelectorAll('.slot')].map((el) => el.querySelector('.tgl.live')?.textContent?.trim() ?? null),
        inFlight: recall.recallInFlight(),
        toasts: window.__lf.notify.toasts().map((t) => `${t.message} x${t.count}`),
      };
    });
    const logs = app.consoleErrors.slice(errorsBefore);
    state.recallLogs = logs.filter((l) => l.startsWith('[rig-recall]'));
    state.loadFailedLogs = logs.filter((l) => l.startsWith('[instrument] plugin load failed')).length;
    console.log(JSON.stringify(state));
    return state;
  }

  const pick = async (slot, desc) => {
    await page.locator('.slot__select').nth(slot).selectOption(desc ? keyOf(desc) : '');
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
    { arms: [], editors: [], armed: [false, false, false, false], inFlight: false },
    `${what}: nothing armed, no editor, no marker`,
  );

  // 1. First launch: nothing remembered. The owner picks the amp (its automatic GO LIVE and editor
  // run, as on the rig) and a synth.
  let s = await settle(await launch({ scan: ALL }));
  assert.deepEqual(s.loads, [], 'a first launch restores nothing');
  await pick(0, FX);
  await pick(1, SYN);
  const picked = await page.evaluate(() => window.__recallProbe.calls);
  assert.deepEqual(picked.arm, ['input 0', 'monitor 0'], 'the dropdown pick of an effect goes live on its own (baseline)');

  // 2. Restart: both come back in slot order, nothing armed, GO LIVE unpressed.
  s = await settle(await launch({ scan: ALL }));
  assert.deepEqual(s.loads, [loadOf(0, FX), loadOf(1, SYN)]);
  assert.deepEqual(s.slots, [keyOf(FX), keyOf(SYN)]);
  expectIdle(s, 'restored rig');
  assert.deepEqual(s.live, ['GO LIVE', 'GO LIVE']);
  assert.deepEqual(s.toasts, []);
  assert.deepEqual(s.recallLogs, []);
  assert.equal(s.active, 1, 'the restored instrument takes the MIDI slot when the player chose nothing');

  // 3. The owner unloads slot 1: the next launch restores slot 0 only.
  await pick(1, null);
  s = await settle(await launch({ scan: ALL }));
  assert.deepEqual(s.loads, [loadOf(0, FX)], 'an unloaded slot is forgotten');
  assert.deepEqual(s.slots, [keyOf(FX), null]);
  expectIdle(s, 'after an unload');

  // 4. Swap slot 0 to another plugin, then launch with that plugin missing from the scan.
  await pick(0, ALT);
  s = await settle(await launch({ scan: [FX, SYN] }));
  assert.deepEqual(s.loads, [], 'a plugin missing from the scan is not loaded');
  assert.equal(s.recallLogs.length, 1, 'one log line for the missing plugin');
  assert.match(s.recallLogs[0], /^\[rig-recall\] slot 1: Probe Drive \(vst3\) at C:\\probe\\drive\.vst3 is not in this launch's plugin scan/);
  assert.deepEqual(s.toasts, [], 'a missing plugin raises no toast');
  expectIdle(s, 'missing plugin');

  // 5. The plugin is back, but its load fails: one toast, and the next launch does not retry it.
  s = await settle(await launch({ scan: ALL, loads: { 0: 'reject' } }));
  assert.deepEqual(s.loads, [loadOf(0, ALT)], 'a skipped slot keeps its record for a launch where the plugin is back');
  assert.deepEqual(s.toasts, ['Plugin load failed x1']);
  assert.equal(s.loadFailedLogs, 1);
  assert.deepEqual(s.slots, [null, null]);
  s = await settle(await launch({ scan: ALL }));
  assert.deepEqual(s.loads, [], 'a load that failed at recall is not retried');
  assert.deepEqual(s.toasts, []);

  // 6. A pick made while slot 0 is still restoring wins slot 1.
  await pick(0, FX);
  await pick(1, SYN);
  const deferred = await launch({ scan: ALL, loads: { 0: 'defer' } });
  await page.waitForFunction(() => window.__recallProbe.calls.load.length === 1);
  await page.evaluate((alt) => window.__recallProbe.instrument.selectPlugin(1, alt), ALT);
  await page.evaluate(() => window.__recallProbe.release());
  s = await settle(deferred);
  assert.deepEqual(s.loads, [loadOf(0, FX), loadOf(1, ALT)], 'the recall does not load over the pick');
  assert.deepEqual(s.slots, [keyOf(FX), keyOf(ALT)]);

  // 7. A launch that dies inside a hanging load, closed through the close button: the marker stays, so
  // the next launch restores nothing, says so once and forgets the rig; the launch after that is clean.
  await launch({ scan: ALL, loads: { 0: 'hang' } });
  await page.waitForFunction(() => window.__recallProbe.calls.load.length === 1 && window.__recallProbe.recall.recallInFlight());
  assert.deepEqual(await closeApp(), { inFlight: true, done: false }, 'a close while a recalled load hangs keeps the marker');
  s = await settle(await launch({ scan: ALL }));
  assert.deepEqual(s.loads, [], 'no recall after a launch died restoring');
  assert.deepEqual(s.toasts, ['Plugins not restored x1']);
  assert.equal(s.recallLogs.length, 1);
  assert.match(s.recallLogs[0], /^\[rig-recall\] the last launch stopped while restoring Probe Amp \(clap\), Probe Drive \(vst3\)/);
  expectIdle(s, 'skipped recall');
  s = await settle(await launch({ scan: ALL }));
  assert.deepEqual({ loads: s.loads, toasts: s.toasts, recallLogs: s.recallLogs }, { loads: [], toasts: [], recallLogs: [] },
    'the launch after a skipped recall is clean');

  // 8. The same when the loads finished but the launch died inside the settle window.
  await pick(0, FX);
  await pick(1, SYN);
  await launch({ scan: ALL });
  await page.waitForFunction(() => {
    const { instrument, recall } = window.__recallProbe;
    return instrument.slotPlugins().every(Boolean) && recall.recallInFlight() && !recall.rigRecallDone();
  });
  s = await settle(await launch({ scan: ALL }));
  assert.deepEqual(s.loads, [], 'a launch that died right after its loads also skips the next recall');
  assert.deepEqual(s.toasts, ['Plugins not restored x1']);
  s = await settle(await launch({ scan: ALL }));
  assert.deepEqual({ loads: s.loads, toasts: s.toasts }, { loads: [], toasts: [] });

  // 9. A close through the close button once the loads are back, inside the settle window: the marker
  // goes with the close, and the next launch restores the rig without a toast.
  await pick(0, FX);
  await pick(1, SYN);
  await launch({ scan: ALL });
  await page.waitForFunction(() => {
    const { instrument, recall } = window.__recallProbe;
    return instrument.slotPlugins().every(Boolean) && recall.recallInFlight() && !recall.rigRecallDone();
  });
  assert.deepEqual(await closeApp(), { inFlight: false, done: false }, 'a clean close inside the settle window clears the marker');
  s = await settle(await launch({ scan: ALL }));
  assert.deepEqual(s.loads, [loadOf(0, FX), loadOf(1, SYN)], 'a close right after a restore keeps the rig');
  assert.deepEqual({ toasts: s.toasts, recallLogs: s.recallLogs }, { toasts: [], recallLogs: [] });

  // 10. A synth picked while the scan still runs: its slot keeps the synth and the MIDI slot stays with
  // it; the other slot's instrument is restored without taking MIDI. The next launch restores only
  // that instrument, which then takes MIDI again.
  const held = await launch({ scan: ALL, holdScan: true });
  await page.getByRole('button', { name: 'Pad for slot 1' }).click();
  await page.waitForFunction(() => window.__recallProbe.slots.slotPendingCounts().every((n) => n === 0));
  await page.evaluate(() => window.__recallProbe.releaseScan());
  s = await settle(held);
  assert.deepEqual(s.loads, [loadOf(1, SYN)], 'the recall does not load over a synth picked during boot');
  assert.deepEqual({ slots: s.slots, synth: s.synths[0], active: s.active }, { slots: [null, keyOf(SYN)], synth: 'pad', active: 0 },
    'the picked synth keeps its slot and the MIDI slot');
  expectIdle(s, 'synth picked during boot');
  s = await settle(await launch({ scan: ALL }));
  assert.deepEqual({ loads: s.loads, active: s.active }, { loads: [loadOf(1, SYN)], active: 1 },
    'a slot the player turned into a synth stays one at the next launch');

  // 11. A slot activated while the scan still runs keeps the MIDI slot over a restored instrument.
  const activated = await launch({ scan: ALL, holdScan: true });
  await page.getByRole('button', { name: 'Activate slot 1' }).click();
  await page.evaluate(() => window.__recallProbe.releaseScan());
  s = await settle(activated);
  assert.deepEqual({ loads: s.loads, active: s.active }, { loads: [loadOf(1, SYN)], active: 0 },
    'a restored instrument does not move a MIDI slot the player chose');
});
