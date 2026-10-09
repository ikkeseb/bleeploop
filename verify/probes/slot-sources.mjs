/**
 * A slot is its own input, on the web engine fake (`src/platform/host.web.ts`, the engine-seam pattern)
 * with a stand-in plugin host (one amp-sim effect, a four-input device). Drives the real slot header and
 * asserts the commands the fake engine received:
 *
 * - boot: a saved global input channel (from before the per-slot pick) seeds both slots: the device
 *   opens with it for each, and each slot's input pick shows it; the first reset frame sends every
 *   synth's level;
 * - there is no MIC button and no input channel select in Audio Settings (the input device select
 *   stays);
 * - the source picker lists Off, then the Built-in synths, then the Plugins; Off on the active slot sends
 *   the note target `Off` to native MIDI's router (`input.selectTarget`, `__lf.native.inputSent`) and a
 *   later note goes to that target (the engine plays nothing there);
 * - both slots Off and live at once on different inputs: each input pick switches its slot's channel, and
 *   going live on one never takes the other off;
 * - the slot volume sends what its source needs: an Off slot's input level (`SetSlotGain`), a synth's
 *   level (`SetInstrumentGain`), a plugin's output (`SetSlotGain`); the Off source, the input picks and
 *   the Off and synth levels survive a reload and are sent to the engine again;
 * - a slot's input names only a channel the slot reads: an open whose status reads auto for a pick the
 *   device lacks, a status from a device the owner reopened on, an input device change and a prune (a
 *   vanished device, a pick past the device's inputs) reset that pick to Auto, in storage and on screen;
 * - GO LIVE pressed (the named action, as a pedal) while a tone reload holds a live slot: one press stops
 *   it (the reload does not resume it), two leave it live;
 * - a source pick whose plugin unload fails is not saved for the next launch; one that unloads is;
 * - a plugin's level is kept per slot and plugin: loaded again, and loaded by the rig recall's load after
 *   a reload, it comes back at that level (not its type default);
 * - the header at 1280×800 and 1000×700 (slot A Off and live on In 2, slot B the amp-sim): no control
 *   leaves its card, the source's own controls stay on one line, and at 1280 the header has at most two.
 *   `--shots=<dir>` saves those two screenshots there (else `logs/slot-sources/`).
 *
 * Cannot see the native engine, the Rust device side (a channel the device lacks) or anything audible:
 * the fake answers nothing by itself. Run: pnpm probe slot-sources [--shots=<dir>]
 */
import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { arg, probe } from '../harness/probe.ts';

const shotsDir = arg('shots') ?? 'logs/slot-sources';
const RATE = 48000;
const AMP = { id: 'probe.amp', name: 'Probe Amp Sim', format: 'vst3', isEffect: true, path: 'C:\\probe\\amp.vst3' };
const empty = { state: 'Empty', length: 0, armed: false, autoArmed: false, canUndo: false, canReverse: false, reversed: false, stopAt: null, fading: false, retakePass: 0 };

/** The engine fake on, a plugin host that is there (its replies stubbed in `stubHost`). */
async function engineInit(page, saved) {
  await page.route('**/src/platform/host.web.ts', async (route) => {
    const response = await route.fetch();
    await route.fulfill({ response, body: (await response.text()).replace('available: false', 'available: true') });
  });
  await page.addInitScript((devices) => {
    window.__lfEngineFake = true;
    if (devices && !sessionStorage.getItem('slot-sources.seeded')) {
      sessionStorage.setItem('slot-sources.seeded', '1');
      localStorage.setItem('lf.audioDevices', devices);
    }
  }, saved ?? null);
}

/** Wait for the engine boot, answer with a reset frame, and give the host an amp-sim and four inputs. */
async function boot(page) {
  await page.waitForFunction(() => window.__lf.native.opened.length >= 1, undefined, { timeout: 5000 });
  await page.evaluate(async ({ AMP, RATE, empty }) => {
    const { platform } = await import('/src/platform/index.ts');
    const instrument = await import('/src/ui/state/instrument.ts');
    const devices = await import('/src/ui/state/audio-devices.ts');
    const deadline = Date.now() + 10000;
    while (!instrument.nativeHostReady() || instrument.scanning()) {
      if (Date.now() > deadline) throw new Error('native host boot did not finish');
      await new Promise((r) => setTimeout(r, 20));
    }
    const host = platform.pluginHost;
    host.scanPlugins = async () => [AMP];
    host.loadPlugin = async (slot) => ({ slot, descriptor: AMP });
    host.unloadPlugin = async () => {};
    host.openEditor = async () => {};
    host.listParams = async () => [];
    host.listInputDevices = async () => [{ id: 'fake-in', name: 'Fake input', channels: 4 }];
    await devices.refreshInputDevices();
    await instrument.scanForPlugins();
    window.__lf.native.emit({
      seq: 1,
      reset: true,
      settings: [],
      events: [...[0, 1, 2, 3, 4].map((lane) => ({ Lane: { frame: 0, lane, info: empty } })),
        { Transport: { frame: 0, master: 0, bpm: 120, locked: false } }, { Selected: { frame: 0, lane: 0 } }],
      anchor: { frame: 0, atMs: Date.now(), rate: RATE, grid: 0 },
      meter: { peak: 0, clip: false },
    });
  }, { AMP, RATE, empty });
}

