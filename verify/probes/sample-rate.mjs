/**
 * The Sample rate select (the owner's C2 pick) in engine mode, on the web engine fake
 * (`window.__lfEngineFake`) with the native chrome on (host.web.ts served with `available: true`) and a
 * scripted ASIO driver whose own rate is 44.1 kHz:
 *
 * - the row shows the device's rate ("Device (44.1 kHz)") and the rates the driver runs;
 * - picking 48 kHz while the engine holds loops: the open asks for `sampleRate: 48000`, the engine
 *   refuses it (`OpenError::RateChange`, scripted as the native owner answers) and the existing confirm
 *   appears; declined, the pick goes back to the device's rate; accepted, the forced open runs at 48 kHz
 *   and the row shows it;
 * - the pick survives a reload: the first open asks for it and the row shows it;
 * - a rate, buffer, ASIO on/off or output device pick whose open fails (the device that ran keeps
 *   running, or the native owner reopens it): every pick that ran goes back, saved and shown;
 * - a fallback the owner made on its own (ASIO lost, WASAPI at its endpoint's rate, scripted on the
 *   feed): the row shows what runs, and the pick stays saved for the next ASIO open;
 * - a driver that runs only 44.1 kHz: no 48 kHz option, the saved pick kept, the device's rate shown;
 * - WASAPI: the device's rate alone, and the hint says Windows sets it.
 *
 * The native half (the driver's rates, the pick's resolution, the refusal and the rebuild) is
 * `cargo test` (`engine_io/transition.rs`, `engine_io/tests.rs`); this probe sees neither a driver nor
 * cpal, and no lane: the refusal stands in for the engine holding audio. logs/layout/sample-rate.png
 * shows the panel.
 * Run: pnpm probe sample-rate
 */
import { probe } from '../harness/probe.ts';
import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';

/** Runs before the app: a ready ASIO driver at 44.1 kHz that runs `window.__driver.rates`. */
function installDriver() {
  const driver = (window.__driver = { rate: 44100, rates: [44100, 48000], holdsLoops: false });
  window.__rateHost = {
    asioStatus: async () => ({ status: 'ready', detail: '' }),
    asioProbe: async () => ({ status: 'ready', detail: '' }),
    asioDrivers: async () => ['Probe ASIO'],
    listOutputDevices: async () => [
      { id: 'out-a', name: 'Probe Output A', channels: 2 },
      { id: 'out-b', name: 'Probe Output B', channels: 2 },
    ],
    asioDeviceInfo: async () => ({
      name: 'Probe ASIO',
      inputChannels: 2,
      outputChannels: 2,
      bufferMin: null,
      bufferMax: null,
      sampleRates: [...driver.rates],
    }),
  };
  // As the native owner resolves a request (`transition::open_rate`): ASIO runs the pick where the
  // driver runs it, else its own; WASAPI the endpoint's own. While the engine holds loops, another rate
  // than the one running is refused unless forced.
  window.__rateFor = (request) =>
    request.backend === 'Asio' && request.sampleRate !== null && driver.rates.includes(request.sampleRate) ? request.sampleRate : driver.rate;
}

const serveHost = (p) =>
  p.route('**/src/platform/host.web.ts', async (route) => {
    const response = await route.fetch();
    const body =
      (await response.text()).replace('available: false', 'available: true') +
      `
Object.assign(webPluginHost, window.__rateHost ?? {});
const __probeOpen = webEngineFake.open;
webEngineFake.open = async (request, force) => {
  if (window.__driver.fail) {
    // As the native owner answers a device that did not start: an error, the device that ran reopened.
    webEngineFake.opened.push(request);
    webEngineFake.forced.push(force ?? false);
    throw window.__driver.fail;
  }
  const rate = window.__rateFor(request);
  const running = globalThis.__lfEngineFakeRate ?? 48000;
  webEngineFake.refusal = window.__driver.holdsLoops && rate !== running ? { device: 'Probe ASIO', from: running, to: rate } : null;
  if (!webEngineFake.refusal || force) globalThis.__lfEngineFakeRate = rate;
  return __probeOpen(request, force);
};
`;
    await route.fulfill({ response, body });
  });

