/**
 * ASIO startup flows through the real frontend orchestration (`src/app/boot.ts` `bootEngine`,
 * `src/ui/state/audio-devices.ts`, the Audio Settings ASIO row), in engine mode on the web engine fake
 * (`window.__lfEngineFake`) with the native chrome on: `src/platform/host.web.ts` is served with the plugin
 * host `available` and a scripted ASIO host installed before the app boots (the `asio-driver` pattern).
 * Each scenario is a fresh page with a saved preference:
 *
 * - saved "off" never asks for the driver at boot (status only); turning the toggle on saves the
 *   preference BEFORE the explicit probe runs (a driver that crashes the app mid-probe leaves "on" saved,
 *   so the next launch meets the native sentinel), then opens the device on ASIO; the input select is
 *   then disabled and names the ASIO device. The host records each change of the saved preference in the
 *   call log and what the saved preference reads when each probe is called;
 * - saved "on" probes at boot, before the device opens and before plugins become selectable (the scan);
 *   a blocked probe offers RETRY and an explicit retry can publish;
 * - timed-out offers no retry and says to restart;
 * - failed offers RETRY; turning the toggle off saves "off" and never probes, on again saves "on" and
 *   then probes explicitly;
 * - not-compiled / disabled-by-flag hide the toggle (an explanatory word instead) and never probe;
 *   About this build shows the ASIO lines for every build that links the SDK.
 *
 * The native state machine is `cargo test` in `src-tauri/src/asio_startup.rs`; this probe drives no
 * driver and makes no hardware claim: the fake engine opens whatever is asked.
 * Run: pnpm probe asio-startup
 */
import { probe } from '../harness/probe.ts';
import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';

const DETAIL = {
  'timed-out': 'the ASIO driver did not respond within 15 s; restart BleepLoop to try again',
  failed: 'no usable ASIO driver found',
};

