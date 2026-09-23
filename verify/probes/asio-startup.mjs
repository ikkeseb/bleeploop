/**
 * ASIO startup flows through the real frontend orchestration with an instrumented host: a saved
 * "off" never asks for the driver; a saved "on" probes at boot before plugins become selectable; a
 * blocked/failed probe offers RETRY and an explicit retry can publish; timed-out offers no retry;
 * not-compiled / disabled-by-flag hide the toggle. Drives the ASIO startup coordinator's frontend
 * half — the native state machine itself is `cargo test` in `src-tauri/src/asio_startup.rs` — and
 * makes no hardware claim. Run: pnpm probe asio-startup
 */
import { probe } from '../harness/probe.ts';
import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';

await probe(async ({ open }) => {

async function scenario(name, saved, hostScript) {
  const { page } = await open({
    viewport: { width: 1100, height: 760 },
    init: (p) => p.addInitScript((s) => localStorage.setItem('lf.audioDevices', JSON.stringify(s)), saved),
  });
  const result = await page.evaluate(hostScript);
  await mkdir('logs/layout', { recursive: true });
  await page.screenshot({ path: `logs/layout/asio-startup-${name}.png` });
  await page.close();
  console.log(name, JSON.stringify(result));
  return result;
}

// Shared instrumented-host setup, evaluated in the page. `plan` is a list of statuses the host's
// asioProbe returns in order; `initial` is what asioStatus reports before any probe.
const setup = `
  const { platform } = await import('/src/platform/index.ts');
  const { pluginBridge } = await import('/src/audio/plugin-bridge.ts');
  const { bootPluginHost } = await import('/src/app/boot.ts');
  const settings = await import('/src/audio/audio-devices.ts');
  const instrument = await import('/src/audio/instrument.ts');
  const calls = [];
  const host = platform.pluginHost;
  host.available = true;
  pluginBridge.init = async () => {};
  host.init = async () => { calls.push('init'); };
  host.listLoaded = async () => [];
  host.listInputDevices = async () => [];
  host.listOutputDevices = async () => [];
  host.setBufferSize = async (frames) => { calls.push('buffer:' + frames); };
  host.setAsioEnabled = async (enabled) => { calls.push('asio:' + enabled); };
  let status = { status: INITIAL, detail: '' };
  const plan = PLAN;
  host.asioStatus = async () => { calls.push('status'); return status; };
  host.asioProbe = async (explicit) => {
    calls.push('probe:' + explicit);
    const next = plan.shift();
    status = typeof next === 'string' ? { status: next, detail: next === 'timed-out' ? 'the ASIO driver did not respond within 15 s; restart BleepLoop to try again' : next === 'failed' ? 'no usable ASIO driver found' : '' } : next;
    return status;
  };
  host.asioAvailable = async () => status.status === 'ready';
  host.asioDeviceInfo = async () => (status.status === 'ready' ? { name: 'Probe ASIO', inputChannels: 4, outputChannels: 2 } : null);
  host.scanPlugins = async () => { calls.push('scan'); return []; };
  const dispose = bootPluginHost();
  await new Promise((resolve) => setTimeout(resolve, 200));
  dispose();
  const startup = [...calls];
  const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  window.__lf.ui.openSettings();
  await wait(60);
  const q = (sel) => document.querySelector(sel);
  const read = () => ({
    status: settings.asioStatus().status,
    available: settings.asioAvailable(),
    enabled: settings.asioEnabled(),
    toggle: q('[aria-label="Use ASIO low-latency audio"]') ? { checked: q('[aria-label="Use ASIO low-latency audio"]').checked, disabled: q('[aria-label="Use ASIO low-latency audio"]').disabled, text: q('.audio-settings__toggle-text')?.textContent } : null,
    soon: [...document.querySelectorAll('.audio-settings__row')].find((r) => r.textContent.includes('ASIO'))?.querySelector('.audio-settings__soon')?.textContent ?? null,
    hint: q('.audio-settings__hint--asio')?.textContent ?? null,
    retry: !!q('[aria-label="Retry starting the ASIO driver"]'),
    inputDisabled: q('[aria-label="Audio input device"]').disabled,
    inputName: q('[aria-label="Audio input device"]').selectedOptions[0].textContent,
    aboutBuild: (() => { window.__lf.ui.openHelp(); const has = !!document.querySelector('.help__asio'); window.__lf.ui.closeHelp(); window.__lf.ui.openSettings(); return has; })(),
  });
`;

const script = (initial, plan, body) => `(async () => { ${setup.replace('INITIAL', JSON.stringify(initial)).replace('PLAN', JSON.stringify(plan))} ${body} })()`;

// A: saved OFF → boot asks for status only; turning the toggle on runs the explicit probe.
const a = await scenario('saved-off', { asioEnabled: false }, script('unprobed', ['ready'], `
  const before = read();
  q('[aria-label="Use ASIO low-latency audio"]').click();
  await wait(120);
  const after = read();
  return { startup, before, after, calls: calls.slice(startup.length) };
`));
assert.ok(!a.startup.includes('probe:false') && !a.startup.includes('probe:true'), 'saved OFF must never request the driver at boot');
assert.ok(a.startup.includes('status'), 'boot reads the status');
assert.equal(a.before.status, 'unprobed');
assert.deepEqual(a.before.toggle, { checked: false, disabled: false, text: 'WASAPI' });
assert.equal(a.before.retry, false, 'no retry line while the preference is off');
assert.equal(a.before.aboutBuild, true, 'About this build shows whenever ASIO is compiled in');
assert.deepEqual(a.calls, ['asio:true', 'probe:true'], 'turning ASIO on = preference write, then the explicit probe');
assert.equal(a.after.status, 'ready');
assert.equal(a.after.available, true);
assert.deepEqual(a.after.toggle, { checked: true, disabled: false, text: 'low-latency' });
assert.equal(a.after.inputDisabled, true);
assert.equal(a.after.inputName, 'Probe ASIO');

// B: saved ON, previous attempt never completed → boot probe (before scan) returns blocked; RETRY publishes.
const b = await scenario('blocked-retry', { asioEnabled: true }, script('unprobed', ['blocked', 'ready'], `
  const before = read();
  q('[aria-label="Retry starting the ASIO driver"]').click();
  await wait(120);
  const after = read();
  return { startup, before, after, calls: calls.slice(startup.length) };
`));
assert.ok(b.startup.indexOf('probe:false') >= 0 && b.startup.indexOf('probe:false') < b.startup.indexOf('scan'), 'saved ON probes at boot, before plugins become selectable');
assert.equal(b.before.status, 'blocked');
assert.deepEqual(b.before.toggle, { checked: true, disabled: false, text: 'WASAPI until the driver starts' });
assert.match(b.before.hint ?? '', /previous ASIO start did not complete/);
assert.equal(b.before.retry, true);
assert.deepEqual(b.calls, ['probe:true']);
assert.equal(b.after.status, 'ready');
assert.equal(b.after.retry, false);
assert.deepEqual(b.after.toggle, { checked: true, disabled: false, text: 'low-latency' });

// C: timed-out → no retry in this process, the restart sentence shows, ASIO stays off for arms.
const c = await scenario('timed-out', { asioEnabled: true }, script('unprobed', ['timed-out'], `return { startup, state: read() };`));
assert.equal(c.state.status, 'timed-out');
assert.equal(c.state.available, false);
assert.equal(c.state.retry, false, 'a timed-out probe offers no in-process retry');
assert.match(c.state.hint ?? '', /restart BleepLoop/);

// D: failed → retry offered; toggling off then on re-probes explicitly.
const d = await scenario('failed', { asioEnabled: true }, script('unprobed', ['failed', 'ready'], `
  const before = read();
  q('[aria-label="Use ASIO low-latency audio"]').click(); // off
  await wait(60);
  const off = read();
  q('[aria-label="Use ASIO low-latency audio"]').click(); // on again → explicit probe
  await wait(120);
  const after = read();
  return { before, off, after, calls: calls.slice(startup.length) };
`));
assert.equal(d.before.status, 'failed');
assert.match(d.before.hint ?? '', /no usable ASIO driver found/);
assert.equal(d.before.retry, true);
assert.equal(d.off.hint, null, 'turning the preference off hides the status line');
assert.deepEqual(d.calls, ['asio:false', 'asio:true', 'probe:true']);
assert.equal(d.after.status, 'ready');

// E: not compiled / disabled by flag → no toggle, an explanatory word, no About-this-build for not-compiled.
const e1 = await scenario('not-compiled', { asioEnabled: true }, script('not-compiled', [], `return { startup, state: read() };`));
assert.ok(!e1.startup.some((c) => c.startsWith('probe:')), 'not-compiled never probes');
assert.equal(e1.state.toggle, null);
assert.equal(e1.state.soon, 'unavailable');
assert.equal(e1.state.aboutBuild, false);
const e2 = await scenario('disabled-by-flag', { asioEnabled: true }, script('disabled-by-flag', [], `return { startup, state: read() };`));
assert.ok(!e2.startup.some((c) => c.startsWith('probe:')), 'the launch flag never probes');
assert.equal(e2.state.toggle, null);
assert.match(e2.state.soon ?? '', /--disable-asio/);
assert.equal(e2.state.aboutBuild, true, 'the licence/About section is about the binary, not the launch');

}, { launch: {} });