/** The Sample rate row as the panel shows it. */
const readRow = (page) =>
  page.evaluate(async () => {
    window.__lf.ui.openSettings();
    await new Promise((resolve) => setTimeout(resolve, 80));
    const select = document.querySelector('[aria-label="Sample rate"]');
    const row = select.closest('.audio-settings__row');
    const out = {
      value: select.value,
      options: [...select.options].map((o) => [o.value, o.textContent.trim()]),
      hint: row.nextElementSibling?.textContent?.trim() ?? null,
      saved: JSON.parse(localStorage.getItem('lf.audioDevices') ?? '{}').sampleRate ?? null,
      runs: window.__lf.native.opened.length ? (globalThis.__lfEngineFakeRate ?? 48000) : null,
    };
    window.__lf.ui.closeSettings();
    return out;
  });

/** Pick `value` in the select (or set the checkbox) labelled `label`; resolves once the open it queued
 * finished. */
const pick = (page, label, value) =>
  page.evaluate(async ([l, v]) => {
    window.__lf.ui.openSettings();
    await new Promise((resolve) => setTimeout(resolve, 80));
    const before = window.__lf.native.opened.length;
    const control = document.querySelector(`[aria-label="${l}"]`);
    if (control.type === 'checkbox') control.checked = v;
    else control.value = v;
    control.dispatchEvent(new Event('change', { bubbles: true }));
    const { openEngineDevice } = await import('/src/ui/state/engine-store.ts');
    while (window.__lf.native.opened.length === before) await new Promise((resolve) => setTimeout(resolve, 10));
    await openEngineDevice(); // queued behind the pick's open: resolves once it finished
    const opened = window.__lf.native.opened.slice(before);
    const forced = window.__lf.native.forced.slice(before);
    window.__lf.ui.closeSettings();
    return opened.map((r, i) => ({ sampleRate: r.sampleRate, backend: r.backend, buffer: r.buffer, output: r.output, forced: forced[i] }));
  }, [label, value]);
const pickRate = async (page, value) =>
  (await pick(page, 'Sample rate', value)).map(({ sampleRate, backend, forced }) => ({ sampleRate, backend, forced }));

/** The control labelled `label` and the saved setting `key`, as the panel shows them. */
const readPick = (page, label, key) =>
  page.evaluate(async ([l, k]) => {
    window.__lf.ui.openSettings();
    await new Promise((resolve) => setTimeout(resolve, 80));
    const control = document.querySelector(`[aria-label="${l}"]`);
    const { readAudioDeviceSettings } = await import('/src/ui/state/audio-settings.ts');
    const out = { shown: control.type === 'checkbox' ? control.checked : control.value, saved: readAudioDeviceSettings()[k] };
    window.__lf.ui.closeSettings();
    return out;
  }, [label, key]);

/** Pick `value` in `label` while the device fails to open: every pick that ran goes back. */
async function failedPick(page, label, key, value, asked) {
  const before = await readPick(page, label, key);
  await page.evaluate(() => (window.__driver.fail = 'the device did not start'));
  const opens = await pick(page, label, value);
  await page.evaluate(() => (window.__driver.fail = null));
  const after = await readPick(page, label, key);
  console.log(`failed ${label}`, JSON.stringify({ before, opens, after }));
  assert.deepEqual(Object.fromEntries(Object.keys(asked).map((k) => [k, opens[0][k]])), asked, `the ${label} pick asks for ${value}`);
  assert.deepEqual(after, before, `a failed ${label} pick goes back, saved and shown`);
}

