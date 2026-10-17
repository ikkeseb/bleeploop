/**
 * The ASIO driver picker, the driver-range Buffer select and the no-device honesty, in engine mode on the
 * web engine fake (`window.__lfEngineFake`) with the native chrome on (host.web.ts served with
 * `available: true`) and a scripted ASIO host in the page:
 *
 * - a first open that fails: the slot note says no audio device is open (not "No plugins found"), the
 *   disabled rescan button's title says why, Diagnostics' engine row carries the reason, and the open's
 *   toast stays; a driver that starts meanwhile is not called low-latency, nor the buffer given a block
 *   length, until a device runs on it;
 * - the Driver row lists Automatic (naming the driver it took) and the installed drivers; picking one
 *   switches the driver (the scripted host closes the device on ASIO and reopens it, as the native
 *   device owner does), then the picks open; the pick is saved and the saved buffer left alone;
 * - a driver fixed at 512 frames: the Buffer select offers 512 alone, shows it and says where to change
 *   it; a wide range offers the sizes inside it;
 * - a switch the host refuses: a toast, the device reopens and the pick stays the driver that runs;
 * - a switch to a driver at another rate while the engine holds loops, confirmed, whose recovery save
 *   fails: the driver that ran comes back before the device reopens, so it runs there, not silent;
 * - a saved driver no longer installed (the host takes the automatic choice): one toast (a note) names it and the
 *   driver that runs, the pick stays saved and the Driver row marks it not installed; an installed pick,
 *   Automatic, or a probe that found no driver says nothing of the kind.
 *
 * The native half (the owner-run switch, the range clamp, a stepped driver's fallback) is `cargo test`
 * (`asio_startup.rs`, `engine_io/transition.rs`, `engine_io/tests.rs`); this probe sees neither a driver
 * nor cpal.
 * Run: pnpm probe asio-driver
 */
import { probe } from '../harness/probe.ts';
import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';

// name: [buffer min, buffer max, rate]
const DRIVERS = {
  'Focusrite USB ASIO': [16, 2048, 48000],
  'Yamaha Steinberg USB ASIO': [512, 512, 48000],
  'ASIO4ALL v2': [64, 2048, 48000],
  'RME at 44.1 kHz': [64, 2048, 44100],
};

