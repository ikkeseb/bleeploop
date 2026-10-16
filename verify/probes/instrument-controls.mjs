/**
 * Rendered regression coverage for keyboard state, plugin controls and the MIDI device list, on the web
 * engine fake (`src/platform/host.web.ts`, the engine-seam pattern) with the native slot chrome on
 * (host.web.ts served with `available: true`): native MIDI's port list (scripted with
 * `__lf.native.midiEmit`) reads in the Audio Settings diagnostics and the command bar's status lamp, a port
 * another program holds says so, and LEARN is enabled only while a port is open; both empty native slots
 * show the install hint, the keyboard octave survives a placement remount, the slot volume (a
 * plugin's output gain) honors the unity detent under keyboard steps and slider input, and with one
 * plugin file in both slots, a second EDITOR pressed while the first open is in flight is refused (one
 * open reaches the host), and an open that fails late, after its slot unloaded the plugin and reserved its
 * editor again, leaves that newer reservation standing. The plugin host and
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
        await route.fulfill({ response, body: body.replace('available: false', 'available: true') });
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
  await paramsButton.click();
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
  await page.evaluate(async () => {
    const notify = await import('/src/notify.ts');
    for (const t of notify.toasts()) notify.dismissToast(t.id);
  });
  await editor('B').click();
  await page.locator('.toast', { hasText: 'Could not open a second editor for this plugin.' }).waitFor({ timeout: 5000 });
  assert.equal(await editor('B').getAttribute('aria-pressed'), 'false');
  assert.deepEqual(await page.evaluate(() => window.__opens), [0, 0], "slot B's open never reached the host: A's newer reservation stood");
});