await probe(async ({ open }) => {
  const { page } = await open({
    viewport: { width: 960, height: 820 },
    init: async (p) => {
      await serveHost(p);
      await p.addInitScript(() => {
        window.__lfEngineFake = true;
        if (!localStorage.getItem('lf.audioDevices')) localStorage.setItem('lf.audioDevices', JSON.stringify({ asioEnabled: true }));
      });
      await p.addInitScript(installDriver);
    },
  });

  // 1. Boot: the first open asks for the device's own rate, and the row says which it is.
  await page.waitForFunction(() => window.__lf.native.opened.length > 0, undefined, { timeout: 10000 });
  const boot = await page.evaluate(() => window.__lf.native.opened[0]);
  assert.equal(boot.backend, 'Asio');
  assert.equal(boot.sampleRate, null, 'no pick: the device rate');
  const first = await readRow(page);
  console.log('boot', JSON.stringify(first));
  assert.deepEqual(first.options, [['', 'Device (44.1 kHz)'], ['44100', '44.1 kHz'], ['48000', '48 kHz']]);
  assert.deepEqual({ value: first.value, runs: first.runs }, { value: '', runs: 44100 });

  // 2. 48 kHz while the engine holds loops: the existing confirm; declined, nothing changes.
  await page.evaluate(async () => {
    window.__driver.holdsLoops = true;
    const { autosave } = await import('/src/session/autosave.ts');
    window.__saves = 0;
    autosave.saveNow = async () => {
      window.__saves++;
    };
  });
  let asked = null;
  page.once('dialog', (d) => {
    asked = d.message();
    void d.dismiss();
  });
  const declined = await pickRate(page, '48000');
  console.log('declined', JSON.stringify(declined), asked);
  assert.deepEqual(declined[0], { sampleRate: 48000, backend: 'Asio', forced: false }, 'the pick asks for 48 kHz');
  assert.match(asked ?? '', /Probe ASIO runs at 48 kHz\. Your loops were recorded at 44\.1 kHz/, 'the existing confirm');
  const kept = await readRow(page);
  assert.deepEqual({ value: kept.value, saved: kept.saved, runs: kept.runs }, { value: '', saved: null, runs: 44100 }, 'declined: the device rate stays');

  // Accepted: the recovery saves first, then the forced open runs at the pick.
  page.once('dialog', (d) => void d.accept());
  const accepted = await pickRate(page, '48000');
  console.log('accepted', JSON.stringify(accepted));
  assert.deepEqual(accepted.slice(0, 2), [
    { sampleRate: 48000, backend: 'Asio', forced: false },
    { sampleRate: 48000, backend: 'Asio', forced: true },
  ]);
  assert.equal(await page.evaluate(() => window.__saves), 1, 'the loops go to the recovery first');
  const picked = await readRow(page);
  console.log('picked', JSON.stringify(picked));
  assert.deepEqual(picked.options, [['', 'Device'], ['44100', '44.1 kHz'], ['48000', '48 kHz']]);
  assert.deepEqual({ value: picked.value, saved: picked.saved, runs: picked.runs }, { value: '48000', saved: 48000, runs: 48000 });
  await mkdir('logs/layout', { recursive: true });
  await page.evaluate(() => window.__lf.ui.openSettings());
  await page.locator('[aria-label="Sample rate"]').scrollIntoViewIfNeeded();
  await page.screenshot({ path: 'logs/layout/sample-rate.png' });
  await page.evaluate(() => window.__lf.ui.closeSettings());

  // 3. A reload: the saved pick opens and shows.
  await page.reload();
  await page.waitForFunction(() => window.__lf?.native?.opened.length > 0, undefined, { timeout: 10000 });
  assert.equal(await page.evaluate(() => window.__lf.native.opened[0].sampleRate), 48000, 'the first open asks for the pick');
  const reloaded = await readRow(page);
  console.log('reloaded', JSON.stringify(reloaded));
  assert.deepEqual({ value: reloaded.value, saved: reloaded.saved, runs: reloaded.runs }, { value: '48000', saved: 48000, runs: 48000 });

  // 4. A pick whose open fails: the native owner reopens the device that ran (`owner.rs` `open`), so the
  // pick that ran goes back, saved and shown.
  await page.evaluate(() => (window.__driver.fail = 'the device did not start'));
  const failed = await pickRate(page, '44100');
  await page.evaluate(() => (window.__driver.fail = null));
  assert.deepEqual(failed[0], { sampleRate: 44100, backend: 'Asio', forced: false }, 'the pick asks for 44.1 kHz');
  const restored = await readRow(page);
  console.log('failed open', JSON.stringify(restored));
  assert.deepEqual({ value: restored.value, saved: restored.saved, runs: restored.runs }, { value: '48000', saved: 48000, runs: 48000 }, 'the pick that ran goes back');
  // The same for a buffer pick and ASIO off: the size and the toggle that ran go back.
  await failedPick(page, 'Buffer size in frames', 'bufferFrames', '64', { buffer: 64, backend: 'Asio' });
  await failedPick(page, 'Use ASIO low-latency audio', 'asioEnabled', false, { backend: 'Wasapi' });

  // 5. ASIO lost, the owner fell back to WASAPI at its endpoint's 44.1 kHz on its own (the feed's
  // status + Fallback): the row shows what runs, and the 48 kHz pick stays saved for the next ASIO open.
  await page.evaluate(() => {
    const status = { backend: 'Wasapi', sampleRate: 44100, block: 256, inputName: 'Fake input', outputName: 'Fake output', alignFrames: 0, inputFrames: 0, inputOpen: true, inputChannels: [1, 1] };
    window.__lf.native.emit({ seq: 1000, reset: false, events: [], status, device: [{ Fallback: status }] });
  });
  const fellBack = await readRow(page);
  console.log('fallback', JSON.stringify(fellBack));
  assert.deepEqual(fellBack.options, [['', 'Device (44.1 kHz)']], 'WASAPI runs: its rate alone');
  assert.deepEqual({ value: fellBack.value, saved: fellBack.saved }, { value: '', saved: 48000 });
  assert.match(fellBack.hint ?? '', /^Set by Windows/);

  // 6. A driver that runs only 44.1 kHz (probed again): no 48 kHz option; the device's rate runs and
  // shows, and the player's pick stays saved for a driver that runs it.
  await page.evaluate(async () => {
    window.__driver.rates = [44100];
    const settings = await import('/src/ui/state/audio-devices.ts');
    const { openEngineDevice } = await import('/src/ui/state/engine-store.ts');
    await settings.probeAsio(true);
    await openEngineDevice();
  });
  const narrow = await readRow(page);
  console.log('44.1 only', JSON.stringify(narrow));
  assert.deepEqual(narrow.options, [['', 'Device (44.1 kHz)'], ['44100', '44.1 kHz']], 'no 48 kHz option');
  assert.deepEqual({ value: narrow.value, saved: narrow.saved, runs: narrow.runs }, { value: '', saved: 48000, runs: 44100 });

  // 7. WASAPI: the endpoint's rate alone, set in Windows.
  await page.evaluate(async () => {
    const settings = await import('/src/ui/state/audio-devices.ts');
    const { openEngineDevice } = await import('/src/ui/state/engine-store.ts');
    await settings.setAsioEnabled(false);
    await openEngineDevice();
  });
  const wasapi = await readRow(page);
  console.log('wasapi', JSON.stringify(wasapi));
  assert.deepEqual(wasapi.options, [['', 'Device (44.1 kHz)']]);
  assert.match(wasapi.hint ?? '', /^Set by Windows/);

  // 8. An output device pick whose open fails: the endpoint that ran goes back, saved and shown.
  await failedPick(page, 'Output device', 'outputDeviceId', 'out-b', { output: 'out-b', backend: 'Wasapi' });
});