await probe(async ({ browser, open }) => {
  // ── Engine mode, a saved global channel (input 2) from before the per-slot pick ─────────────────────
  const { page, consoleErrors } = await open({
    viewport: { width: 1280, height: 800 },
    init: (p) => engineInit(p, JSON.stringify({ inputChannel: '1' })),
  });
  await boot(page);
  const sent = () => page.evaluate(() => window.__lf.native.sent.map((c) => JSON.stringify(c)));
  const clearSent = () => page.evaluate(() => void (window.__lf.native.sent.length = window.__lf.native.inputSent.length = 0));
  /** What the UI sent native MIDI's router (note targets, notes), each as JSON. */
  const inputSent = () => page.evaluate(() => window.__lf.native.inputSent.map((c) => JSON.stringify(c)));
  const settle = () => page.waitForTimeout(60);
  const picker = (slot) => page.getByRole('combobox', { name: `Source for slot ${slot}`, exact: true });
  const input = (slot) => page.getByRole('combobox', { name: `Input for slot ${slot}`, exact: true });
  const volume = (slot) => page.getByRole('slider', { name: `Volume for slot ${slot}`, exact: true });
  const live = (slot) => page.getByRole('button', { name: `Live input for slot ${slot}`, exact: true });

  const opened = await page.evaluate(() => window.__lf.native.opened[0]);
  console.log('opened', JSON.stringify(opened));
  assert.deepEqual(opened.inputChannels, [1, 1], 'the saved global channel opens both slots on input 2');
  const firstReset = await sent();
  for (const id of ['lead', 'pad', 'piano', 'organ', 'bass', 'drum']) {
    assert.ok(firstReset.includes(JSON.stringify({ SetInstrumentGain: [id, 1] })), `the reset sends ${id}'s level`);
  }

  // No MIC in engine mode; Audio Settings keeps the input device and drops the channel select.
  assert.equal(await page.getByRole('button', { name: 'Mic / line input' }).count(), 0, 'no MIC button in engine mode');
  await page.evaluate(() => window.__lf.ui.openSettings());
  await page.getByRole('combobox', { name: 'Audio input device', exact: true }).waitFor();
  assert.equal(await page.locator('[aria-label="Input channel"]').count(), 0, 'no input channel select in engine mode');
  await page.evaluate(() => window.__lf.ui.closeSettings());

  // The picker: Off, then the built-in synths, then the plugins.
  const listing = await picker(1).evaluate((select) => [...select.children].filter((c) => !c.hidden).map((c) =>
    c.tagName === 'OPTGROUP' ? { group: c.label, options: [...c.children].map((o) => o.textContent) } : c.textContent));
  console.log('picker', JSON.stringify(listing));
  assert.deepEqual(listing, [
    'Off',
    { group: 'Built-in', options: ['Lead', 'Bass', 'Pad', 'Piano', 'Organ', 'Drum'] },
    { group: 'Plugins', options: ['Probe Amp Sim (vst3)'] },
  ]);
  assert.equal(await input(1).count(), 0, 'a synth takes no input: no input pick');

  // Off on the active slot: its notes go to the Off target.
  await clearSent();
  await picker(1).selectOption('off');
  await settle();
  const offSent = await inputSent();
  assert.ok(offSent.includes(JSON.stringify({ selectTarget: { slot: 0, target: 'Off' } })), `Off routes the notes to "Off": ${offSent}`);
  assert.equal(await input(1).inputValue(), '1', "the migrated channel is the Off slot's input (In 2)");
  await page.evaluate(() => document.activeElement?.blur());
  await page.keyboard.down('a');
  await page.keyboard.up('a');
  await settle();
  const noteSent = await inputSent();
  const onAt = noteSent.findIndex((c) => c.startsWith('{"note"'));
  const lastTarget = noteSent.slice(0, onAt).filter((c) => c.startsWith('{"selectTarget"')).at(-1);
  assert.ok(onAt > 0 && lastTarget === JSON.stringify({ selectTarget: { slot: 0, target: 'Off' } }), `a note on an Off slot reaches only the Off target: ${noteSent}`);

  // Both slots Off and live, each on its own input.
  await input(1).selectOption('0');
  await picker(2).selectOption('off');
  await settle();
  await input(2).selectOption('1');
  const picks = await page.evaluate(() => window.__lf.native.slotInputChannels.map((p) => JSON.stringify(p)));
  assert.deepEqual(picks, ['[0,0]', '[1,1]'], 'each input pick switches its own slot');
  await clearSent();
  await live(1).click();
  await page.getByRole('button', { name: 'Live input for slot 1', exact: true, pressed: true }).waitFor();
  await live(2).click();
  await page.getByRole('button', { name: 'Live input for slot 2', exact: true, pressed: true }).waitFor();
  const liveSent = await sent();
  assert.deepEqual(liveSent.filter((c) => c.startsWith('{"SetSlotLive"')), ['{"SetSlotLive":[0,true]}', '{"SetSlotLive":[1,true]}'],
    'going live on slot B leaves slot A live');
  assert.deepEqual(await page.evaluate(async () => (await import('/src/ui/state/native-io.ts')).inputArmed()), [true, true]);

  // The volume per source kind.
  await clearSent();
  await volume(1).fill('0.5');
  await settle();
  assert.ok((await sent()).includes('{"SetSlotGain":[0,0.5]}'), "an Off slot's volume is its input level");
  await picker(2).selectOption('pad');
  await settle();
  await clearSent();
  await volume(2).fill('0.7');
  await settle();
  assert.deepEqual(await sent(), ['{"SetInstrumentGain":["pad",0.7]}'], "a synth slot's volume is the synth's level");
  await picker(2).selectOption({ label: 'Probe Amp Sim (vst3)' });
  await page.getByRole('button', { name: 'Live input for slot 2', exact: true, pressed: true }).waitFor(); // the effect auto-starts
  await clearSent();
  await volume(2).fill('0.8');
  await settle();
  assert.deepEqual(await sent(), ['{"SetSlotGain":[1,0.8]}'], "a plugin slot's volume is the plugin's output");
  await picker(2).selectOption('pad'); // unloads the amp, so the next launch has no plugin to recall
  await settle();

  // A reload: the Off source, the input picks and the levels come back and reach the new engine.
  await page.reload();
  await page.waitForFunction(() => '__lf' in window);
  await boot(page);
  await settle();
  const reopened = await page.evaluate(() => window.__lf.native.opened.at(-1));
  assert.deepEqual(reopened.inputChannels, [0, 1], 'the input picks reopen with the device');
  assert.equal(await picker(1).inputValue(), 'off', 'slot A is still Off');
  assert.equal(await picker(2).inputValue(), 'pad', 'slot B still plays the pad');
  assert.equal(await input(1).inputValue(), '0');
  assert.equal(Number(await volume(1).inputValue()), 0.5, "slot A's input level is kept");
  assert.equal(Number(await volume(2).inputValue()), 0.7, "the pad's level is kept");
  const resent = [...(await sent()), ...(await inputSent())];
  for (const c of [{ SetSlotGain: [0, 0.5] }, { SetInstrumentGain: ['pad', 0.7] }, { selectTarget: { slot: 0, target: 'Off' } }]) {
    assert.ok(resent.includes(JSON.stringify(c)), `the reset sends ${JSON.stringify(c)}: ${resent}`);
  }
  assert.deepEqual(consoleErrors, [], 'no console errors');

  // ── Review fixes, in a fresh profile ───────────────────────────────────────────────────────────────
  {
    const context = await browser.newContext();
    const { page: p, consoleErrors: errors } = await open({ context, viewport: { width: 1280, height: 800 }, init: (pg) => engineInit(pg) });
    await boot(p);
    const pk = (slot) => p.getByRole('combobox', { name: `Source for slot ${slot}`, exact: true });
    const inp = (slot) => p.getByRole('combobox', { name: `Input for slot ${slot}`, exact: true });
    const saved = () => p.evaluate(() => JSON.parse(localStorage.getItem('lf.audioDevices') ?? '{}').slotInputChannels ?? null);
    const sentNow = () => p.evaluate(() => window.__lf.native.sent.map((c) => JSON.stringify(c)));
    const clear = () => p.evaluate(() => void (window.__lf.native.sent.length = 0));
    await pk(1).selectOption('off');
    await pk(2).selectOption('off');
    await inp(1).selectOption('3');
    await inp(2).selectOption('1');
    await p.waitForFunction(() => window.__lf.native.slotInputChannels.length === 2);
    assert.deepEqual(await saved(), ['3', '1']);

    // 1a. A reopen whose status says slot A reads input 2: the device lacks input 4.
    await p.evaluate(async () => {
      const native = window.__lf.native;
      const real = native.open;
      native.open = async (request, force) => ({ ...(await real.call(native, request, force)), inputChannels: [1, 1] });
      await (await import('/src/ui/state/engine-store.ts')).openEngineDevice();
      native.open = real;
    });
    assert.deepEqual(await saved(), ['', '1'], 'a pick the device lacks is saved as Auto; the other stays');
    assert.equal(await inp(1).inputValue(), '', 'and the slot shows Auto');
    const autoText = (slot, text) => p.waitForFunction(([s, t]) =>
      document.querySelector(`[aria-label="Input for slot ${s}"] option[value=""]`)?.textContent === t, [slot, text], { timeout: 2000 })
      .then(() => true, () => false);
    assert.ok(await autoText(1, 'Auto · In 2'), "Auto names the input the engine reads for the slot (its status)");
    assert.ok(await autoText(2, 'Auto'), "a slot with a pick of its own keeps Auto plain: the status reads the pick's channel");
    // 1b. The owner reopened on another device (a fallback) whose status reads input 1 for slot B.
    await p.evaluate(() => window.__lf.native.emit({ seq: 2, reset: false, events: [], status: {
      backend: 'Wasapi', sampleRate: 48000, block: 256, inputName: 'Small input', outputName: 'Fake output',
      alignFrames: 0, inputFrames: 0, inputOpen: true, inputChannels: [0, 0] } }));
    assert.deepEqual(await saved(), ['', ''], "a device the owner reopened on without slot B's pick resets it");
    assert.equal(await inp(2).inputValue(), '');
    assert.ok(await autoText(2, 'Auto · In 1'), 'on a device whose status reads input 1, Auto says so');
    // 1c. An input device change in Audio Settings resets both picks (back on the four-input device first).
    await p.evaluate(() => window.__lf.native.emit({ seq: 3, reset: false, events: [], status: {
      backend: 'Wasapi', sampleRate: 48000, block: 256, inputName: 'Fake input', outputName: 'Fake output',
      alignFrames: 0, inputFrames: 0, inputOpen: true, inputChannels: [1, 1] } }));
    await inp(1).selectOption('2');
    await p.evaluate(async () => {
      const { platform } = await import('/src/platform/index.ts');
      platform.pluginHost.listInputDevices = async () => [{ id: 'fake-in', name: 'Fake input', channels: 4 }, { id: 'other', name: 'Other input', channels: 2 }];
      window.__lf.ui.openSettings();
    });
    await p.waitForFunction(() => document.querySelectorAll('[aria-label="Audio input device"] option').length === 3);
    await p.getByRole('combobox', { name: 'Audio input device', exact: true }).selectOption('other');
    await p.waitForFunction(() => window.__lf.native.opened.length >= 3);
    assert.deepEqual(await saved(), ['', ''], 'an input device change resets both picks');
    assert.deepEqual((await p.evaluate(() => window.__lf.native.opened.at(-1))).inputChannels, [null, null], 'and the reopen asks for auto');
    await p.evaluate(() => window.__lf.ui.closeSettings());
    // 1d. The prune (startup, Audio Settings): a pick past the saved device's inputs, then a vanished device.
    const pruned = await p.evaluate(async () => {
      const devices = await import('/src/ui/state/audio-devices.ts');
      const { writeAudioDeviceSettings, readAudioDeviceSettings } = await import('/src/ui/state/audio-settings.ts');
      writeAudioDeviceSettings({ inputDeviceId: 'other' });
      devices.saveSlotInputChannels(['3', '1']);
      await devices.refreshAndPruneDevices();
      const past = readAudioDeviceSettings().slotInputChannels;
      devices.saveSlotInputChannels(['1', '0']);
      const { platform } = await import('/src/platform/index.ts');
      platform.pluginHost.listInputDevices = async () => [{ id: 'fake-in', name: 'Fake input', channels: 4 }];
      await devices.refreshAndPruneDevices();
      return { past, vanished: readAudioDeviceSettings().slotInputChannels, shown: devices.slotInputChannels() };
    });
    assert.deepEqual(pruned, { past: ['', '1'], vanished: ['', ''], shown: ['', ''] }, 'the prune drops a pick past the inputs, and both with a vanished device');

    // 2. GO LIVE pressed (the named action) while a tone reload holds live slot A. Under the amp, A plays
    // a synth, and B is Off (takes input): the press must still reach A, whose plugin the reload removed.
    await pk(1).selectOption('lead');
    await pk(1).selectOption({ label: 'Probe Amp Sim (vst3)' });
    await p.getByRole('button', { name: 'Live input for slot 1', exact: true, pressed: true }).waitFor();
    const duringReload = (presses) => p.evaluate(async (presses) => {
      const { platform } = await import('/src/platform/index.ts');
      const instrument = await import('/src/ui/state/instrument.ts');
      const io = await import('/src/ui/state/native-io.ts');
      const { runAction } = await import('/src/app/actions.ts');
      const host = platform.pluginHost;
      const load = host.loadPlugin;
      let release;
      host.loadPlugin = (...args) => new Promise((resolve) => { release = () => resolve(load(...args)); });
      window.__lf.native.sent.length = 0;
      const reload = instrument.reloadPlugin(0, instrument.slotPlugins()[0], instrument.slotSourceGeneration(0), 7);
      while (!release) await new Promise((r) => setTimeout(r, 5));
      for (let i = 0; i < presses; i++) runAction('goLive');
      release();
      const result = await reload;
      host.loadPlugin = load;
      await new Promise((r) => setTimeout(r, 50));
      return { result, live: io.inputArmed()[0], sent: window.__lf.native.sent.filter((c) => c.SetSlotLive).map((c) => JSON.stringify(c)) };
    }, presses);
    const stopped = await duringReload(1);
    console.log('one press during the reload', JSON.stringify(stopped));
    assert.deepEqual(stopped, { result: 'reloaded', live: false, sent: ['{"SetSlotLive":[0,false]}'] },
      'a stop pressed during the reload wins: the reload does not resume GO LIVE');
    await p.getByRole('button', { name: 'Live input for slot 1', exact: true, pressed: false }).click();
    await p.getByRole('button', { name: 'Live input for slot 1', exact: true, pressed: true }).waitFor();
    const twice = await duringReload(2);
    console.log('two presses during the reload', JSON.stringify(twice));
    assert.equal(twice.live, true, 'stop then go during the reload leaves the slot live');
    assert.deepEqual(twice.sent, ['{"SetSlotLive":[0,false]}', '{"SetSlotLive":[0,true]}']);

    // 5. The plugin's level is kept per slot and plugin.
    await clear();
    await p.getByRole('slider', { name: 'Volume for slot 1', exact: true }).fill('0.8');
    await p.waitForTimeout(60);
    const levelKey = `lf.pluginGain.0.${JSON.stringify(['vst3', AMP.path, AMP.id])}`;
    assert.equal(await p.evaluate((k) => localStorage.getItem(k), levelKey), '0.8', 'the plugin level is saved per slot and plugin');
    await pk(1).selectOption('lead');
    await p.waitForFunction(() => !window.__lf.slotPlugins()[0]);
    await clear();
    await pk(1).selectOption({ label: 'Probe Amp Sim (vst3)' });
    await p.getByRole('button', { name: 'Live input for slot 1', exact: true, pressed: true }).waitFor();
    assert.ok((await sentNow()).includes('{"SetSlotGain":[0,0.8]}'), `loaded again, it comes back at its level: ${await sentNow()}`);

    // 4. A pick whose unload fails is not saved; one that unloads is.
    const failedPick = await p.evaluate(async () => {
      const { platform } = await import('/src/platform/index.ts');
      const instrument = await import('/src/ui/state/instrument.ts');
      const slots = await import('/src/ui/state/instrument-slots.ts');
      const idle = async () => { while (slots.slotPendingCounts().some((n) => n > 0)) await new Promise((r) => setTimeout(r, 5)); };
      const host = platform.pluginHost;
      const unload = host.unloadPlugin;
      const before = localStorage.getItem('lf.slotSource.0');
      host.unloadPlugin = async () => { throw new Error('injected unload failure'); };
      instrument.selectOff(0);
      await idle();
      instrument.selectSynth(0, 'pad');
      await idle();
      const failed = { before, after: localStorage.getItem('lf.slotSource.0'), plugin: instrument.slotPlugins()[0]?.id ?? null };
      host.unloadPlugin = unload;
      instrument.selectOff(0);
      await idle();
      return { ...failed, unloaded: localStorage.getItem('lf.slotSource.0'), pluginAfter: instrument.slotPlugins()[0]?.id ?? null };
    });
    console.log('pick over a failed unload', JSON.stringify(failedPick));
    assert.deepEqual(failedPick, { before: 'lead', after: 'lead', plugin: AMP.id, unloaded: 'off', pluginAfter: null },
      'Off and a synth are saved only once the plugin unloaded');

    // 5, at the next launch: the rig recall's load brings the plugin back at its level.
    await p.reload();
    await p.waitForFunction(() => '__lf' in window);
    await boot(p);
    await clear();
    await p.evaluate(async (AMP) => (await import('/src/ui/state/instrument.ts')).restorePlugin(0, AMP), AMP);
    assert.ok((await sentNow()).includes('{"SetSlotGain":[0,0.8]}'), `the recalled plugin comes back at its level: ${await sentNow()}`);
    const unexpected = errors.filter((e) => !e.startsWith('[instrument] plugin unload failed'));
    assert.deepEqual(unexpected, [], 'no console errors beyond the injected unload failures');
    await context.close();
  }

  // ── The header at two window sizes: A Off and live on In 2, B the amp-sim ──────────────────────────
  await mkdir(shotsDir, { recursive: true });
  for (const [width, height] of [[1280, 800], [1000, 700]]) {
    const context = await browser.newContext();
    const { page: p } = await open({ context, viewport: { width, height }, init: (pg) => engineInit(pg) });
    await boot(p);
    const pk = (slot) => p.getByRole('combobox', { name: `Source for slot ${slot}`, exact: true });
    await pk(1).selectOption('off');
    await p.getByRole('combobox', { name: 'Input for slot 1', exact: true }).selectOption('1');
    await p.getByRole('button', { name: 'Live input for slot 1', exact: true, pressed: false }).click();
    await pk(2).selectOption({ label: 'Probe Amp Sim (vst3)' });
    await p.getByRole('button', { name: 'Live input for slot 2', exact: true, pressed: true }).waitFor();
    await p.getByRole('button', { name: 'Live input for slot 1', exact: true, pressed: true }).waitFor();
    await p.evaluate(() => document.activeElement?.blur());
    await p.mouse.move(width - 2, height - 2); // no hover on the caps
    await p.waitForTimeout(200);
    const layout = await p.evaluate(() => [...document.querySelectorAll('.slot')].map((card) => {
      const box = card.getBoundingClientRect();
      const row = card.querySelector('.slot__row');
      const controls = [...row.querySelectorAll('button, select, input')].map((el) => el.getBoundingClientRect());
      // A line: controls whose vertical centres lie within a few pixels of each other.
      const lineCount = (rects) => {
        const centres = rects.map((r) => r.top + r.height / 2).sort((a, b) => a - b);
        return centres.filter((c, i) => i === 0 || c - centres[i - 1] > 6).length;
      };
      const acts = [...row.querySelectorAll('.slot__acts > *')].map((el) => el.getBoundingClientRect());
      return {
        outside: controls.filter((r) => r.left < box.left - 0.5 || r.right > box.right + 0.5).length,
        actsLines: lineCount(acts),
        lines: lineCount(controls),
        height: Math.round(row.getBoundingClientRect().height),
      };
    }));
    console.log(`${width}x${height} header`, JSON.stringify(layout));
    assert.equal(layout.length, 2, `${width}: both slot cards measured`);
    for (const [i, slot] of layout.entries()) {
      assert.equal(slot.outside, 0, `${width}: slot ${i + 1}'s controls stay inside its card`);
      assert.equal(slot.actsLines, 1, `${width}: slot ${i + 1}'s own controls stay on one line`);
      if (width === 1280) assert.ok(slot.lines <= 2, `1280: slot ${i + 1}'s header keeps to two lines (${slot.lines})`);
    }
    const file = `${shotsDir}/slot-header-${width}x${height}.png`;
    await p.locator('.src').screenshot({ path: file });
    await p.screenshot({ path: `${shotsDir}/slot-page-${width}x${height}.png` });
    console.log('screenshot', file);
    await context.close();
  }
});