await probe(async ({ open }) => {
  await mkdir('logs/layout', { recursive: true });

  /** A fresh app whose saved device settings are `saved`, whose host reports `initial` before any probe
   * and answers each probe with the next status of `plan`; runs `body` in the page once boot scanned. */
  async function scenario(name, saved, initial, plan, body) {
    const { page, consoleErrors } = await open({
      viewport: { width: 1100, height: 760 },
      init: async (p) => {
        await p.route('**/src/platform/host.web.ts*', async (route) => {
          const response = await route.fetch();
          const text = await response.text();
          if (!text.includes('available: false')) throw new Error('Could not script host.web.ts');
          const body = `${text.replace('available: false', 'available: true')}\n;globalThis.__lfHostScript?.(webPluginHost, webEngineFake);\n`;
          await route.fulfill({ response, body });
        });
        await p.addInitScript(
          ({ saved, initial, plan, detail }) => {
            window.__lfEngineFake = true;
            localStorage.setItem('lf.audioDevices', JSON.stringify(saved));
            const calls = (window.__calls = []);
            // Each change of the saved ASIO preference lands in the call log where it happens (`asio:<on>`),
            // so its order against the host's probe is on record.
            const savedOn = () => JSON.parse(localStorage.getItem('lf.audioDevices') ?? '{}').asioEnabled === true;
            let lastSaved = savedOn();
            const setItem = Storage.prototype.setItem;
            Storage.prototype.setItem = function (key, value) {
              setItem.call(this, key, value);
              if (key !== 'lf.audioDevices' || savedOn() === lastSaved) return;
              lastSaved = savedOn();
              calls.push(`asio:${lastSaved}`);
            };
            // What the saved preference read when each probe was called.
            const savedAtProbe = (window.__savedAtProbe = []);
            window.__lfHostScript = (host, engine) => {
              let status = { status: initial, detail: '' };
              host.init = async () => void calls.push('init');
              host.listLoaded = async () => [];
              host.listInputDevices = async () => [];
              host.listOutputDevices = async () => [];
              host.asioDrivers = async () => [];
              host.asioStatus = async () => {
                calls.push('status');
                return status;
              };
              host.asioProbe = async (explicit) => {
                calls.push(`probe:${explicit}`);
                savedAtProbe.push(savedOn());
                const next = plan.shift();
                status = { status: next, detail: detail[next] ?? '' };
                return status;
              };
              host.asioDeviceInfo = async () =>
                status.status === 'ready' ? { name: 'Probe ASIO', inputChannels: 4, outputChannels: 2, bufferMin: null, bufferMax: null } : null;
              host.scanPlugins = async () => {
                calls.push('scan');
                return [];
              };
              const openDevice = engine.open.bind(engine);
              engine.open = async (request, force) => {
                calls.push(`open:${request.backend}`);
                return openDevice(request, force);
              };
            };
          },
          { saved, initial, plan, detail: DETAIL },
        );
      },
    });
    await page.waitForFunction(() => window.__calls.includes('scan'), undefined, { timeout: 10_000 });
    const result = await page.evaluate(`(async () => {
      const settings = await import('/src/ui/state/audio-devices.ts');
      const calls = window.__calls;
      const startup = [...calls];
      const savedAtProbe = window.__savedAtProbe;
      const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
      const savedOn = () => JSON.parse(localStorage.getItem('lf.audioDevices') ?? '{}').asioEnabled;
      window.__lf.ui.openSettings();
      await wait(60);
      const q = (sel) => document.querySelector(sel);
      const read = () => ({
        status: settings.asioStatus().status,
        available: settings.asioAvailable(),
        enabled: settings.asioEnabled(),
        saved: savedOn(),
        toggle: q('[aria-label="Use ASIO low-latency audio"]') ? { checked: q('[aria-label="Use ASIO low-latency audio"]').checked, disabled: q('[aria-label="Use ASIO low-latency audio"]').disabled, text: q('.audio-settings__toggle-text')?.textContent } : null,
        soon: [...document.querySelectorAll('.audio-settings__row')].find((r) => r.textContent.includes('ASIO'))?.querySelector('.audio-settings__soon')?.textContent ?? null,
        hint: q('.audio-settings__hint--asio')?.textContent ?? null,
        retry: !!q('[aria-label="Retry starting the ASIO driver"]'),
        inputDisabled: q('[aria-label="Audio input device"]').disabled,
        inputName: q('[aria-label="Audio input device"]').selectedOptions[0].textContent,
        aboutBuild: (() => { window.__lf.ui.openHelp(); const has = !!document.querySelector('.help__asio'); window.__lf.ui.closeHelp(); window.__lf.ui.openSettings(); return has; })(),
      });
      ${body}
    })()`);
    await page.screenshot({ path: `logs/layout/asio-startup-${name}.png` });
    console.log(name, JSON.stringify(result));
    assert.deepEqual(consoleErrors, [], `${name}: no console errors`);
    await page.close();
    return result;
  }

  // A: saved OFF → boot asks for status only; turning the toggle on runs the explicit probe.
  const a = await scenario('saved-off', { asioEnabled: false }, 'unprobed', ['ready'], `
    const before = read();
    q('[aria-label="Use ASIO low-latency audio"]').click();
    await wait(200);
    const after = read();
    return { startup, before, after, calls: calls.slice(startup.length), savedAtProbe };
  `);
  assert.ok(!a.startup.includes('probe:false') && !a.startup.includes('probe:true'), 'saved OFF must never request the driver at boot');
  assert.ok(a.startup.includes('status'), 'boot reads the status');
  assert.ok(a.startup.includes('open:Wasapi'), 'saved OFF opens the device on WASAPI');
  assert.equal(a.before.status, 'unprobed');
  assert.deepEqual(a.before.toggle, { checked: false, disabled: false, text: 'WASAPI' });
  assert.equal(a.before.retry, false, 'no retry line while the preference is off');
  assert.equal(a.before.aboutBuild, true, 'About this build shows whenever ASIO is compiled in');
  assert.equal(a.after.saved, true, 'turning ASIO on saves the preference');
  assert.deepEqual(a.calls, ['asio:true', 'probe:true', 'open:Asio'], 'turning ASIO on = the preference saved, then the explicit probe, then the device opens on ASIO');
  assert.deepEqual(a.savedAtProbe, [true], 'the explicit probe runs with "on" already saved');
  assert.equal(a.after.status, 'ready');
  assert.equal(a.after.available, true);
  assert.deepEqual(a.after.toggle, { checked: true, disabled: false, text: 'low-latency' });
  assert.equal(a.after.inputDisabled, true);
  assert.equal(a.after.inputName, 'Probe ASIO');

  // B: saved ON, previous attempt never completed → boot probe (before scan) returns blocked; RETRY publishes.
  const b = await scenario('blocked-retry', { asioEnabled: true }, 'unprobed', ['blocked', 'ready'], `
    const before = read();
    q('[aria-label="Retry starting the ASIO driver"]').click();
    await wait(200);
    const after = read();
    return { startup, before, after, calls: calls.slice(startup.length), savedAtProbe };
  `);
  const bootProbe = b.startup.indexOf('probe:false');
  assert.ok(bootProbe >= 0 && bootProbe < b.startup.indexOf('scan'), 'saved ON probes at boot, before plugins become selectable');
  assert.ok(bootProbe < b.startup.findIndex((c) => c.startsWith('open:')), 'the boot probe settles before the device opens');
  assert.equal(b.before.status, 'blocked');
  assert.deepEqual(b.before.toggle, { checked: true, disabled: false, text: 'WASAPI until the driver starts' });
  assert.match(b.before.hint ?? '', /previous ASIO start did not complete/);
  assert.equal(b.before.retry, true);
  assert.deepEqual(b.calls.filter((c) => c.startsWith('probe:')), ['probe:true']);
  assert.deepEqual(b.savedAtProbe, [true, true], 'the boot probe and RETRY run with "on" saved');
  assert.equal(b.after.status, 'ready');
  assert.equal(b.after.retry, false);
  assert.deepEqual(b.after.toggle, { checked: true, disabled: false, text: 'low-latency' });

  // C: timed-out → no retry in this process, the restart sentence shows, ASIO stays off.
  const c = await scenario('timed-out', { asioEnabled: true }, 'unprobed', ['timed-out'], `return { startup, state: read() };`);
  assert.equal(c.state.status, 'timed-out');
  assert.equal(c.state.available, false);
  assert.equal(c.state.retry, false, 'a timed-out probe offers no in-process retry');
  assert.match(c.state.hint ?? '', /restart BleepLoop/);
  assert.ok(!c.startup.includes('open:Asio'), 'a timed-out driver is not opened');

  // D: failed → retry offered; toggling off then on re-probes explicitly.
  const d = await scenario('failed', { asioEnabled: true }, 'unprobed', ['failed', 'ready'], `
    const before = read();
    q('[aria-label="Use ASIO low-latency audio"]').click(); // off
    await wait(120);
    const off = read();
    const offCalls = calls.slice(startup.length);
    q('[aria-label="Use ASIO low-latency audio"]').click(); // on again → explicit probe
    await wait(200);
    const after = read();
    return { before, off, offCalls, after, calls: calls.slice(startup.length), savedAtProbe };
  `);
  assert.equal(d.before.status, 'failed');
  assert.match(d.before.hint ?? '', /no usable ASIO driver found/);
  assert.equal(d.before.retry, true);
  assert.equal(d.off.hint, null, 'turning the preference off hides the status line');
  assert.equal(d.off.saved, false, 'turning ASIO off saves the preference');
  assert.ok(!d.offCalls.some((call) => call.startsWith('probe:')), 'turning ASIO off never touches the driver');
  assert.equal(d.after.saved, true);
  assert.deepEqual(
    d.calls.filter((call) => call.startsWith('asio:') || call.startsWith('probe:')),
    ['asio:false', 'asio:true', 'probe:true'],
    'off saves "off" and never probes; on again saves "on" before the explicit probe',
  );
  assert.deepEqual(d.savedAtProbe, [true, true], 'the boot probe and the explicit one run with "on" saved');
  assert.equal(d.calls.at(-1), 'open:Asio', 'then the device opens on ASIO');
  assert.equal(d.after.status, 'ready');

  // E: not compiled / disabled by flag → no toggle, an explanatory word, no About-this-build for not-compiled.
  const e1 = await scenario('not-compiled', { asioEnabled: true }, 'not-compiled', [], `return { startup, state: read() };`);
  assert.ok(!e1.startup.some((call) => call.startsWith('probe:')), 'not-compiled never probes');
  assert.equal(e1.state.toggle, null);
  assert.equal(e1.state.soon, 'unavailable');
  assert.equal(e1.state.aboutBuild, false);
  const e2 = await scenario('disabled-by-flag', { asioEnabled: true }, 'disabled-by-flag', [], `return { startup, state: read() };`);
  assert.ok(!e2.startup.some((call) => call.startsWith('probe:')), 'the launch flag never probes');
  assert.equal(e2.state.toggle, null);
  assert.match(e2.state.soon ?? '', /--disable-asio/);
  assert.equal(e2.state.aboutBuild, true, 'the licence/About section is about the binary, not the launch');
});