await probe(async ({ open }) => {
  const { page, consumeConsoleErrors } = await open({
    viewport: { width: 1280, height: 820 },
    init: async (p) => {
      await p.addInitScript(() => {
        window.__lfEngineFake = true;
        window.__lfFakeOpenError = 'fake: Buffer size 64 is not in the supported range 512..=512';
        localStorage.setItem('lf.audioDevices', JSON.stringify({ bufferFrames: 64, asioEnabled: true }));
      });
      await p.route('**/src/platform/host.web.ts*', async (route) => {
        const response = await route.fetch();
        let body = await response.text();
        const push = 'webEngineFake.opened.push(request);';
        if (!body.includes('available: false') || !body.includes(push)) throw new Error('Could not script host.web.ts');
        body = body
          .replace('available: false', 'available: true')
          .replace(push, `${push} if (globalThis.__lfFakeOpenError) throw new Error(globalThis.__lfFakeOpenError);`);
        await route.fulfill({ response, body });
      });
    },
  });

  // 1. The first open failed: nothing claims "no plugins".
  await page.waitForFunction(() => window.__lf.native.opened.length > 0);
  await page.waitForFunction(() => document.querySelector('.slot__plugin-note')?.textContent?.includes('No audio device open'));
  const failed = await page.evaluate(async () => {
    const rescan = document.querySelector('[aria-label="Rescan plugins"]');
    window.__lf.ui.openSettings();
    await new Promise((resolve) => setTimeout(resolve, 60));
    const engineRow = [...document.querySelectorAll('.audio-settings__diag-row')].find((r) => r.querySelector('span')?.textContent === 'engine');
    const out = {
      notes: [...document.querySelectorAll('.slot__plugin-note')].map((n) => n.textContent),
      rescan: { disabled: rescan.disabled, title: rescan.title },
      engine: engineRow?.querySelector('b')?.textContent ?? null,
      toasts: window.__lf.notify.toasts().map((t) => `${t.message}: ${t.detail ?? ''}`),
    };
    window.__lf.ui.closeSettings();
    return out;
  });
  console.log('failed open', JSON.stringify(failed));
  assert.deepEqual(failed.notes, ['No audio device open. See Audio Settings.', 'No audio device open. See Audio Settings.']);
  assert.equal(failed.rescan.disabled, true);
  assert.match(failed.rescan.title, /no audio device is open/);
  assert.equal(failed.engine, 'no device open: fake: Buffer size 64 is not in the supported range 512..=512');
  assert.ok(failed.toasts.some((t) => t.startsWith('Could not open the audio device.') && t.includes(': fake: Buffer size 64')), 'the open keeps its toast');
  assert.equal(consumeConsoleErrors('[engine] device open failed Error: fake: Buffer size 64').length, 1, 'and its release-log line');

  // 2. An ASIO host appears (scripted): the probe, then the saved device opens on it.
  const running = await page.evaluate(async (drivers) => {
    const { platform } = await import('/src/platform/index.ts');
    const settings = await import('/src/ui/state/audio-devices.ts');
    const store = await import('/src/ui/state/engine-store.ts');
    const { asioBlock } = await import('/src/ui/state/audio-settings.ts');
    const host = platform.pluginHost;
    const engine = platform.engine;
    const asio = { cached: null, refuseNext: false, failNext: false };
    const calls = (window.__asioCalls = []);
    const report = () => ({ status: asio.cached ? 'ready' : 'failed', detail: asio.cached ? '' : 'no usable ASIO driver found' });
    const pick = (driver) => (driver && drivers[driver] ? driver : Object.keys(drivers)[0]);
    host.asioStatus = async () => report();
    host.asioProbe = async (explicit, driver) => {
      calls.push(`probe:${driver}`);
      asio.cached = asio.failNext ? null : pick(driver);
      asio.failNext = false;
      return report();
    };
    host.asioDrivers = async () => Object.keys(drivers);
    host.asioDeviceInfo = async () => {
      if (!asio.cached) return null;
      const [bufferMin, bufferMax] = drivers[asio.cached];
      return { name: asio.cached, inputChannels: 2, outputChannels: 2, bufferMin, bufferMax };
    };
    // As the native command in engine mode, on the device owner: a device on ASIO closes, the driver
    // switches (or the switch is refused), and what ran opens again on the driver cached then, as the
    // player's open would (a refusal at another rate is the owner's to log).
    host.asioSwitch = async (driver) => {
      calls.push(`switch:${driver}`);
      const ran = (await engine.status())?.backend === 'Asio' ? window.__lf.native.opened.at(-1) : null;
      if (ran) await engine.close();
      let refused = null;
      if (asio.refuseNext) { asio.refuseNext = false; refused = new Error('a driver probe is already running'); }
      else asio.cached = pick(driver);
      if (ran) await engine.open(ran).catch(() => null);
      if (refused) throw refused;
      return report();
    };
    // The fake engine opens at the size the driver takes, as the native driver does (`asio_block`), and
    // refuses a driver at another rate unless forced (the engine holds loops at 48 kHz).
    const native = window.__lf.native;
    const open = engine.open.bind(engine);
    engine.open = async (request, force) => {
      calls.push(`open:${request.backend}:${request.buffer}`);
      const rate = request.backend === 'Asio' && asio.cached ? drivers[asio.cached][2] : 48000;
      native.refusal = rate === 48000 ? null : { device: asio.cached, from: 48000, to: rate };
      window.__lfEngineFakeRate = rate;
      const status = await open(request, force);
      if (request.backend !== 'Asio' || !asio.cached || request.buffer == null) return status;
      const [min, max] = drivers[asio.cached];
      return { ...status, block: asioBlock(request.buffer, min, max), inputName: asio.cached, outputName: asio.cached };
    };
    const close = engine.close.bind(engine);
    engine.close = async () => { calls.push('close'); return close(); };
    window.__asioFake = asio;
    delete window.__lfFakeOpenError;
    await settings.probeAsio(true);
    // The driver started and no device runs yet: DEVICE claims neither low latency nor a block length.
    const deviceRow = async () => {
      window.__lf.ui.openSettings();
      await new Promise((resolve) => setTimeout(resolve, 80));
      const out = {
        word: document.querySelector('.audio-settings__toggle-text')?.textContent ?? null,
        block: document.querySelector('[aria-label="Buffer size in frames"]').closest('.audio-settings__row').querySelector('.audio-settings__readout').textContent,
      };
      window.__lf.ui.closeSettings();
      return out;
    };
    const idle = await deviceRow();
    const status = await store.openEngineDevice();
    return { status, calls: calls.splice(0), idle, runs: await deviceRow() };
  }, DRIVERS);
  console.log('running', JSON.stringify(running));
  assert.deepEqual(running.calls, ['probe:', 'open:Asio:64']);
  assert.equal(running.status.block, 64, 'Focusrite takes 64');
  assert.deepEqual(running.idle, { word: 'no device', block: 'no device' }, 'a started driver with no device open claims no low latency and no block');
  assert.deepEqual(running.runs, { word: 'low-latency', block: '~1.3 ms/block' }, 'the device on ASIO does');

  const readSettings = () =>
    page.evaluate(async () => {
      window.__lf.ui.openSettings();
      await new Promise((resolve) => setTimeout(resolve, 80));
      const driver = document.querySelector('[aria-label="ASIO driver"]');
      const buffer = document.querySelector('[aria-label="Buffer size in frames"]');
      const bufferHint = buffer.closest('.audio-settings__row').nextElementSibling;
      const out = {
        driver: driver ? { value: driver.value, options: [...driver.options].map((o) => o.textContent.trim()), disabled: driver.disabled } : null,
        buffer: { value: buffer.value, options: [...buffer.options].map((o) => Number(o.value)) },
        hint: bufferHint?.textContent ?? null,
        input: document.querySelector('[aria-label="Audio input device"]').selectedOptions[0].textContent,
        saved: JSON.parse(localStorage.getItem('lf.audioDevices')),
      };
      window.__lf.ui.closeSettings();
      return out;
    });
  const pickDriver = (name) =>
    page.evaluate(async (value) => {
      window.__lf.ui.openSettings();
      await new Promise((resolve) => setTimeout(resolve, 80));
      const select = document.querySelector('[aria-label="ASIO driver"]');
      select.value = value;
      select.dispatchEvent(new Event('change', { bubbles: true }));
      const { openEngineDevice } = await import('/src/ui/state/engine-store.ts');
      await openEngineDevice(); // queued behind the switch: resolves once it finished
      window.__lf.ui.closeSettings();
      return window.__asioCalls.splice(0);
    }, name);

  // 3. The Driver row, and the Buffer select on a wide range.
  const wide = await readSettings();
  console.log('wide', JSON.stringify(wide));
  assert.deepEqual(wide.driver, {
    value: '',
    options: ['Automatic (Focusrite USB ASIO)', ...Object.keys(DRIVERS)],
    disabled: false,
  });
  assert.deepEqual(wide.buffer, { value: '64', options: [64, 128, 256, 512, 1024] });
  assert.doesNotMatch(wide.hint ?? '', /control panel/);
  await mkdir('logs/layout', { recursive: true });

  // 4. A live switch to a driver fixed at 512 frames: the switch (the device closes and reopens inside it),
  // then the picks open.
  const toYamaha = await pickDriver('Yamaha Steinberg USB ASIO');
  console.log('switch', JSON.stringify(toYamaha));
  assert.deepEqual(toYamaha.slice(0, 4), ['switch:Yamaha Steinberg USB ASIO', 'close', 'open:Asio:64', 'open:Asio:64']);
  const fixed = await readSettings();
  console.log('fixed', JSON.stringify(fixed));
  assert.equal(fixed.driver.value, 'Yamaha Steinberg USB ASIO');
  assert.deepEqual(fixed.buffer, { value: '512', options: [512] }, 'the one size the driver takes, shown');
  assert.equal(fixed.hint, "Set by the driver: change it in the driver's control panel.");
  assert.equal(fixed.input, 'Yamaha Steinberg USB ASIO');
  assert.equal(fixed.saved.asioDriver, 'Yamaha Steinberg USB ASIO', 'the pick is saved');
  assert.equal(fixed.saved.bufferFrames, 64, "the player's buffer pick survives the driver's fallback");
  const engineRow = await page.evaluate(() => window.__lf.native.opened.at(-1));
  assert.equal(engineRow.buffer, 64, 'the request still asks for the saved size');
  await page.evaluate(() => window.__lf.ui.openSettings());
  await page.screenshot({ path: 'logs/layout/asio-driver-fixed.png' });
  await page.evaluate(() => window.__lf.ui.closeSettings());

  // 5. A switch the host refuses: a toast, the device reopens, the pick stays the driver that runs.
  await page.evaluate(() => { window.__asioFake.refuseNext = true; });
  const refused = await pickDriver('ASIO4ALL v2');
  console.log('refused', JSON.stringify(refused));
  assert.deepEqual(refused.slice(0, 4), ['switch:ASIO4ALL v2', 'close', 'open:Asio:64', 'open:Asio:64']);
  const after = await readSettings();
  const toasts = await page.evaluate(() => window.__lf.notify.toasts().map((t) => t.message));
  assert.ok(toasts.includes('Could not switch the ASIO driver. Pick it again, or another one.'), 'the refusal toasts');
  assert.equal(consumeConsoleErrors('[engine] ASIO driver switch failed').length, 1, 'and logs');
  assert.equal(after.driver.value, 'Yamaha Steinberg USB ASIO');
  assert.equal(after.saved.asioDriver, 'Yamaha Steinberg USB ASIO');
  assert.deepEqual(after.buffer, { value: '512', options: [512] });

  // 6. A switch to a driver at another rate while the engine holds loops: confirmed, but the recovery
  // save fails, so the loops stay and the device must run again on the driver that ran. The store
  // switches back first; reopening the old request on the new driver would be refused again (silence).
  await page.evaluate(async () => {
    const { autosave } = await import('/src/session/autosave.ts');
    window.__saveNow = autosave.saveNow;
    autosave.saveNow = async () => {
      throw new Error('injected: the recovery save failed');
    };
  });
  const switching = pickDriver('RME at 44.1 kHz');
  // The app's own ask (`src/ui/confirm/`): yes.
  const ask = page.getByRole('alertdialog', { name: 'Sample rate change', exact: true });
  await ask.getByRole('button', { name: 'Switch', exact: true }).click({ timeout: 5000 });
  const kept = await switching;
  console.log('save failed', JSON.stringify(kept));
  const back = kept.indexOf('switch:Yamaha Steinberg USB ASIO');
  assert.ok(back > kept.indexOf('switch:RME at 44.1 kHz'), 'the store switches back to the driver that ran');
  assert.equal(kept[back + 1], 'open:Asio:64', 'then reopens its request');
  const afterSave = await page.evaluate(async () => {
    const { platform } = await import('/src/platform/index.ts');
    const { engineDevice } = await import('/src/ui/state/engine-store.ts');
    return {
      runs: (await platform.engine.status())?.sampleRate ?? null,
      shown: engineDevice()?.outputName ?? null,
      saved: JSON.parse(localStorage.getItem('lf.audioDevices')).asioDriver,
      toasts: window.__lf.notify.toasts().map((t) => t.message),
    };
  });
  console.log('after the failed save', JSON.stringify(afterSave));
  assert.deepEqual(
    { runs: afterSave.runs, shown: afterSave.shown, saved: afterSave.saved },
    { runs: 48000, shown: 'Yamaha Steinberg USB ASIO', saved: 'Yamaha Steinberg USB ASIO' },
    'the device runs on the driver that ran, and the pick says so',
  );
  assert.ok(afterSave.toasts.includes('Could not switch the audio device. Try again, or export the loops first.'), 'the failed save toasts');
  assert.equal(consumeConsoleErrors('[engine] the recovery save before a rate change failed').length, 1, 'and logs');
  await page.evaluate(async () => {
    const { autosave } = await import('/src/session/autosave.ts');
    autosave.saveNow = window.__saveNow;
  });

  // 7. The device runs: the slot note is the plugin scan's again.
  await page.waitForFunction(() => !document.querySelector('.slot__plugin-note')?.textContent?.includes('No audio device open'));

  // 8. A boot probe with the saved driver gone (uninstalled since it was picked): the scripted host, as
  // the native one, takes the first driver; the player hears it from a toast, not only from the log.
  const bootProbe = (asioDriver, fail = false) =>
    page.evaluate(
      async ({ asioDriver, fail }) => {
        const settings = await import('/src/ui/state/audio-devices.ts');
        const saved = JSON.parse(localStorage.getItem('lf.audioDevices'));
        localStorage.setItem('lf.audioDevices', JSON.stringify({ ...saved, asioDriver }));
        for (const t of window.__lf.notify.toasts()) window.__lf.notify.dismissToast(t.id);
        window.__asioFake.failNext = fail;
        const report = await settings.probeAsio(false);
        return {
          status: report.status,
          toasts: window.__lf.notify.toasts().map((t) => ({ message: t.message, detail: t.detail ?? '', count: t.count })),
        };
      },
      { asioDriver, fail },
    );
  const missing = (r) => r.toasts.filter((t) => /not installed/.test(t.message));
  const installed = await bootProbe('Yamaha Steinberg USB ASIO');
  console.log('installed pick', JSON.stringify(installed));
  assert.equal(installed.status, 'ready');
  assert.deepEqual(missing(installed), [], 'an installed pick says nothing');
  const automatic = await bootProbe('');
  assert.deepEqual(missing(automatic), [], 'Automatic says nothing');
  const failedGone = await bootProbe('Old Interface ASIO', true);
  console.log('gone, no driver', JSON.stringify(failedGone));
  assert.equal(failedGone.status, 'failed');
  assert.deepEqual(missing(failedGone), [], 'nothing replaced a pick when no driver started');
  const gone = await bootProbe('Old Interface ASIO');
  console.log('gone', JSON.stringify(gone));
  assert.equal(gone.status, 'ready');
  assert.deepEqual(missing(gone), [
    {
      message: 'ASIO driver Old Interface ASIO is not installed',
      detail: 'Playing through Focusrite USB ASIO instead. Pick a driver in Audio Settings.',
      count: 1,
    },
  ]);
  // A request, not a finished action: the toast is a note (no ✓, and a screen reader hears "Note").
  const goneToast = await page.locator('.toast', { hasText: 'is not installed' }).evaluate((el) => ({
    note: el.classList.contains('toast--note'), spoken: el.querySelector('.lf-visually-hidden')?.textContent }));
  assert.deepEqual(goneToast, { note: true, spoken: 'Note' }, 'the missing driver toasts as a note');
  const goneRow = await readSettings();
  console.log('gone row', JSON.stringify(goneRow.driver));
  assert.equal(goneRow.saved.asioDriver, 'Old Interface ASIO', 'the pick stays saved');
  assert.equal(goneRow.driver.value, 'Old Interface ASIO');
  assert.ok(goneRow.driver.options.includes('Old Interface ASIO (not installed)'), 'the Driver row says so');
});
