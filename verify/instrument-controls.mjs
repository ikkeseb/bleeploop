// Rendered regression coverage for session keyboard state, plugin controls and MIDI startup status.
import assert from 'node:assert/strict';
import { chromium } from 'playwright';

const url = process.argv.find((arg) => arg.startsWith('--url='))?.slice(6) ?? 'http://localhost:1420';
const browser = await chromium.launch({ args: ['--autoplay-policy=no-user-gesture-required'] });

try {
  const page = await browser.newPage({ viewport: { width: 1280, height: 820 } });
  const consoleErrors = [];
  const pageErrors = [];
  page.on('console', (message) => {
    if (message.type() === 'error' || message.type() === 'warning') consoleErrors.push(message.text());
  });
  page.on('pageerror', (error) => pageErrors.push(String(error)));
  await page.addInitScript(() => {
    Object.defineProperty(navigator, 'requestMIDIAccess', {
      configurable: true,
      value: () => Promise.reject(new DOMException('nope', 'SecurityError')),
    });
  });
  await page.route('**/src/platform/host.web.ts*', async (route) => {
    const response = await route.fetch();
    let body = await response.text();
    if (!body.includes('available: false')) throw new Error('Could not enable simulated native chrome');
    if (!body.includes('onStreamFault() {')) throw new Error('Could not expose the simulated stream-fault callback');
    body = body
      .replace('available: false', 'available: true')
      .replace(
        'onStreamFault() {',
        'onStreamFault(callback) { globalThis.__instrumentControlsFault = callback;',
      );
    await route.fulfill({ response, body });
  });

  await page.goto(url);
  await page.waitForFunction(() => !!window.__lf);
  await page.waitForFunction(() => window.__lf.midi.midiStatus() === 'denied');
  await page.waitForFunction(
    () => document.querySelector('[aria-label="Rescan plugins"]')?.disabled === false,
  );

  assert.equal(await page.evaluate(() => window.__lf.midi.midiStatus()), 'denied');
  assert.ok(
    consoleErrors.some((message) => message.includes('[midi] requestMIDIAccess denied')),
    'a denied Web MIDI request must reach the console (warn)',
  );
  const midiRetry = await page.evaluate(async () => {
    await new Promise((resolve) => setTimeout(resolve, 0));
    let calls = 0;
    Object.defineProperty(navigator, 'requestMIDIAccess', {
      configurable: true,
      value: async () => {
        calls++;
        return { inputs: new Map(), onstatechange: null };
      },
    });
    await Promise.all([window.__lf.midi.retry(), window.__lf.midi.retry()]);
    return { calls, status: window.__lf.midi.midiStatus() };
  });
  assert.deepEqual(midiRetry, { calls: 1, status: 'no-devices' }, 'concurrent MIDI retries must share one request');

  const emptyPluginCopy =
    'No plugins found · CLAP in %COMMONPROGRAMFILES%\\CLAP, VST3 in %COMMONPROGRAMFILES%\\VST3 · rescan ⟳ in the command bar';
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
    const slots = await import('/src/audio/instrument-slots.ts');
    const { pluginBridge } = await import('/src/audio/plugin-bridge.ts');
    const descriptor = {
      id: 'instrument-controls',
      name: 'Instrument Controls Probe',
      path: 'C:\\probe\\instrument-controls.vst3',
      format: 'vst3',
      isEffect: true,
    };
    const capacityFrames = 1024;
    const headerBytes = 28;
    await pluginBridge.init(window.__lf.engine.ctx);
    const loadToken = pluginBridge.beginPluginLoad(0, true);
    const buffer = new ArrayBuffer(headerBytes + capacityFrames * Float32Array.BYTES_PER_ELEMENT);
    new Uint32Array(buffer, 0, headerBytes / Uint32Array.BYTES_PER_ELEMENT)[2] = capacityFrames;
    await pluginBridge.acceptPluginBuffer(buffer, {
      kind: 'plugin-audio',
      slot: 0,
      capacityFrames,
      headerBytes,
      sampleRate: window.__lf.engine.ctx.sampleRate,
      inChannels: 2,
      loadToken,
    });
    platform.pluginHost.armInput = async () => {};
    platform.pluginHost.disarmInput = async () => {};
    platform.pluginHost.armMonitor = async () => {};
    platform.pluginHost.disarmMonitor = async () => {};
    platform.pluginHost.setMonitorGain = async () => {};
    platform.pluginHost.monitorLatencySeconds = async () => 0;
    platform.pluginHost.listParams = async () => [];
    slots.setSlotPlugins([descriptor, null]);
  });

  const paramsButton = page.getByRole('button', { name: /Plugin parameters for slot 1/ });
  await paramsButton.click();
  const output = page.getByRole('slider', { name: 'Plugin output gain for slot 1', exact: true });
  await page.evaluate(() => window.__lf.setPluginGain(1, 0));
  await page.waitForFunction(() => window.__lf.pluginBridge.gains()[0] === 1);
  await output.focus();
  await page.keyboard.press('ArrowRight');
  await page.keyboard.press('ArrowRight');
  await page.keyboard.press('ArrowRight');
  assert.ok(
    await page.evaluate(() => window.__lf.pluginBridge.gains()[0] > 1),
    'keyboard steps must leave the unity detent',
  );

  await output.evaluate((element) => {
    element.value = '1.05';
    element.dispatchEvent(new Event('input', { bubbles: true }));
  });
  assert.equal(await page.evaluate(() => window.__lf.pluginBridge.gains()[0]), 1.05);
  await page.keyboard.press('ArrowLeft');
  await page.keyboard.press('ArrowLeft');
  assert.equal(
    await page.evaluate(() => window.__lf.pluginBridge.gains()[0]),
    1,
    'approaching unity must retain the soft detent',
  );

  await page.getByRole('button', { name: 'Go live for slot 1', exact: true }).click();
  await page.getByRole('button', { name: 'Stop live input for slot 1', exact: true }).waitFor();
  await page.evaluate(() => {
    if (typeof globalThis.__instrumentControlsFault !== 'function') {
      throw new Error('stream-fault callback was not registered');
    }
    globalThis.__instrumentControlsFault({ slot: 0, kind: 'output' });
  });
  const fallback = page.getByRole('button', {
    name: 'Input live, monitoring through the web path; click to stop',
    exact: true,
  });
  await fallback.waitFor();
  assert.equal((await fallback.textContent())?.trim(), 'INPUT LIVE · WEB MONITOR');
  assert.equal(
    await fallback.getAttribute('title'),
    'native monitor lost; go live again to restore low-latency monitoring',
  );
  assert.equal(await fallback.locator('.tgl__dot').count(), 1, 'the live dot must remain visible on fallback');

  assert.deepEqual(pageErrors, []);
  console.log('=== RESULT: instrument-controls passed ===');
} finally {
  await browser.close();
}
