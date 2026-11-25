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
 * Chain mode (DEV; `docs/unit-contract.md` §1, §6), in a browser context of its own with the chain fake on
 * (`__lfChainFake`) over the same substituted plugin host, scanning the chain fake's canned plugins (a scan
 * kind that disagrees with the fake's activation stands for a VST2 scanned as an effect that activates as a
 * source: the frontend path does not depend on the format). Each launch reloads; its first `bindings` report
 * (a REC/DUB pedal only) and the engine's reset frame are scripted, the reset frame carrying the previous
 * page's chains only to model a WebView reload; every chain add goes through a wrapper that keeps
 * `lf.rig` as it stood at the call and can reject (an IPC failure), lose its reply or hang it; the ids come
 * from the fake's `mint` seam, the native MIDI epoch that tells a reload from a new launch from a base the
 * launch sets. Proves, on a stored v0.8.9 profile (the amp-sim in slot A at a saved level, a pad in slot B,
 * RING MOD and ECHO on, REVERB off at a changed level, no OTT key): `lf.rig` is written whole before the
 * first add; the record's units are added in order under the minted ids; no slot level is sent before the
 * record is known, then each slot's as `SetSlotGain`, and no add before the engine took the levels and the
 * legacy sends' send-off, each refused apart (a full command ring) and resent with no reset frame until taken
 * (past the 10 s log point too, one log line);
 * before the first record each slot's fader shows the upgrade's level, and a level set on it lands in the
 * record's first write; the old sends wait in `pending` until the head seats (a held `loading` head
 * keeps them, a slot-level change meanwhile keeps units and `pending`), then are placed into the input-fed
 * slot in RING MOD, ECHO, REVERB order (the OTT first when its key is on), on/off as bypass, while a
 * source-headed slot takes only an OTT that was on; GO LIVE is never pressed; a second launch restores the
 * same ids with no second upgrade; a WebView reload whose reset frame carries the chains adds nothing, and
 * one during the settle keeps the marker until the reloaded page's settle; the marker names the plugin units
 * whose adds were issued, so a leftover one forgets those units only, never touching the legacy keys and
 * keeping a level set meanwhile, in a new launch, nothing when no plugin add was issued yet, and in a reload
 * (before the add reached the host) nothing; a marker storage refuses adds and places nothing; a plugin after
 * an add rejected with no plate (in time) is never tried nor waited for, so the restore finishes and both are
 * restored next launch, a late plate holding its place, and the marker drops the rejected one, so a crash in
 * the other slot's load forgets only that slot's plugin; the settle starts only once each plugin add's outcome is in the
 * feed (a lost reply's included), so a close meanwhile keeps the marker; a lost reply holds its unit's
 * place, so the units after it keep record order; a plugin missing from the scan keeps its entry and its
 * slot's `pending` until it is back; an Off slot keeps its input level and takes the sends; adds that never
 * answer leave the record durable, so the next launch adds each unit once under the same ids; a record that
 * cannot be stored adds nothing and the next launch upgrades, one that reads back other text adds nothing
 * either; one of a version
 * this build does not know is left as it is and adds nothing; a failed plate (its cause unknown) and an IPC
 * rejection both keep the head and its `pending`, retried at the next launch; with no record and a reset
 * frame that already reports a chain, the record adopts it instead of the legacy plugin, at the slot gain
 * the reset frame replays.
 *
 * The native record's handover (H cases, the same context with the rig fake on, `__lfRigFake`, scripted per
 * launch as that launch's process holds the record; its replies go through the page's strict reading of D13's
 * answer, a scripted raw reply included, and the Tauri adapter's source is checked to read both answers so):
 * on `absent` one import, equal to the WebView's upgrade of the same profile, after the device's open answered
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
 * replacing the whole localStorage (a fresh profile); `failRig` makes every store of `lf.rig` throw, and
 * `garbleRig` every read of it answer other text than was stored (a failed read-back); `failMarker` every
 * store of `lf.rigInFlight` throw; `refuse` (engine command names) refuses every batch carrying one of them,
 * as a full command ring would, until `window.__refuse` is cleared, and `refusePartly` takes the rest of such
 * a batch and answers that it dropped some, until `window.__refusePartly` is cleared; `sendDelay` makes every
 * batch take that many ms (the chain context's route of `host.web.ts` gates `InputHost.send`).
 * `install(chain)`, called once `__lf` is up and before the reset frame (the restore waits for its chains,
 * so nothing is minted or added before): `mint` counts from `mintFrom`; `hold` holds every activation
 * (`release()`); `failures` scripts failed plates by id; the add wrapper logs each add with `lf.rig` as
 * stored at the call, rejects the ids in `reject` once as an IPC failure would (a string, no plate),
 * passes the ids in `lose` to the host but rejects them at once (a lost reply: the plate shows), rejects the
 * ids in `late` at once and passes them to the host that many ms later (a plate after the restore's cutoff),
 * and never answers the ids in `hang` (or every add for 'all'). The page's native MIDI epoch counts from `epochBase`
 * (the chain context's route of `host.web.ts`): 0 a new launch, more a WebView reload of the same process.
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
  if (cfg.failRig) {
    const real = Storage.prototype.setItem;
    Storage.prototype.setItem = function (key, value) {
      if (this === window.localStorage && key === 'lf.rig') throw new DOMException('injected: the quota is full', 'QuotaExceededError');
      return real.call(this, key, value);
    };
  }
  if (cfg.failMarker) {
    const real = Storage.prototype.setItem;
    Storage.prototype.setItem = function (key, value) {
      if (this === window.localStorage && key === 'lf.rigInFlight') throw new DOMException('injected: the quota is full', 'QuotaExceededError');
      return real.call(this, key, value);
    };
  }
  window.__refuse = cfg.refuse ?? null;
  window.__refusePartly = cfg.refusePartly ?? null;
  window.__sendDelay = cfg.sendDelay ?? 0;
  // `holdDevice`: every device open's answer waits until `__releaseDevice()` (the device itself runs at once).
  if (cfg.holdDevice) window.__deviceHold = new Promise((resolve) => (window.__releaseDevice = resolve));
  // The native record's handover on (`host.web.ts` `RigFake`), scripted as this launch's process holds it.
  if (cfg.rig) window.__lfRigFake = cfg.rig;
  if (cfg.garbleRig) {
    // A store that lands but reads back other text (the read-back check's case).
    const real = Storage.prototype.getItem;
    Storage.prototype.getItem = function (key) {
      const value = real.call(this, key);
      return this === window.localStorage && key === 'lf.rig' && value !== null ? `${value} ` : value;
    };
  }
  const probe = { adds: [], minted: [], release: () => {} };
  probe.install = (chain) => {
    let next = cfg.mintFrom ?? 1001;
    chain.mint = () => {
      probe.minted.push(next);
      return next++;
    };
    if (cfg.hold) {
      chain.holdLoading = new Promise((resolve) => (probe.release = () => {
        chain.holdLoading = null;
        resolve();
      }));
    }
    for (const [id, why] of Object.entries(cfg.failures ?? {})) chain.failures.set(Number(id), why);
    const rejects = new Set(cfg.reject ?? []);
    const loses = new Set(cfg.lose ?? []);
    const hangs = cfg.hang ?? [];
    const lates = new Map(Object.entries(cfg.late ?? {}).map(([id, ms]) => [Number(id), ms]));
    const real = chain.host.add;
    chain.host.add = (slot, unit, spec, at, toneToken) => {
      probe.adds.push({ slot, unit, spec: structuredClone(spec), at, rig: localStorage.getItem('lf.rig') });
      if (rejects.delete(unit)) return Promise.reject(`chain_add: injected IPC failure for unit ${unit}`);
      if (hangs === 'all' || hangs.includes(unit)) return new Promise(() => {});
      if (lates.has(unit)) {
        // The answer comes first; the host takes the add only later, so its plate shows after the cutoff.
        const ms = lates.get(unit);
        lates.delete(unit);
        setTimeout(() => void real(slot, unit, spec, at, toneToken).catch(() => {}), ms);
        return Promise.reject(`chain_add: injected early rejection for unit ${unit}`);
      }
      if (loses.delete(unit)) {
        // The host takes the add (its plate shows in the next report); its answer is lost on the way.
        void real(slot, unit, spec, at, toneToken).catch(() => {});
        return Promise.reject(`chain_add: injected lost reply for unit ${unit}`);
      }
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

  // ── Chain mode (DEV): the rig record `lf.rig` (unit contract §1, §6) ─────────────────────────────────
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
        const source = await response.text();
        const epoch = 'let fakeEpoch = 0;';
        if (!source.includes(epoch)) throw new Error('Cannot locate the fake\'s MIDI epoch counter');
        const body = source.replace(epoch, 'let fakeEpoch = Number(JSON.parse(sessionStorage.getItem(\'chainProbe\') ?? \'{}\').epochBase ?? 0);')
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
  /** The same profile with slot A Off (no plugin remembered) at a raised input level. */
  const { 'lf.rigRecall': _amp, ...V089_OFF } = { ...V089, 'lf.slotSource.0': 'off', 'lf.slotInputGain.0': '1.2' };
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

  /** What the launch did so far: its adds (in order, and per slot as `<at> <id> <kind>[ off]`), the ids it
   * minted, `lf.rig` (now, and as stored at each add), the markers, the legacy keys, the slot gains sent,
   * any GO LIVE, the toasts and the fake's chains. */
  const chainRead = () => cp.evaluate(() => {
    const p = window.__chainProbe;
    const ls = (k) => localStorage.getItem(k);
    const add = ({ slot, unit, spec, at }) => `${'AB'[slot]}${at} ${unit} ${spec.kind}${spec.bypass ? ' off' : ''}`;
    return {
      adds: p.adds.map(add),
      rigAt: p.adds.map((a) => JSON.parse(a.rig ?? 'null')),
      minted: p.minted,
      rig: JSON.parse(ls('lf.rig') ?? 'null'),
      marker: ls('lf.rigInFlight'),
      legacy: { rigRecall: ls('lf.rigRecall'), inFlight: ls('lf.rigRecallInFlight') },
      gains: window.__lf.native.sent.filter((c) => 'SetSlotGain' in c).map((c) => c.SetSlotGain),
      live: window.__lf.native.sent.filter((c) => c.SetSlotLive?.[1] === true),
      toasts: window.__lf.notify.toasts().map((t) => `${t.message} x${t.count}`),
      chains: window.__lf.native.chain.chains().map((c) => c.units.map((u) => `${u.id} ${u.kind} ${u.state}${u.source ? ' source' : ''}`)),
    };
  });
  /** The adds of slot `letter` in order, `<at> <id> <kind>[ off]`. */
  const addsIn = (c, letter) => c.adds.filter((a) => a.startsWith(letter)).map((a) => a.slice(1));
  const unitsOf = (list = []) => list.map((u) => `${u.id} ${u.kind}${u.bypass ? ' off' : ''}`);
  /** A record as each slot's volume, units and pending, `<id> <kind>[ off]`. */
  const slotsOf = (rig) => rig.slots.map((s) => ({ volume: s.volume, units: unitsOf(s.units), pending: unitsOf(s.pending) }));

  /**
   * Wait for this launch's restore (`rigRecallDone`, the settle included), then for `adds` adds in all and
   * every reported unit out of `loading`; its `[rig-recall]` lines must match `recall` one for one and its
   * toasts be `toasts` (`refused`: batches were refused on purpose, their log lines and toast taken out).
   */
  async function chainSettle({ adds = 0, recall = [], toasts = [], refused = false } = {}) {
    await cp.waitForFunction(() => window.__chainProbe.recall.rigRecallFinished(), undefined, { timeout: 20_000 });
    await cp.waitForFunction((n) => window.__chainProbe.adds.length >= n
      && window.__lf.native.chain.chains().every((c) => c.units.every((u) => u.state !== 'loading')), adds, { timeout: 10_000 });
    // a placement's write and its last report land a moment after its adds
    await cp.waitForTimeout(300);
    const c = await chainRead();
    if (refused) {
      assert.ok(chainApp.consumeConsoleErrors(/^\[platform\] input batch failed/).length > 0, 'the refused batches are logged');
      c.toasts = c.toasts.filter((t) => !t.startsWith('Could not send that to the audio engine.'));
    }
    const recallLogs = chainApp.consumeConsoleErrors(/^\[rig-recall\]/);
    console.log(JSON.stringify({ ...c, rigAt: undefined, rig: c.rig && slotsOf(c.rig), recallLogs }));
    assert.equal(recallLogs.length, recall.length, `[rig-recall] lines: ${JSON.stringify(recallLogs)}`);
    recall.forEach((re, i) => assert.match(recallLogs[i], re));
    assert.deepEqual(c.toasts, toasts, 'toasts');
    assert.equal(c.adds.length, adds, `adds: ${JSON.stringify(c.adds)}`);
    return c;
  }

  const RING = { ringFreq: 220, ringLevel: 0.3 };
  const ECHO = { echoTime: 2, echoFeedback: 0.55, echoLevel: 0.35 };
  const REVERB = { reverbLevel: 0.6 };
  const UPGRADED = [
    { volume: 0.8, units: ['1001 plugin'], pending: ['1002 ring', '1003 echo', '1004 reverb off'] },
    { volume: 1, units: ['1005 synth'], pending: [] },
  ];
  const PLACED_A = ['1001 plugin', '1002 ring', '1003 echo', '1004 reverb off'];
  const RESTORE_ALL = { A: ['0 1001 plugin', '1 1002 ring', '2 1003 echo', '3 1004 reverb off'], B: ['0 1005 synth'] };

  // C1. The upgrade of a v0.8.9 profile, its amp-sim's load held: the record is stored whole before the
  // first add; only the head's add runs while it loads, the sends wait in `pending`, each slot's level
  // goes to the engine, and a slot-level change meanwhile keeps the units and `pending`. Released: the
  // synth, the settle, then the sends placed after the amp in RING MOD, ECHO, REVERB order, REVERB
  // bypassed; slot B, a SYNTH, takes none (no OTT was stored); GO LIVE never pressed.
  await chainLaunch({ seed: V089, hold: true });
  await cp.waitForFunction(() => window.__chainProbe.adds.length === 1 && localStorage.getItem('lf.rigInFlight') !== null);
  await cp.waitForTimeout(300);
  let c = await chainRead();
  assert.deepEqual(c.minted, [1001, 1002, 1003, 1004, 1005], 'the upgrade mints slot A\'s units, its pending, then slot B\'s');
  assert.deepEqual(c.adds, ['A0 1001 plugin'], 'while the head loads, only its add has run');
  assert.ok(c.rigAt[0], 'lf.rig is stored before the first add');
  assert.deepEqual(slotsOf(c.rigAt[0]), UPGRADED, 'lf.rig at the first add: the whole upgraded record');
  // the WebView's upgrade of the v0.8.9 profile: the native handover imports the same record (H1)
  const upgradedV089 = c.rigAt[0];
  assert.deepEqual(slotsOf(c.rig), UPGRADED, 'pending kept while the head loads');
  {
    const [a, b] = c.rig.slots;
    assert.deepEqual(a.units[0].plugin, saved(AMP), 'the plugin unit is the legacy recall\'s, its name inside `plugin`');
    assert.deepEqual(a.pending.map((u) => u.params), [RING, ECHO, REVERB], 'the sends\' params as stored');
    assert.deepEqual([b.units[0].params.instrument, b.units[0].params.gainPad], [2, 0.6], 'slot B: the stored Pad, its stored level');
  }
  // no slot level before the record is known (an Off slot never passes its input at unity first), then
  // each slot's migrated level
  assert.deepEqual(c.gains, [[0, 0.8], [1, 1]], 'each slot\'s level, sent before the adds, and nothing before it');
  assert.equal(c.marker, '[1001]', 'the chain marker names the plugin unit whose add is in flight');
  assert.deepEqual(c.legacy, { rigRecall: V089['lf.rigRecall'], inFlight: null }, 'the legacy recall\'s keys untouched');
  await cp.evaluate(async () => (await import('/src/ui/state/instrument.ts')).setSlotLevel(1, 0.5));
  c = await chainRead();
  assert.deepEqual(slotsOf(c.rig), [UPGRADED[0], { ...UPGRADED[1], volume: 0.5 }], 'a slot-level change mid-restore keeps units and pending');
  await cp.evaluate(() => window.__chainProbe.release());
  c = await chainSettle({ adds: 5 });
  assert.deepEqual(c.adds, ['A0 1001 plugin', 'B0 1005 synth', 'A1 1002 ring', 'A2 1003 echo', 'A3 1004 reverb off'],
    'the record\'s units in order, then the sends once the head seated');
  assert.deepEqual(slotsOf(c.rig), [{ volume: 0.8, units: PLACED_A, pending: [] }, { volume: 0.5, units: ['1005 synth'], pending: [] }]);
  assert.deepEqual(c.rig.slots[0].units.slice(1).map((u) => u.params), [RING, ECHO, REVERB], 'placed with their params');
  assert.deepEqual({ live: c.live, marker: c.marker }, { live: [], marker: null }, 'no GO LIVE; the marker gone after the settle');
  const restored = c.rig;

  // C2. A restart: the same ids, added in order, nothing minted, the record unchanged, the kept level sent.
  await chainLaunch({ mintFrom: 2001 });
  c = await chainSettle({ adds: 5 });
  assert.deepEqual({ A: addsIn(c, 'A'), B: addsIn(c, 'B') }, RESTORE_ALL, 'every unit back under its id');
  assert.deepEqual(c.minted, [], 'no second upgrade');
  assert.deepEqual(c.rig, restored, 'the record unchanged');
  assert.deepEqual([...new Set(c.gains.map((g) => g.join(' ')))], ['0 0.8', '1 0.5'], 'only the kept levels are sent');
  assert.deepEqual({ live: c.live, marker: c.marker }, { live: [], marker: null });

  // C3. A WebView reload: the reset frame carries the chains the native host kept; nothing is added.
  const kept = await cp.evaluate(() => window.__lf.native.chain.chains());
  await chainLaunch({ mintFrom: 2001, chains: kept, epochBase: 1 });
  c = await chainSettle();
  assert.deepEqual({ minted: c.minted, rig: c.rig, marker: c.marker }, { minted: [], rig: restored, marker: null }, 'a WebView reload adds nothing');

  // C4. A WebView reload during the settle: the marker outlives it, nothing is forgotten, and the reloaded
  // page's own settle clears it; the next restart restores everything.
  await chainLaunch({ mintFrom: 2001 });
  await cp.waitForFunction(() => window.__chainProbe.adds.length === 5
    && window.__lf.native.chain.chains().flatMap((ch) => ch.units).every((u) => u.state === 'seated')
    && localStorage.getItem('lf.rigInFlight') !== null && !window.__chainProbe.recall.rigRecallFinished());
  const mid = await cp.evaluate(() => window.__lf.native.chain.chains());
  await chainLaunch({ mintFrom: 2001, chains: mid, epochBase: 1 });
  assert.equal(await cp.evaluate(() => localStorage.getItem('lf.rigInFlight')), '[1001]', 'the marker is kept across a reload during the settle');
  c = await chainSettle();
  assert.deepEqual({ rig: c.rig, marker: c.marker }, { rig: restored, marker: null }, 'nothing forgotten; the reloaded page\'s settle clears the marker');
  await chainLaunch({ mintFrom: 2001 });
  c = await chainSettle({ adds: 5 });
  assert.deepEqual({ A: addsIn(c, 'A'), B: addsIn(c, 'B') }, RESTORE_ALL, 'the next restart restores everything');

  // C5. A launch stopped while its amp-sim loads: the next launch forgets the plugin units only (one log
  // line, one toast), re-adds the built-ins under their ids, and never touches the legacy keys.
  await chainLaunch({ mintFrom: 2001, hold: true });
  await cp.waitForFunction(() => window.__chainProbe.adds.length === 1 && localStorage.getItem('lf.rigInFlight') !== null);
  await chainLaunch({ mintFrom: 2001 });
  c = await chainSettle({
    adds: 4,
    recall: [/^\[rig-recall\] the last launch stopped while restoring Fake Amp \(VST3\); its plugins are forgotten/],
    toasts: ['Could not restore the plugins. Add a plugin again to load it. x1'],
  });
  assert.deepEqual({ A: addsIn(c, 'A'), B: addsIn(c, 'B') }, { A: ['0 1002 ring', '1 1003 echo', '2 1004 reverb off'], B: ['0 1005 synth'] });
  assert.deepEqual(slotsOf(c.rig)[0].units, ['1002 ring', '1003 echo', '1004 reverb off'], 'the plugin unit forgotten, the built-ins kept');
  assert.deepEqual({ minted: c.minted, marker: c.marker, legacy: c.legacy }, { minted: [], marker: null, legacy: { rigRecall: V089['lf.rigRecall'], inFlight: null } },
    'the legacy recall\'s record kept and its marker never written');

  // C6. The amp-sim missing from the scan: one log line, only the synth added, slot A keeps the plugin's
  // entry and its `pending`; back at the next launch, it is added, then the sends placed.
  await chainLaunch({ seed: V089, scan: [FSYN, LOOK] });
  c = await chainSettle({ adds: 1, recall: [/^\[rig-recall\] slot 1: Fake Amp \(VST3\) at C:\\Fake\\FakeAmp\.vst3 is not in this launch's plugin scan/] });
  assert.deepEqual(c.adds, ['B0 1005 synth']);
  assert.deepEqual(slotsOf(c.rig), UPGRADED, 'the missing plugin keeps its entry, its slot its pending');
  await chainLaunch({ mintFrom: 2001 });
  c = await chainSettle({ adds: 5 });
  assert.deepEqual(c.adds, ['A0 1001 plugin', 'B0 1005 synth', 'A1 1002 ring', 'A2 1003 echo', 'A3 1004 reverb off'], 'back: added, then the sends placed');
  assert.deepEqual(slotsOf(c.rig)[0], { volume: 0.8, units: PLACED_A, pending: [] });

  // C7. The scan and the activation disagree (Fake Synth scanned as an effect, activating as a source),
  // with an OTT key off but changed: the level is the effect default, the OTT waits in both slots' pending,
  // the head seats as a source, and no send is placed: each slot's pending goes, the OTT being off.
  await chainLaunch({
    seed: { ...V089, 'lf.rigRecall': JSON.stringify([saved(FSYN), null]), 'lf.inputSend.ott': '0', 'lf.inputSend.ottDepth': '0.7' },
    scan: [AMP, { ...FSYN, isEffect: true }, LOOK],
  });
  c = await chainSettle({ adds: 2 });
  assert.deepEqual(slotsOf(c.rigAt[0]), [
    { volume: 0.9, units: ['1001 plugin'], pending: ['1002 ott off', '1003 ring', '1004 echo', '1005 reverb off'] },
    { volume: 1, units: ['1006 synth'], pending: ['1007 ott off'] },
  ], 'the upgrade: the scan kind\'s level, an eligible OTT in every slot\'s pending');
  assert.equal(c.rigAt[0].slots[0].pending[0].params.ottDepth, 0.7);
  assert.deepEqual(c.adds, ['A0 1001 plugin', 'B0 1006 synth'], 'no send placed in a source-headed chain');
  assert.equal(c.chains[0][0], '1001 plugin seated source', 'the head activated as a source');
  assert.deepEqual(slotsOf(c.rig), [{ volume: 0.9, units: ['1001 plugin'], pending: [] }, { volume: 1, units: ['1006 synth'], pending: [] }], 'pending cleared');

  // C8. The OTT on (a dev profile): slot A places OTT, RING MOD, ECHO, REVERB; slot B, a SYNTH, the OTT only.
  await chainLaunch({ seed: { ...V089, 'lf.inputSend.ott': '1' } });
  c = await chainSettle({ adds: 7 });
  assert.deepEqual({ A: addsIn(c, 'A'), B: addsIn(c, 'B') }, {
    A: ['0 1001 plugin', '1 1002 ott', '2 1003 ring', '3 1004 echo', '4 1005 reverb off'],
    B: ['0 1006 synth', '1 1007 ott'],
  });
  assert.deepEqual(slotsOf(c.rig).map((s) => s.pending), [[], []]);

  // C9. Slot A Off at a raised input level, and every add hanging: the record is durable before any add,
  // nothing seats; the next launch adds each unit under the same ids, then places the sends into the Off
  // slot (no head: the input feeds it).
  await chainLaunch({ seed: V089_OFF, hang: 'all' });
  await cp.waitForFunction(() => window.__chainProbe.adds.length === 1);
  await cp.waitForTimeout(300);
  c = await chainRead();
  const offRecord = [
    { volume: 1.2, units: [], pending: ['1001 ring', '1002 echo', '1003 reverb off'] },
    { volume: 1, units: ['1004 synth'], pending: [] },
  ];
  assert.deepEqual({ adds: c.adds, rig: slotsOf(c.rigAt[0]), chains: c.chains }, { adds: ['B0 1004 synth'], rig: offRecord, chains: [[], []] },
    'the record stored before an add that never answers');
  assert.deepEqual(c.gains.slice(-2), [[0, 1.2], [1, 1]], 'an Off slot\'s level is its input level');
  await chainLaunch({ mintFrom: 2001 });
  c = await chainSettle({ adds: 4 });
  assert.deepEqual({ A: addsIn(c, 'A'), B: addsIn(c, 'B'), minted: c.minted }, { A: ['0 1001 ring', '1 1002 echo', '2 1003 reverb off'], B: ['0 1004 synth'], minted: [] });
  assert.deepEqual(slotsOf(c.rig)[0], { volume: 1.2, units: ['1001 ring', '1002 echo', '1003 reverb off'], pending: [] });

  // C10. The placement's adds hanging: its write landed before them, so the next launch adds each once.
  await chainLaunch({ seed: V089_OFF, hang: [1001, 1002, 1003] });
  c = await chainSettle({ adds: 2 });
  assert.deepEqual(c.adds, ['B0 1004 synth', 'A0 1001 ring'], 'the placement waits on its first add');
  assert.deepEqual(slotsOf(c.rigAt[1])[0], { volume: 1.2, units: ['1001 ring', '1002 echo', '1003 reverb off'], pending: [] },
    'the placement stored before its first add');
  await chainLaunch({ mintFrom: 2001 });
  c = await chainSettle({ adds: 4 });
  await cp.waitForTimeout(500);
  c = await chainRead();
  assert.deepEqual({ A: addsIn(c, 'A'), B: addsIn(c, 'B') }, { A: ['0 1001 ring', '1 1002 echo', '2 1003 reverb off'], B: ['0 1004 synth'] }, 'each unit added once');

  // C11. The record cannot be stored: nothing is added, and the next launch upgrades.
  await chainLaunch({ seed: V089, failRig: true });
  c = await chainSettle({ recall: [/^\[rig-recall\] could not store the rig record; nothing restored/] });
  assert.deepEqual({ adds: c.adds, rig: c.rig }, { adds: [], rig: null });
  await chainLaunch({});
  c = await chainSettle({ adds: 5 });
  assert.deepEqual(c.minted, [1001, 1002, 1003, 1004, 1005], 'the next launch upgrades');
  assert.deepEqual(slotsOf(c.rigAt[0]), UPGRADED);

  // C12. The amp-sim's add ends in a failed plate: one log line, and the plate is kept with its slot's
  // `pending` (the feed names no cause: a plugin's own load and a host failure look alike), the marker gone
  // once its outcome is in; the next launch retries it, then places the sends. An IPC rejection instead (no
  // plate): the head and the pending kept too, retried next launch.
  await chainLaunch({ seed: V089, failures: { 1001: 'injected: the plugin failed to activate' } });
  c = await chainSettle({ adds: 2, recall: [/^\[rig-recall\] slot 1: Fake Amp \(VST3\) was not restored/] });
  assert.deepEqual(c.adds, ['A0 1001 plugin', 'B0 1005 synth']);
  assert.equal(c.chains[0][0], '1001 plugin failed', 'the failed plate in its place');
  assert.deepEqual({ rig: slotsOf(c.rig), marker: c.marker }, { rig: UPGRADED, marker: null }, 'a failed plate keeps the head and its pending');
  await chainLaunch({ mintFrom: 2001 });
  c = await chainSettle({ adds: 5 });
  assert.deepEqual(c.adds, ['A0 1001 plugin', 'B0 1005 synth', 'A1 1002 ring', 'A2 1003 echo', 'A3 1004 reverb off'], 'retried, then placed');
  await chainLaunch({ seed: V089, reject: [1001] });
  c = await chainSettle({ adds: 2, recall: [/^\[rig-recall\] slot 1: Fake Amp \(VST3\) was not restored/] });
  assert.deepEqual(slotsOf(c.rig), UPGRADED, 'an IPC rejection keeps the head and its pending');
  await chainLaunch({ mintFrom: 2001 });
  c = await chainSettle({ adds: 5 });
  assert.deepEqual(c.adds, ['A0 1001 plugin', 'B0 1005 synth', 'A1 1002 ring', 'A2 1003 echo', 'A3 1004 reverb off'], 'retried, then placed');

  // C13. No record, and the first reset frame already reports a chain (a DEV chain run before any record):
  // the record adopts it instead of the legacy plugin and source, the sends go to both slots' pending, and
  // each is placed after what the slot holds. Slot A keeps the gain the engine replays for it; slot B,
  // replayed none, is at unity.
  const look501 = { id: 501, kind: 'plugin', plugin: { format: LOOK.format, path: LOOK.path, id: LOOK.id }, bypass: false, source: false, name: LOOK.name, latency: 64, state: 'seated' };
  await chainLaunch({
    seed: V089,
    chains: [{ units: [look501], latency: 64, held: false }, { units: [], latency: 0, held: false }],
    settings: [{ SetSlotGain: [0, 0.7] }],
  });
  c = await chainSettle({ adds: 6 });
  assert.deepEqual(c.gains, [[0, 0.7], [1, 1]], 'the adopted levels sent');
  assert.deepEqual(c.minted, [1001, 1002, 1003, 1004, 1005, 1006]);
  assert.deepEqual({ A: addsIn(c, 'A'), B: addsIn(c, 'B') }, {
    A: ['1 1001 ring', '2 1002 echo', '3 1003 reverb off'],
    B: ['0 1004 ring', '1 1005 echo', '2 1006 reverb off'],
  }, 'no legacy amp-sim or synth added; the sends placed after each chain');
  assert.deepEqual(slotsOf(c.rig), [
    { volume: 0.7, units: ['501 plugin', '1001 ring', '1002 echo', '1003 reverb off'], pending: [] },
    { volume: 1, units: ['1004 ring', '1005 echo', '1006 reverb off'], pending: [] },
  ], 'the reported chain adopted at the slot gain the engine kept');

  // C14. The record's write lands but reads back other text: nothing is added, no slot level is sent.
  await chainLaunch({ seed: V089_OFF, garbleRig: true });
  c = await chainSettle({ recall: [/^\[rig-recall\] could not store the rig record; nothing restored/] });
  assert.deepEqual({ adds: c.adds, gains: c.gains }, { adds: [], gains: [] }, 'a failed read-back adds nothing');

  // C15. A WebView reload (native MIDI's epoch past 1) after the marker but before the amp-sim's add reached
  // the host (its reset frame reports no chain): not a crash, nothing forgotten, no toast; the reloaded page
  // adds the amp-sim under its id, then places the sends.
  await chainLaunch({ seed: V089, hang: [1001] });
  await cp.waitForFunction(() => window.__chainProbe.adds.length === 1 && localStorage.getItem('lf.rigInFlight') !== null);
  await chainLaunch({ mintFrom: 2001, epochBase: 1 });
  c = await chainSettle({ adds: 5 });
  assert.deepEqual(c.adds, ['A0 1001 plugin', 'B0 1005 synth', 'A1 1002 ring', 'A2 1003 echo', 'A3 1004 reverb off'], 'the reload restores the amp-sim');
  assert.deepEqual({ minted: c.minted, marker: c.marker }, { minted: [], marker: null });

  // C16. The amp-sim's add reaches the host but its reply is lost, its load held: the settle waits for its
  // outcome in the feed, so the marker stays while it loads, a close through the close button included;
  // seated, the settle clears it, then the sends are placed (slot B Off: its own sends, no head).
  await chainLaunch({ seed: { ...V089, 'lf.slotSource.1': 'off' }, hold: true, lose: [1001] });
  await cp.waitForFunction(() => window.__lf.native.chain.chains()[0].units.some((u) => u.id === 1001 && u.state === 'loading'));
  await cp.waitForTimeout(300);
  await cp.evaluate(() => window.__chainProbe.recall.rigRecallOnClose());
  assert.equal(await cp.evaluate(() => localStorage.getItem('lf.rigInFlight')), '[1001]', 'the marker kept while the amp-sim loads, through a close');
  await cp.evaluate(() => window.__chainProbe.release());
  c = await chainSettle({ adds: 7, recall: [/^\[rig-recall\] slot 1: Fake Amp \(VST3\) was not restored/] });
  assert.deepEqual({ A: addsIn(c, 'A'), B: addsIn(c, 'B'), marker: c.marker }, {
    A: ['0 1001 plugin', '1 1002 ring', '2 1003 echo', '3 1004 reverb off'],
    B: ['0 1005 ring', '1 1006 echo', '2 1007 reverb off'],
    marker: null,
  });

  // C17. RING MOD's add seats with its reply lost: ECHO and REVERB go after it, in record order.
  await chainLaunch({ seed: V089, lose: [1002] });
  c = await chainSettle({ adds: 5, recall: [/^\[rig-recall\] slot 1: RING was not placed/] });
  assert.deepEqual(c.adds, ['A0 1001 plugin', 'B0 1005 synth', 'A1 1002 ring', 'A2 1003 echo', 'A3 1004 reverb off']);
  assert.deepEqual({ chain: c.chains[0], units: slotsOf(c.rig)[0].units }, {
    chain: ['1001 plugin seated', '1002 ring seated', '1003 echo seated', '1004 reverb seated'],
    units: PLACED_A,
  }, 'the chain and the record in record order');

  // C18. A restart whose engine refuses, as a briefly full command ring does, every batch carrying the legacy
  // sends' send-off (the slot levels taken), then every one carrying a slot level (the send-off taken): no add
  // meanwhile; the refusal over, with no reset frame between, the resends are taken and every unit comes back.
  const sentKinds = () => cp.evaluate(() => [...new Set(window.__lf.native.sent.map((c) => Object.keys(c)[0]))]);
  for (const [refuse, taken] of [['SetInputSend', 'SetSlotGain'], ['SetSlotGain', 'SetInputSend']]) {
    await chainLaunch({ mintFrom: 2001, refuse: [refuse] });
    await cp.waitForTimeout(1000);
    assert.equal(await cp.evaluate(() => window.__chainProbe.adds.length), 0, `no add while the engine refuses ${refuse}`);
    const kinds = await sentKinds();
    assert.ok(kinds.includes(taken) && !kinds.includes(refuse), `${taken} taken, ${refuse} not: ${JSON.stringify(kinds)}`);
    await cp.evaluate(() => void (window.__refuse = null));
    c = await chainSettle({ adds: 5, refused: true });
    assert.deepEqual({ A: addsIn(c, 'A'), B: addsIn(c, 'B') }, RESTORE_ALL, `${refuse} resent and taken with no reset frame`);
  }

  // C19. A slot level set on the slot's fader before the first record exists (the upgrade's scan of the legacy
  // plugin held): the fader shows the level the upgrade derives from the old keys, nothing is sent to the
  // engine and no record written meanwhile, and the upgrade's first write takes the player's level.
  await chainLaunch({ seed: V089, holdPaths: true });
  await cp.waitForFunction(() => window.__recallProbe.calls.paths.length === 1);
  const faderB = cp.getByRole('slider', { name: 'Volume for slot B', exact: true });
  await faderB.waitFor({ timeout: 5000 });
  assert.deepEqual([await cp.getByRole('slider', { name: 'Volume for slot A', exact: true }).inputValue(), await faderB.inputValue()], ['0.8', '1'],
    'each fader at the upgrade\'s level before the record');
  await faderB.fill('0.5');
  await cp.waitForTimeout(300);
  c = await chainRead();
  assert.deepEqual({ gains: c.gains, rig: c.rig }, { gains: [], rig: null }, 'no slot level sent, no record, while the upgrade waits');
  await cp.evaluate(() => window.__recallProbe.releasePaths());
  c = await chainSettle({ adds: 5 });
  assert.deepEqual([c.rigAt[0].slots[1].volume, c.rig.slots[1].volume], [0.5, 0.5], 'the level set meanwhile, in the first write and after');
  assert.deepEqual(c.gains.slice(0, 2), [[0, 0.8], [1, 0.5]], 'then each slot\'s level, the player\'s included');

  // C20. A stored record of a version this build does not know: one log line, left as it is, nothing added,
  // no slot level sent.
  const future = JSON.stringify({ version: 2, slots: [] });
  await chainLaunch({ seed: { ...V089, 'lf.rig': future } });
  c = await chainSettle({ recall: [/^\[rig-recall\] the rig record cannot be read/] });
  assert.deepEqual({ adds: c.adds, gains: c.gains, rig: await cp.evaluate(() => localStorage.getItem('lf.rig')) }, { adds: [], gains: [], rig: future });

  /** `lf.rig` as stored: each slot `[volume, units, pending]`; units by `<id> <kind>` (`plugin` the AMP, `look`
   * the Fake Lookahead, `synth` a Pad, `echo` the v0.8.9 ECHO). */
  const rigText = (...slots) => {
    const unit = (u) => {
      const [id, kind] = u.split(' ');
      if (kind === 'plugin' || kind === 'look') return { id: Number(id), kind: 'plugin', plugin: saved(kind === 'look' ? LOOK : AMP), bypass: false };
      if (kind === 'synth') return { id: Number(id), kind: 'synth', params: { instrument: 2 }, bypass: false };
      return { id: Number(id), kind, params: ECHO, bypass: false };
    };
    return JSON.stringify({ version: 1, slots: slots.map(([volume, units, pending = []]) => ({ volume, units: units.map(unit), ...(pending.length ? { pending: pending.map(unit) } : {}) })) });
  };
  // Slot A: the amp-sim, then the lookahead, ECHO still to place; slot B: a SYNTH.
  const TWO_PLUGINS = rigText([1, ['1001 plugin', '1002 look'], ['1003 echo']], [1, ['1004 synth']]);
  const TWO_KEPT = [
    { volume: 1, units: ['1001 plugin', '1002 plugin'], pending: ['1003 echo'] },
    { volume: 1, units: ['1004 synth'], pending: [] },
  ];

  // C21. Two plugins in one slot, the first's add rejected with no plate: the second is never tried (its index
  // would be a guess) and not waited for, so the restore finishes; both kept with the slot's `pending`, the
  // marker gone; the next launch forgets nothing and restores both, then places ECHO.
  await chainLaunch({ seed: { 'lf.rig': TWO_PLUGINS }, mintFrom: 2001, reject: [1001] });
  c = await chainSettle({ adds: 2, recall: [/^\[rig-recall\] slot 1: Fake Amp \(VST3\) was not restored/] });
  assert.deepEqual(c.adds, ['A0 1001 plugin', 'B0 1004 synth'], 'the second plugin never tried');
  assert.deepEqual({ rig: slotsOf(c.rig), marker: c.marker }, { rig: TWO_KEPT, marker: null }, 'both kept, the marker gone');
  await chainLaunch({ mintFrom: 2001 });
  c = await chainSettle({ adds: 4 });
  assert.deepEqual({ A: addsIn(c, 'A'), B: addsIn(c, 'B') }, { A: ['0 1001 plugin', '1 1002 plugin', '2 1003 echo'], B: ['0 1004 synth'] },
    'nothing forgotten: both restored, then ECHO placed');

  // C22. The same, the first add's plate showing only after the restore's cutoff: the second waits for the next
  // launch, the late plate seats and holds its place, and ECHO is placed after it; the record keeps record
  // order, so the next launch restores all three in it.
  await chainLaunch({ seed: { 'lf.rig': TWO_PLUGINS }, mintFrom: 2001, late: { 1001: 800 } });
  c = await chainSettle({ adds: 3, recall: [/^\[rig-recall\] slot 1: Fake Amp \(VST3\) was not restored/] });
  assert.deepEqual(c.adds, ['A0 1001 plugin', 'B0 1004 synth', 'A1 1003 echo'], 'the second plugin never tried; ECHO after the late plate');
  assert.deepEqual({ chain: c.chains[0], units: slotsOf(c.rig)[0].units, marker: c.marker },
    { chain: ['1001 plugin seated', '1003 echo seated'], units: ['1001 plugin', '1002 plugin', '1003 echo'], marker: null });
  await chainLaunch({ mintFrom: 2001 });
  c = await chainSettle({ adds: 4 });
  assert.deepEqual(addsIn(c, 'A'), ['0 1001 plugin', '1 1002 plugin', '2 1003 echo'], 'the next launch restores in record order');

  // C23. A built-in's add in slot A hangs before slot B's plugin add was issued: the marker names no plugin,
  // so the next launch forgets nothing (no log line, no toast) and restores both.
  await chainLaunch({ seed: { 'lf.rig': rigText([1, ['1001 synth']], [1, ['1002 plugin']]) }, mintFrom: 2001, hang: [1001] });
  await cp.waitForFunction(() => window.__chainProbe.adds.length === 1 && localStorage.getItem('lf.rigInFlight') !== null);
  assert.equal(await cp.evaluate(() => localStorage.getItem('lf.rigInFlight')), '[]', 'the marker names no plugin before a plugin add');
  await chainLaunch({ mintFrom: 2001 });
  c = await chainSettle({ adds: 2 });
  assert.deepEqual({ adds: c.adds, units: slotsOf(c.rig)[1].units }, { adds: ['A0 1001 synth', 'B0 1002 plugin'], units: ['1002 plugin'] },
    'the plugin never issued is kept and restored');

  // C24. A leftover marker in a new launch, the engine refusing the send-off a while: slot B's level set on its
  // fader meanwhile is kept when the record forgets the marked plugin.
  await chainLaunch({
    seed: { 'lf.rig': rigText([0.8, ['1001 plugin']], [1, ['1002 synth']]), 'lf.rigInFlight': '[1001]' },
    mintFrom: 2001,
    refuse: ['SetInputSend'],
  });
  await cp.waitForTimeout(300);
  await cp.getByRole('slider', { name: 'Volume for slot B', exact: true }).fill('0.5');
  assert.equal(await cp.evaluate(() => JSON.parse(localStorage.getItem('lf.rig')).slots[1].volume), 0.5, 'the level stored meanwhile');
  await cp.evaluate(() => void (window.__refuse = null));
  c = await chainSettle({
    adds: 1,
    refused: true,
    recall: [/^\[rig-recall\] the last launch stopped while restoring Fake Amp \(VST3\); its plugins are forgotten/],
    toasts: ['Could not restore the plugins. Add a plugin again to load it. x1'],
  });
  assert.deepEqual(slotsOf(c.rig), [{ volume: 0.8, units: [], pending: [] }, { volume: 0.5, units: ['1002 synth'], pending: [] }],
    'the plugin forgotten, the level set meanwhile kept');

  // C25. Storage refuses the in-flight marker: nothing is added and nothing placed this launch (slot B Off,
  // its sends pending), the record stored; the next launch restores and places.
  await chainLaunch({ seed: { ...V089, 'lf.slotSource.1': 'off' }, failMarker: true });
  c = await chainSettle({ recall: [/^\[rig-recall\] could not store the in-flight marker; nothing restored/] });
  assert.deepEqual({ adds: c.adds, pending: slotsOf(c.rig).map((sl) => sl.pending.length) }, { adds: [], pending: [3, 3] }, 'nothing added or placed');

  // C26. Slot A's plugin add rejected with no plate, then slot B's plugin crashing in its load (its add
  // hangs, the marker left): the marker no longer names A's unit, so the next launch forgets only B's; A's
  // unit stays in the record under its id (its tone is the id's) and is restored.
  await chainLaunch({ seed: { 'lf.rig': rigText([1, ['1001 plugin']], [1, ['1002 look']]) }, mintFrom: 2001, reject: [1001], hang: [1002] });
  await cp.waitForFunction(() => window.__chainProbe.adds.length === 2);
  await cp.waitForTimeout(300);
  assert.equal(await cp.evaluate(() => localStorage.getItem('lf.rigInFlight')), '[1002]', 'the marker names only the add that reached the host');
  {
    const lines = chainApp.consumeConsoleErrors(/^\[rig-recall\]/);
    assert.equal(lines.length, 1, `[rig-recall] lines: ${JSON.stringify(lines)}`);
    assert.match(lines[0], /^\[rig-recall\] slot 1: Fake Amp \(VST3\) was not restored/);
  }
  await chainLaunch({ mintFrom: 2001 });
  c = await chainSettle({
    adds: 1,
    recall: [/^\[rig-recall\] the last launch stopped while restoring Fake Lookahead \(VST2\); its plugins are forgotten/],
    toasts: ['Could not restore the plugins. Add a plugin again to load it. x1'],
  });
  assert.deepEqual({ adds: c.adds, rig: slotsOf(c.rig) }, {
    adds: ['A0 1001 plugin'],
    rig: [{ volume: 1, units: ['1001 plugin'], pending: [] }, { volume: 1, units: [], pending: [] }],
  }, 'only B\'s plugin forgotten; A\'s kept under its id and restored');

  // C27. The engine refuses the legacy sends' send-off for longer than the 10 s log point, then takes it with
  // no reset frame between: one log line, no add meanwhile, then the restore goes on.
  await chainLaunch({ seed: { 'lf.rig': rigText([1, ['1001 plugin']], [1, ['1002 synth']]) }, mintFrom: 2001, refuse: ['SetInputSend'] });
  await cp.waitForTimeout(11_500);
  assert.equal(await cp.evaluate(() => window.__chainProbe.adds.length), 0, 'no add while the send-off is refused');
  {
    const lines = chainApp.consumeConsoleErrors(/^\[instrument\] /);
    assert.equal(lines.length, 1, `[instrument] lines: ${JSON.stringify(lines)}`);
    assert.match(lines[0], /^\[instrument\] the legacy sends' send-off was refused for 10 s; still sending it/);
  }
  await cp.evaluate(() => void (window.__refuse = null));
  c = await chainSettle({ adds: 2, refused: true });
  assert.deepEqual(c.adds, ['A0 1001 plugin', 'B0 1002 synth'], 'taken past 10 s with no reset frame: the restore goes on');

  // ── The native record's handover (D12, D13; `rig-recall.ts` `openRig`, `importRig`) ───────────────────
  // The same chain context with the rig fake on (`__lfRigFake`, scripted per launch as that launch's process
  // holds the record): the WebView's chain recall stays out, the page opens the record before the device and
  // imports the upgrade only on `absent`.

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

  // H1. Absent: one import, equal to the WebView's upgrade of the same v0.8.9 profile (no reported chains),
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
  assert.deepEqual(r.calls, [['open'], ['import', upgradedV089]], 'open, then one import of the upgrade');
  assert.deepEqual({ adds: r.adds, gains: r.gains, toasts: r.toasts, faders: r.faders }, { adds: 0, gains: [], toasts: [], faders: ['0.8', '1'] });
  const imported = r;

  // H3. A WebView reload of that process: open again, no import; the faders show open's volumes, but where the
  // reset frame replays a slot gain (the engine's settings memory), that one.
  await chainLaunch({ mintFrom: 2001, epochBase: 1, rig: held(imported) });
  r = await rigSettle();
  assert.deepEqual({ calls: r.calls, faders: r.faders, gains: r.gains }, { calls: [['open']], faders: ['0.8', '1'], gains: [] }, 'a reload imports nothing');
  await chainLaunch({ mintFrom: 2001, epochBase: 1, rig: held(imported), settings: [{ SetSlotGain: [1, 0.6] }] });
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
  await chainLaunch({ epochBase: 1, rig: held(r) });
  r = await rigSettle({ recall: [/^\[rig-recall\] the last launch stopped while restoring Fake Amp/] });
  assert.deepEqual({ calls: r.calls, toasts: r.toasts, notice: r.state.notice, seenSaw: r.seenSaw }, { calls: [['open'], ['seen', 7]], toasts: [FORGOT], notice: null, seenSaw: 1 },
    'a reload before seen shows it again, then seen; confirmed now');
  await chainLaunch({ epochBase: 2, rig: held(r) });
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
    seed: { ...V089, 'lf.rig': TWO_PLUGINS, 'lf.rigInFlight': '[1001]' },
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
