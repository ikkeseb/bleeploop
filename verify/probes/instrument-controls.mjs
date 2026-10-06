/**
 * Rendered regression coverage for keyboard state, plugin controls and MIDI startup status, on the web
 * engine fake (`src/platform/host.web.ts`, the engine-seam pattern) with the native slot chrome on
 * (host.web.ts served with `available: true`): a denied Web MIDI request logs and lands in `denied`,
 * concurrent retries share one request, both empty native slots show the install hint, the keyboard
 * octave survives a placement remount, and the slot volume (a plugin's output gain) honors the unity
 * detent under keyboard steps and slider input. The plugin host and MIDI access are simulated in the
 * page; no native or hardware claim. Run: pnpm probe instrument-controls
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

await probe(async ({ open }) => {
  // The harness's own consoleErrors only keeps `console.error` text; the denied-MIDI notice logs
  // through `console.warn`, so this probe keeps its own listener across both types.
  const consoleMessages = [];
  const { page } = await open({
    viewport: { width: 1280, height: 820 },
    init: async (p) => {
      p.on('console', (message) => {
        if (message.type() === 'error' || message.type() === 'warning') consoleMessages.push(message.text());
      });
      await p.addInitScript(() => {
        Object.defineProperty(navigator, 'requestMIDIAccess', {
          configurable: true,
          value: () => Promise.reject(new DOMException('nope', 'SecurityError')),
        });
      });
      await p.addInitScript(() => void (window.__lfEngineFake = true));
      await p.route('**/src/platform/host.web.ts*', async (route) => {
        const response = await route.fetch();
        const body = await response.text();
        if (!body.includes('available: false')) throw new Error('Could not enable simulated native chrome');
        await route.fulfill({ response, body: body.replace('available: false', 'available: true') });
      });
    },
  });

  await page.waitForFunction(() => window.__lf.midi.midiStatus() === 'denied');
  await page.waitForFunction(
    () => document.querySelector('[aria-label="Rescan plugins"]')?.disabled === false,
  );

  assert.equal(await page.evaluate(() => window.__lf.midi.midiStatus()), 'denied');
  assert.ok(
    consoleMessages.some((message) => message.includes('[midi] requestMIDIAccess denied')),
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
    'No plugins found · the standard CLAP and VST3 folders are scanned · add your own in Audio Settings · rescan ⟳ in the command bar';
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

  const paramsButton = page.getByRole('button', { name: /Plugin parameters for slot 1/ });
  await paramsButton.click();
  const output = page.getByRole('slider', { name: 'Volume for slot 1', exact: true });
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
});
