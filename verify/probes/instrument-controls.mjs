/**
 * Rendered regression coverage for keyboard state, plugin controls and the MIDI device list, on the web
 * engine fake (`src/platform/host.web.ts`, the engine-seam pattern) with the native slot chrome on
 * (host.web.ts served with `available: true`): native MIDI's port list (scripted with
 * `__lf.native.midiEmit`) reads in the Audio Settings diagnostics and the command bar's status lamp, a port
 * another program holds says so, and LEARN is enabled only while a port is open; both empty native slots
 * show the install hint (in sans, as prose), the keyboard's hide cap reads the one close glyph ✕, the keyboard
 * octave survives a placement remount, PARAMS wears the shared SVG caret (turned over open), the slot volume (a
 * plugin's output gain) honors the unity detent under keyboard steps and slider input, and with one
 * plugin file in both slots, a second EDITOR pressed while the first open is in flight is refused (one
 * open reaches the host), the owner slot reloading that file while the other slot keeps it refuses both
 * slots' EDITOR until both unload it (also while the other slot's unload or load is in flight), any native
 * load or unload error on the file (a failed or timed-out load, a failed unload, a teardown error whose
 * next clear succeeds) keeps it refused for the rest of the run, and an open that fails late, after its
 * slot unloaded the plugin and reserved its editor again, leaves that newer reservation standing, and after a
 * WebView reload whose startup resync failed to unload a stranded plugin, its file's EDITOR is refused in both
 * slots although the reloaded page saw no editor open. The plugin host and
 * native MIDI are simulated in the page; no native or hardware claim (which port Windows lets the app open
 * is native MIDI's, `src-tauri/src/engine_io/midi/ports.rs`). Run: pnpm probe instrument-controls
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

await probe(async ({ open }) => {
  const { page, consumeConsoleErrors } = await open({
    viewport: { width: 1280, height: 820 },
    init: async (p) => {
      await p.addInitScript(() => void (window.__lfEngineFake = true));
      await p.route('**/src/platform/host.web.ts*', async (route) => {
        const response = await route.fetch();
        const body = await response.text();
        if (!body.includes('available: false')) throw new Error('Could not enable simulated native chrome');
        // The reload case's stranded host: `window.__stranded` (set by an init script before the reload) is
        // what the host says it still holds at boot, and unloading such a slot fails.
        const hooks = [
          ['async listLoaded() {', 'async listLoaded() { if (window.__stranded) return window.__stranded;'],
          ['async unloadPlugin() {', "async unloadPlugin(slot) { if (window.__stranded?.some((s) => s.slot === slot)) throw new Error('the stranded plugin would not unload (probe)');"],
        ];
        let patched = body.replace('available: false', 'available: true');
        for (const [from, to] of hooks) {
          if (!patched.includes(from)) throw new Error(`Could not hook the simulated plugin host: ${from}`);
          patched = patched.replace(from, to);
        }
        await route.fulfill({ response, body: patched });
      });
    },
  });

  await page.waitForFunction(
    () => document.querySelector('[aria-label="Rescan plugins"]')?.disabled === false,
  );

  // The MIDI device list: what native MIDI's ports event says, a held port named as such.
  await page.evaluate(() => window.__lf.ui.openSettings('midi'));
  const LEARN = page.locator('[aria-label="Learn a MIDI control for this action"]');
  const midiRow = page.locator('.audio-settings__diag-row', { hasText: /^midi/ }).locator('b');
  await LEARN.waitFor();
  assert.equal(await midiRow.textContent(), 'no devices', 'no port: no devices');
  assert.equal(await LEARN.isDisabled(), true, 'LEARN waits for an open port');
  const ports = (list) => page.evaluate((p) => window.__lf.native.midiEmit({ ports: { ports: p } }), list);
  await ports([{ id: 'busy', name: 'Keystation 49', state: 'busy' }]);
  await page.waitForFunction(() => document.querySelector('.audio-settings__diag')?.textContent.includes('Keystation'));
  assert.equal(await midiRow.textContent(), 'Keystation 49 (held by another program)', 'a port another program holds says so');
  assert.equal(await LEARN.isDisabled(), true, 'a held port cannot learn');
  await ports([{ id: 'pedal', name: 'FS-6 Pedal', state: 'open' }, { id: 'busy', name: 'Keystation 49', state: 'busy' }, { id: 'gone', name: 'Launchkey', state: 'closed' }]);
  await page.waitForFunction(() => document.querySelector('.audio-settings__diag')?.textContent.includes('FS-6'));
  assert.equal(await midiRow.textContent(), 'FS-6 Pedal, Keystation 49 (held by another program), Launchkey (closed)');
  assert.equal(await LEARN.isDisabled(), false, 'an open port can learn');
  const lamp = await page.locator('[title^="host:"]').first().getAttribute('title');
  assert.ok(lamp?.includes('midi: FS-6 Pedal, Keystation 49 (held by another program)'), `the status lamp lists the ports: ${lamp}`);
  await page.evaluate(() => window.__lf.ui.closeSettings());

  const emptyPluginCopy =
    'No plugins found · the standard CLAP, VST3 and VST2 folders are scanned · add your own in Audio Settings · rescan ⟳ in the command bar';
  await page.waitForFunction(
    (copy) => [...document.querySelectorAll('[role="note"]')].filter((element) => element.textContent?.trim() === copy).length === 2,
    emptyPluginCopy,
  );
  assert.deepEqual(
    await page.locator('[role="note"]').allTextContents(),
    [emptyPluginCopy, emptyPluginCopy],
    'both empty native slots must show the plugin installation hint',
  );
  // The hint is prose: Geist, not the mono kept for changing numerals.
  const noteFonts = await page.locator('[role="note"]').evaluateAll((els) => els.map((el) => getComputedStyle(el).fontFamily));
  console.log('install hint fonts', JSON.stringify(noteFonts));
  assert.ok(noteFonts.every((f) => f.startsWith('Geist') && !f.includes('Mono')), `the install hint is set in ${noteFonts[0]}`);

  // The keyboard's hide cap is the app's one close glyph, ✕ (the FX drawer, a MIDI binding, a plugin folder).
  assert.equal((await page.getByRole('button', { name: 'Hide keyboard', exact: true }).textContent()).trim(), '✕', 'hide keyboard reads ✕');

  const octaveDown = page.getByRole('button', { name: 'Octave down', exact: true });
  await octaveDown.click();
  await octaveDown.click();
  assert.equal(await page.locator('.kb__base').textContent(), 'C2');
  await page.evaluate(() => window.__lf.layoutStore.setKeyboardPlacement('hidden'));
  await page.locator('.kb').waitFor({ state: 'detached' });
  await page.evaluate(() => window.__lf.layoutStore.setKeyboardPlacement('bottom'));
  await page.locator('.kb').waitFor();
  assert.equal(await page.locator('.kb__base').textContent(), 'C2', 'octave must survive a keyboard remount');

  await page.evaluate(async () => {
    const { platform } = await import('/src/platform/index.ts');
    const instrument = await import('/src/ui/state/instrument.ts');
    const descriptor = {
      id: 'instrument-controls',
      name: 'Instrument Controls Probe',
      path: 'C:\\probe\\instrument-controls.vst3',
      format: 'vst3',
      isEffect: true,
    };
    platform.pluginHost.loadPlugin = async (slot) => ({ slot, descriptor });
    platform.pluginHost.listParams = async () => [];
    platform.pluginHost.openEditor = async () => {};
    await instrument.selectPlugin(0, descriptor);
    window.__gain = () => instrument.pluginGain()[0];
  });

  const paramsButton = page.getByRole('button', { name: /Plugin parameters for slot A/ });
  // PARAMS wears the app's one caret (--caret-svg, the EDIT pill's), not a text glyph, turned over open.
  const paramsCaret = () =>
    paramsButton.evaluate((el) => {
      const after = getComputedStyle(el, '::after');
      return { text: el.textContent.trim(), mask: after.maskImage, transform: after.transform };
    });
  const caretClosed = await paramsCaret();
  assert.equal(caretClosed.text, 'PARAMS', `PARAMS carries no text caret: ${caretClosed.text}`);
  assert.ok(caretClosed.mask.includes('M3 6l5 5 5-5'), `PARAMS wears the shared SVG caret: ${caretClosed.mask.slice(0, 60)}`);
  assert.equal(caretClosed.transform, 'none', 'the caret points down while the drawer is shut');
  await paramsButton.click();
  await page.waitForFunction(() => getComputedStyle(document.querySelector('.slot__params.on'), '::after').transform !== 'none');
  const output = page.getByRole('slider', { name: 'Volume for slot A', exact: true });
  await page.evaluate(() => window.__lf.setPluginGain(1, 0));
  await page.waitForFunction(() => window.__gain() === 1);
  await output.focus();
  await page.keyboard.press('ArrowRight');
  await page.keyboard.press('ArrowRight');
  await page.keyboard.press('ArrowRight');
  assert.ok(
    await page.evaluate(() => window.__gain() > 1),
    'keyboard steps must leave the unity detent',
  );

  await output.evaluate((element) => {
    element.value = '1.05';
    element.dispatchEvent(new Event('input', { bubbles: true }));
  });
  assert.equal(await page.evaluate(() => window.__gain()), 1.05);
  await page.keyboard.press('ArrowLeft');
  await page.keyboard.press('ArrowLeft');
  assert.equal(
    await page.evaluate(() => window.__gain()),
    1,
    'approaching unity must retain the soft detent',
  );

  // Two editor opens racing: the same plugin file in both slots shares one GUI runtime, so only the
  // first open may run. A's open is held in flight while B's EDITOR is pressed: B is refused at once,
  // the host sees one open, and one editor results once A's lands.
  await page.evaluate(async () => {
    const { platform } = await import('/src/platform/index.ts');
    const instrument = await import('/src/ui/state/instrument.ts');
    const descriptor = instrument.slotPlugins()[0];
    await instrument.selectPlugin(1, descriptor);
    window.__opens = [];
    let land;
    window.__landOpen = () => land();
    const held = new Promise((resolve) => (land = resolve));
    platform.pluginHost.openEditor = async (slot) => {
      window.__opens.push(slot);
      await held;
    };
  });
  const editor = (letter, pressed) => page.getByRole('button', { name: `Editor for slot ${letter}`, exact: true, ...(pressed === undefined ? {} : { pressed }) });
  await editor('B').waitFor();
  await editor('A').click();
  await page.waitForFunction(() => window.__opens.length === 1);
  await editor('B').click();
  await page.locator('.toast', { hasText: 'Could not open a second editor for this plugin.' }).waitFor({ timeout: 5000 });
  await page.evaluate(() => window.__landOpen());
  await editor('A', true).waitFor();
  assert.deepEqual(await page.evaluate(() => window.__opens), [0], 'the host saw one editor open: slot A, which pressed first');
  assert.equal(await editor('B').getAttribute('aria-pressed'), 'false', "slot B's racing open was refused");

  // The owner reloads while the other slot keeps the module: A closes its editor, then reloads the
  // plugin in place (a session import's tone reload) while B still holds the file. The module's GUI
  // runtime stays bound to A's old owner thread, which the reload ended, so A's EDITOR is refused now
  // too. Once both slots unload the file, the module goes, and a fresh load in A opens its editor again.
  const dismissToasts = () =>
    page.evaluate(async () => {
      const notify = await import('/src/notify.ts');
      for (const t of notify.toasts()) notify.dismissToast(t.id);
    });
  await editor('A', true).click();
  await editor('A', false).waitFor();
  assert.equal(
    await page.evaluate(async () => {
      const instrument = await import('/src/ui/state/instrument.ts');
      const descriptor = instrument.slotPlugins()[0];
      return instrument.reloadPlugin(0, descriptor, instrument.slotSourceGeneration(0), 1);
    }),
    'reloaded',
    'slot A reloads its plugin in place while slot B holds the same file',
  );
  await dismissToasts();
  await editor('A', false).click();
  await page.locator('.toast', { hasText: 'Could not open a second editor for this plugin.' }).waitFor({ timeout: 5000 });
  assert.equal(await editor('A').getAttribute('aria-pressed'), 'false', "slot A's reloaded editor was refused");
  await dismissToasts();
  await editor('B').click();
  await page.locator('.toast', { hasText: 'Could not open a second editor for this plugin.' }).waitFor({ timeout: 5000 });
  assert.equal(await editor('B').getAttribute('aria-pressed'), 'false', "slot B stays refused");
  assert.deepEqual(await page.evaluate(() => window.__opens), [0], 'no open reached the host after the reload');
  await page.evaluate(async () => {
    const instrument = await import('/src/ui/state/instrument.ts');
    const descriptor = instrument.slotPlugins()[0];
    await instrument.clearPlugin(1);
    await instrument.clearPlugin(0);
    await instrument.selectPlugin(0, descriptor);
  });
  await dismissToasts();
  await editor('A', false).click();
  await editor('A', true).waitFor();
  assert.deepEqual(await page.evaluate(() => window.__opens), [0, 0], 'a fresh load after the last holder unloaded opens its editor');

  // The other slot's unload counts as holding the file until the host is done with it: B's native unload
  // is held in flight while A, the owner, reloads. A's reload must still find B holding the module (its
  // descriptor already reads empty), so A's EDITOR is refused. Then the same with B's unload failing:
  // B keeps the module, and both slots stay refused.
  const holdUnloadB = () =>
    page.evaluate(async () => {
      const { platform } = await import('/src/platform/index.ts');
      const instrument = await import('/src/ui/state/instrument.ts');
      window.__unloadHost ??= platform.pluginHost.unloadPlugin;
      let settle;
      const held = new Promise((resolve, reject) => (settle = { resolve, reject }));
      window.__settleUnloadB = (ok) => (ok ? settle.resolve() : settle.reject(new Error('the host could not unload (probe)')));
      platform.pluginHost.unloadPlugin = async (slot) => {
        if (slot !== 1) return window.__unloadHost(slot);
        window.__unloadBInFlight = true;
        await held;
      };
      window.__unloadBInFlight = false;
      window.__clearB = instrument.clearPlugin(1);
    });
  const reloadA = () =>
    page.evaluate(async () => {
      const instrument = await import('/src/ui/state/instrument.ts');
      return instrument.reloadPlugin(0, instrument.slotPlugins()[0], instrument.slotSourceGeneration(0), 1);
    });
  const settleUnloadB = (ok) =>
    page.evaluate(async (ok) => {
      window.__settleUnloadB(ok);
      await window.__clearB;
    }, ok);
  const expectRefused = async (letter, why, toast = 'Could not open a second editor for this plugin.') => {
    await dismissToasts();
    await editor(letter, false).click();
    await page.locator('.toast', { hasText: toast }).waitFor({ timeout: 5000 });
    assert.equal(await editor(letter).getAttribute('aria-pressed'), 'false', why);
  };
  // A owns the editor (open from the block above); it closes, and B loads the file again.
  await editor('A', true).click();
  await editor('A', false).waitFor();
  await page.evaluate(async () => (await import('/src/ui/state/instrument.ts')).selectPlugin(1, (await import('/src/ui/state/instrument.ts')).slotPlugins()[0]));
  await holdUnloadB();
  await page.waitForFunction(() => window.__unloadBInFlight);
  assert.equal(await reloadA(), 'reloaded', "slot A reloads while slot B's unload is still in flight");
  await expectRefused('A', "slot A's reloaded editor is refused while B's unload is in flight");
  await settleUnloadB(true);
  // B's unload landed after A's reload re-took the module, so the module never went: A stays refused.
  await expectRefused('A', "slot A stays refused: its reload never let the module go");
  assert.deepEqual(await page.evaluate(() => window.__opens), [0, 0], 'no open reached the host during the overlap');

  // From here on each case takes its own file: an error leaves its file guarded for the rest of the run.
  // `ownFile`: both slots let go of what they hold, A loads a fresh file and opens then closes its editor
  // (A owns it), and the host's open count starts over.
  const ownFile = async (name) => {
    await page.evaluate(async (name) => {
      const instrument = await import('/src/ui/state/instrument.ts');
      window.__desc = { id: `${name}-probe`, name: `${name} probe`, path: `C:\\probe\\${name}.vst3`, format: 'vst3', isEffect: true };
      await instrument.clearPlugin(1);
      await instrument.clearPlugin(0);
      await instrument.selectPlugin(0, window.__desc);
    }, name);
    await dismissToasts();
    await editor('A', false).click();
    await editor('A', true).waitFor();
    await editor('A', true).click();
    await editor('A', false).waitFor();
    await page.evaluate(() => (window.__opens = []));
  };
  const loadB = () => page.evaluate(async () => (await import('/src/ui/state/instrument.ts')).selectPlugin(1, window.__desc));

  // B's unload fails while A, the owner, reloads: B keeps the module, and both slots stay refused.
  await ownFile('failed-unload');
  await loadB();
  await holdUnloadB();
  await page.waitForFunction(() => window.__unloadBInFlight);
  assert.equal(await reloadA(), 'reloaded', "slot A reloads while slot B's unload is in flight (it will fail)");
  await settleUnloadB(false);
  assert.equal(consumeConsoleErrors('[instrument] plugin unload failed').length, 1, "B's failed unload reaches the release log");
  assert.ok(await page.evaluate(async () => (await import('/src/ui/state/instrument.ts')).slotPlugins()[1] !== null), 'slot B keeps the plugin');
  await expectRefused('B', "slot B, which kept the module, is refused: A's ended load owns its editor");
  await expectRefused('A', "slot A's reloaded editor is refused");
  assert.deepEqual(await page.evaluate(() => window.__opens), [], 'no open reached the host after the failed unload');
  await page.evaluate(async () => {
    const { platform } = await import('/src/platform/index.ts');
    platform.pluginHost.unloadPlugin = window.__unloadHost;
  });

  // An unload error may leave the module loaded although the host's slot reads empty (a teardown that
  // leaked the plugin, `src-tauri/src/host/engine_slot.rs`): B's unload errors after A's reload, B's next
  // clear succeeds, A clears too, and A's fresh load is still refused.
  await ownFile('teardown');
  await loadB();
  assert.equal(await reloadA(), 'reloaded', 'slot A reloads while slot B holds the file');
  await page.evaluate(async () => {
    const { platform } = await import('/src/platform/index.ts');
    const instrument = await import('/src/ui/state/instrument.ts');
    const unloadHost = platform.pluginHost.unloadPlugin;
    let failed = false;
    platform.pluginHost.unloadPlugin = async (slot) => {
      if (slot === 1 && !failed) {
        failed = true;
        throw new Error('engine slot 1 teardown: the unit did not come back (probe)');
      }
      return unloadHost(slot);
    };
    await instrument.clearPlugin(1);
    await instrument.clearPlugin(1);
    platform.pluginHost.unloadPlugin = unloadHost;
    await instrument.clearPlugin(0);
    await instrument.selectPlugin(0, window.__desc);
  });
  assert.equal(consumeConsoleErrors('[instrument] plugin unload failed').length, 1, "B's teardown error reaches the release log");
  await expectRefused('A', "slot A's fresh load is refused: B's teardown may have left the module loaded");
  assert.deepEqual(await page.evaluate(() => window.__opens), [], "no open reached the host after B's teardown error");

  // A load counts as holding the file from the moment it starts: B's native load is held in flight (the
  // host opens the module before it answers) while A, the owner, reloads. A's unload must still find B
  // holding the module, so A's EDITOR is refused, and so is B's once its load lands.
  await ownFile('held-load');
  await page.evaluate(async () => {
    const { platform } = await import('/src/platform/index.ts');
    const instrument = await import('/src/ui/state/instrument.ts');
    const loadHost = platform.pluginHost.loadPlugin;
    let release;
    const held = new Promise((resolve) => (release = resolve));
    window.__releaseLoadB = release;
    window.__loadBInFlight = false;
    platform.pluginHost.loadPlugin = async (slot, ...rest) => {
      if (slot === 1) {
        window.__loadBInFlight = true;
        await held;
      }
      return loadHost(slot, ...rest);
    };
    window.__restoreLoad = () => (platform.pluginHost.loadPlugin = loadHost);
    window.__loadB = instrument.selectPlugin(1, window.__desc);
  });
  await page.waitForFunction(() => window.__loadBInFlight);
  assert.equal(await reloadA(), 'reloaded', "slot A reloads while slot B's load is still in flight");
  await expectRefused('A', "slot A's reloaded editor is refused while B's load is in flight");
  await page.evaluate(async () => {
    window.__releaseLoadB();
    await window.__loadB;
    window.__restoreLoad();
  });
  await expectRefused('B', "slot B, whose load held the module across A's reload, is refused");
  await expectRefused('A', "slot A stays refused once B's load landed");
  assert.deepEqual(await page.evaluate(() => window.__opens), [], "no open reached the host around B's load");

  // A failed load keeps the guard too, whatever its error says: A owns the editor and unloads while B's
  // load is in flight (the entry stays, orphaned); B's load then fails, and A's next load is refused.
  await ownFile('failed-load');
  await page.evaluate(async () => {
    const { platform } = await import('/src/platform/index.ts');
    const instrument = await import('/src/ui/state/instrument.ts');
    const loadHost = platform.pluginHost.loadPlugin;
    let fail;
    const held = new Promise((_, reject) => (fail = reject));
    window.__failLoadB = () => fail(new Error('the plugin would not load (probe)'));
    window.__loadBInFlight = false;
    platform.pluginHost.loadPlugin = async (slot, ...rest) => {
      if (slot !== 1) return loadHost(slot, ...rest);
      window.__loadBInFlight = true;
      await held;
    };
    window.__restoreLoad = () => (platform.pluginHost.loadPlugin = loadHost);
    window.__loadB = instrument.selectPlugin(1, window.__desc);
  });
  await page.waitForFunction(() => window.__loadBInFlight);
  await page.evaluate(async () => {
    const instrument = await import('/src/ui/state/instrument.ts');
    await instrument.clearPlugin(0);
    window.__failLoadB();
    await window.__loadB;
    window.__restoreLoad();
    await instrument.selectPlugin(0, window.__desc);
  });
  assert.equal(consumeConsoleErrors('[instrument] plugin load failed').length, 1, "B's failed load reaches the release log");
  await expectRefused('A', "slot A's next load is refused: B's failed load may have left the module loaded");
  assert.deepEqual(await page.evaluate(() => window.__opens), [], "no open reached the host after B's failed load");

  // The guard outlives the slot's next load: B's load of A's file times out, B loads another file, A
  // reloads, and A is refused.
  await ownFile('timed-out');
  await page.evaluate(async () => {
    const { platform } = await import('/src/platform/index.ts');
    const instrument = await import('/src/ui/state/instrument.ts');
    const loadHost = platform.pluginHost.loadPlugin;
    platform.pluginHost.loadPlugin = async (slot, ...rest) => {
      if (slot === 1) throw new Error('plugin setup timed out (probe)');
      return loadHost(slot, ...rest);
    };
    await instrument.selectPlugin(1, window.__desc);
    platform.pluginHost.loadPlugin = loadHost;
    await instrument.selectPlugin(1, { id: 'other-probe', name: 'Other Probe', path: 'C:\\probe\\other.vst3', format: 'vst3', isEffect: true });
  });
  assert.equal(consumeConsoleErrors('[instrument] plugin load failed').length, 1, "B's timed-out load reaches the release log");
  assert.equal(await reloadA(), 'reloaded', 'slot A reloads after B moved on from its timed-out load');
  await expectRefused('A', "slot A's reloaded editor is refused while B's timed-out setup may hold the file");
  assert.deepEqual(await page.evaluate(() => window.__opens), [], "no open reached the host after B's timed-out load");

  // A late failure must not drop a newer reservation. Slot A opens another plugin's editor (held), the
  // plugin unloads (its last holder: the affinity goes), loads again and opens its editor at once; then
  // the first open fails. Slot B, loading that plugin, must still be refused: A's editor is open.
  await page.evaluate(async () => {
    const { platform } = await import('/src/platform/index.ts');
    const instrument = await import('/src/ui/state/instrument.ts');
    window.__late = { id: 'late-probe', name: 'Late Probe', path: 'C:\\probe\\late.vst3', format: 'vst3', isEffect: true };
    await instrument.clearPlugin(1);
    await instrument.selectPlugin(0, window.__late);
    window.__opens = [];
    let fail;
    window.__failFirst = () => fail(new Error('the plugin exposes no editor (late)'));
    const first = new Promise((_, reject) => (fail = reject));
    platform.pluginHost.openEditor = async (slot) => {
      window.__opens.push(slot);
      if (window.__opens.length === 1) await first;
    };
  });
  await editor('A').click();
  await page.waitForFunction(() => window.__opens.length === 1);
  await page.evaluate(async () => {
    const instrument = await import('/src/ui/state/instrument.ts');
    await instrument.clearPlugin(0);
    await instrument.selectPlugin(0, window.__late);
  });
  await editor('A', false).click();
  await editor('A', true).waitFor();
  await page.evaluate(() => window.__failFirst());
  await page.waitForTimeout(100);
  assert.equal(consumeConsoleErrors('[PluginControls] editor toggle failed').length, 1, 'the late failure reaches the release log');
  await page.evaluate(async () => (await import('/src/ui/state/instrument.ts')).selectPlugin(1, window.__late));
  await editor('B').waitFor();
  // The race's refusal toast first goes, so the one this press raises is seen afresh.
  await dismissToasts();
  await editor('B').click();
  await page.locator('.toast', { hasText: 'Could not open a second editor for this plugin.' }).waitFor({ timeout: 5000 });
  assert.equal(await editor('B').getAttribute('aria-pressed'), 'false');
  assert.deepEqual(await page.evaluate(() => window.__opens), [0, 0], "slot B's open never reached the host: A's newer reservation stood");

  // A WebView reload empties the editor affinity while the host keeps its plugins. A owns a file's editor
  // (opened and closed), the page reloads, and the startup resync's unload of A's stranded plugin fails: the
  // host may have leaked the module, still bound to A's old owner thread, and vacated the slot anyway. The
  // file loaded again, in B or in A, has its EDITOR refused until restart, and no open reaches the host.
  await ownFile('stranded');
  const stranded = await page.evaluate(() => window.__desc);
  await page.addInitScript((descriptor) => (window.__stranded = [{ slot: 0, descriptor }]), stranded);
  await page.reload();
  await page.waitForFunction(() => '__lf' in window);
  await page.waitForFunction(async () => {
    const instrument = await import('/src/ui/state/instrument.ts');
    return instrument.nativeHostReady() && !instrument.scanning();
  });
  assert.equal(consumeConsoleErrors('[instrument] resync: unloading stranded plugin in slot 0').length, 1, 'the resync finds A stranded');
  assert.equal(consumeConsoleErrors('[instrument] resync: stranded-slot unload failed').length, 1, "the resync's failed unload reaches the release log");
  assert.equal(consumeConsoleErrors(/^\[rig-recall\] slot 1: .*stranded/).length, 1, "A's remembered file is not in the fake's scan");
  await page.evaluate(async (descriptor) => {
    const { platform } = await import('/src/platform/index.ts');
    const instrument = await import('/src/ui/state/instrument.ts');
    window.__desc = descriptor;
    window.__opens = [];
    platform.pluginHost.loadPlugin = async (slot) => ({ slot, descriptor });
    platform.pluginHost.listParams = async () => [];
    platform.pluginHost.openEditor = async (slot) => void window.__opens.push(slot);
    await instrument.selectPlugin(1, descriptor);
  }, stranded);
  const heldUntilRestart = "Could not open this plugin's editor until BleepLoop restarts.";
  await expectRefused('B', "slot B's editor of the stranded file is refused: the module may be bound to A's old thread", heldUntilRestart);
  await page.evaluate(async () => (await import('/src/ui/state/instrument.ts')).selectPlugin(0, window.__desc));
  await expectRefused('A', "slot A's new load of the stranded file is refused too: its owner thread is a new one", heldUntilRestart);
  assert.deepEqual(await page.evaluate(() => window.__opens), [], 'no open of the stranded file reached the host');
});
