/**
 * Help's "About this build" (`src/ui/settings/diagnostics.ts`): the version + commit line, Copy
 * diagnostics and Open log folder, driven through the rendered popover with clipboard permissions
 * granted. Three pages, each a fresh app:
 *
 * - browser build: the line reads package.json's version and the commit Vite built from (GITHUB_SHA
 *   in CI, else git); Open log folder is absent; Copy diagnostics puts the build, the web path, both
 *   slots' synths and the user agent on the clipboard, no log line, and toasts "Diagnostics copied";
 * - a log folder, an ASIO build and a plugin standing in for the Windows app (`platform.logs`, the ASIO
 *   status and the slot state replaced in the page before Help opens): the ASIO licence lines render,
 *   Open log folder shows and calls the host, and the copied block names the ASIO status, the plugin
 *   with its format (never its path) and the folder;
 * - engine mode on the web engine fake (`window.__lfEngineFake`): the block reads the device that runs.
 *
 * Cannot see the Rust commands, Explorer, WebView2's clipboard or a real device: every native answer
 * here is a stand-in. Run: pnpm probe diagnostics [--shots=<dir>]
 */
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { arg, probe } from '../harness/probe.ts';

const shots = arg('shots');
const version = JSON.parse(readFileSync(new URL('../../package.json', import.meta.url), 'utf8')).version;
// The same answer vite.config.ts bakes in; the probe's Vite runs from this checkout.
const commit = process.env.GITHUB_SHA
  ? process.env.GITHUB_SHA.slice(0, 7)
  : execFileSync('git', ['rev-parse', '--short', 'HEAD'], { encoding: 'utf8' }).trim();
const LOG_DIR = '%LOCALAPPDATA%\\com.bleeploop.app\\logs';

await probe(async ({ browser, open }) => {
  const context = await browser.newContext({ permissions: ['clipboard-read', 'clipboard-write'] });

  /** Open Help, bring About this build into view and return its section. */
  const openAbout = async (page) => {
    await page.getByRole('button', { name: 'Keyboard & layout help' }).click();
    const about = page.locator('#lf-help-popover .help__sec', { has: page.getByRole('heading', { name: 'About this build' }) });
    await about.scrollIntoViewIfNeeded();
    return about;
  };
  /** Click Copy diagnostics, wait for its toast, and return what landed on the clipboard as lines (the
   * Windows clipboard hands the block back with CRLF). */
  const copy = async (page, about) => {
    await page.evaluate(() => navigator.clipboard.writeText(''));
    await about.getByRole('button', { name: 'Copy diagnostics' }).click();
    await page.locator('.toast', { hasText: 'Diagnostics copied' }).waitFor({ timeout: 5000 });
    const text = await page.evaluate(() => navigator.clipboard.readText());
    console.log(`copied:\n${text}`);
    return text.split(/\r?\n/);
  };

  // 1. The browser build.
  {
    const { page, consoleErrors } = await open({ context, viewport: { width: 1280, height: 820 } });
    const about = await openAbout(page);
    const label = `BleepLoop ${version} · ${commit}`;
    assert.equal((await about.locator('.help__about').first().textContent())?.trim(), label, 'the version line');
    assert.equal(await about.getByRole('button', { name: 'Open log folder' }).count(), 0, 'Open log folder in the browser build');
    if (shots) await page.screenshot({ path: `${shots}/diagnostics-about-web.png` });
    const lines = await copy(page, about);
    const userAgent = await page.evaluate(() => navigator.userAgent);
    assert.equal(lines[0], `${label} (dev build)`, 'the build line');
    for (const line of ['App: browser build', 'Audio: web path (browser build)', 'Slot A: built-in synth (lead)', 'Slot B: built-in synth (bass)', `OS/WebView: ${userAgent}`]) {
      assert.ok(lines.includes(line), `missing "${line}"`);
    }
    assert.ok(lines.some((l) => /^Sample rate: \d+ Hz$/.test(l)), 'the sample rate');
    assert.ok(!lines.some((l) => l.startsWith('Log folder')), 'a log line without a log file');
    assert.deepEqual(consoleErrors, [], 'console errors');
    await page.close();
  }

  // 2. A log folder and a loaded plugin, as the Windows app has them.
  {
    const { page, consoleErrors } = await open({ context, viewport: { width: 1280, height: 820 } });
    await page.evaluate(async (dir) => {
      const { platform } = await import('/src/platform/index.ts');
      const { setSlotPlugins } = await import('/src/audio/instrument-slots.ts');
      const { probeAsio } = await import('/src/audio/audio-devices.ts');
      window.__logOpens = 0;
      platform.logs = { available: true, path: async () => dir, open: async () => void window.__logOpens++ };
      setSlotPlugins([{ id: 'amp', name: 'Probe Amp', format: 'vst3', path: 'C:\\probe\\amp.vst3', isEffect: true }, null]);
      // An ASIO build launched with --disable-asio: the licence lines render, the driver stays untouched.
      platform.pluginHost.asioProbe = async () => ({ status: 'disabled-by-flag', detail: '' });
      await probeAsio(true);
    }, LOG_DIR);
    const about = await openAbout(page);
    assert.equal(await about.getByRole('img', { name: 'ASIO Compatible' }).count(), 1, 'the ASIO logo in an ASIO build');
    await about.getByRole('button', { name: 'Open log folder' }).click();
    assert.equal(await page.evaluate(() => window.__logOpens), 1, 'Open log folder did not call the host');
    if (shots) await page.screenshot({ path: `${shots}/diagnostics-about-app.png` });
    const lines = await copy(page, about);
    assert.ok(lines.includes('ASIO status: disabled-by-flag'), 'the ASIO status line');
    assert.ok(lines.includes('Slot A: Probe Amp (VST3)'), 'the plugin line');
    assert.equal(lines.at(-1), `Log folder: ${LOG_DIR}`, 'the log line');
    assert.ok(!lines.some((l) => l.includes('amp.vst3')), 'a plugin path in the block');
    assert.deepEqual(consoleErrors, [], 'console errors');
    await page.close();
  }

  // 3. Engine mode: the device that runs.
  {
    const { page, consoleErrors } = await open({
      context,
      viewport: { width: 1280, height: 820 },
      init: (p) => p.addInitScript(() => { window.__lfEngineFake = true; }),
    });
    await page.waitForFunction(() => window.__lf.native.opened.length > 0, undefined, { timeout: 10_000 });
    const about = await openAbout(page);
    const lines = await copy(page, about);
    for (const line of ['Audio: native engine', 'Backend: WASAPI', 'Device: Fake input → Fake output', 'Sample rate: 48000 Hz']) {
      assert.ok(lines.includes(line), `missing "${line}"`);
    }
    assert.ok(lines.some((l) => /^Buffer: \d+ frames$/.test(l)), 'the buffer');
    assert.deepEqual(consoleErrors, [], 'console errors');
    await page.close();
  }

  await context.close();
});
