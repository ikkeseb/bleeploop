// Browser repro for plugin-load buffer identity. It drives the production selectPlugin and
// plugin-bridge modules with an instrumented host; it makes no native timing or COM claim.
import assert from 'node:assert/strict';
import { chromium } from 'playwright';

const browser = await chromium.launch({ args: ['--autoplay-policy=no-user-gesture-required'] });
try {
  const page = await browser.newPage();
  await page.goto(process.argv.find((arg) => arg.startsWith('--url='))?.slice(6) ?? 'http://localhost:1420');
  await page.waitForFunction(() => !!window.__lf);

  const result = await page.evaluate(async () => {
    const { platform } = await import('/src/platform/index.ts');
    const instrument = await import('/src/audio/instrument.ts');
    const slots = await import('/src/audio/instrument-slots.ts');
    const { pluginBridge } = await import('/src/audio/plugin-bridge.ts');

    const released = [];
    await pluginBridge.init(window.__lf.engine.ctx, { release: (ab) => released.push(ab) });
    platform.pluginHost.available = true;
    slots.setNativeHostReady(true);
    platform.pluginHost.unloadPlugin = async () => {};

    const desc = (id) => ({
      id,
      name: `Probe ${id}`,
      path: `C:\\probe\\${id}.vst3`,
      format: 'vst3',
      isEffect: false,
    });
    const baseMeta = { kind: 'plugin-audio', slot: 0, capacityFrames: 1024, headerBytes: 28,
      sampleRate: window.__lf.engine.ctx.sampleRate, inChannels: 0 };
    const meta = (loadToken) => ({ ...baseMeta, loadToken });
    const buffer = (marker) => {
      const ab = new ArrayBuffer(baseMeta.headerBytes + baseMeta.capacityFrames * Float32Array.BYTES_PER_ELEMENT);
      const header = new Uint32Array(ab, 0, baseMeta.headerBytes / Uint32Array.BYTES_PER_ELEMENT);
      header[0] = marker;
      header[2] = baseMeta.capacityFrames;
      return ab;
    };
    const waitFor = async (read) => {
      for (let i = 0; i < 100 && !read(); i++) await new Promise((resolve) => setTimeout(resolve, 0));
      if (!read()) throw new Error('instrumented load was not called');
    };

    // Case 1: the command reports failure, then its already-posted buffer arrives. A failed load
    // owns no bridge slot, so this buffer must be released rather than wired.
    let rejectLoad;
    let failedToken;
    platform.pluginHost.loadPlugin = (_slot, _path, _id, loadToken) => {
      failedToken = loadToken;
      return new Promise((_, reject) => { rejectLoad = reject; });
    };
    const failedPick = instrument.selectPlugin(0, desc('failed-a'));
    await waitFor(() => rejectLoad);
    rejectLoad(new Error('injected native load timeout'));
    await failedPick;
    const orphan = buffer(101);
    await pluginBridge.acceptPluginBuffer(orphan, meta(failedToken));
    const afterFailedLoad = {
      marker: pluginBridge.stats(0)?.hop1Lag ?? null,
      orphanReleased: released.includes(orphan),
      slotPlugin: instrument.slotPlugins()[0]?.id ?? null,
    };
    pluginBridge.teardownPluginSlot(0);

    // Case 2: retry B starts in the same slot before stale A arrives. A and B have the same slot
    // and frontend epoch in production; without a source-load generation, stale A can claim B's
    // JS-local generation and make the correct B buffer fail the bridge's post-await check.
    let rejectA;
    let resolveB;
    let tokenA;
    let tokenB;
    platform.pluginHost.loadPlugin = (_slot, _path, id, loadToken) => {
      if (id === 'retry-a') {
        tokenA = loadToken;
        return new Promise((_, reject) => { rejectA = reject; });
      }
      if (id === 'retry-b') {
        tokenB = loadToken;
        return new Promise((resolve) => { resolveB = resolve; });
      }
      throw new Error(`unexpected plugin ${id}`);
    };
    const pickA = instrument.selectPlugin(0, desc('retry-a'));
    await waitFor(() => rejectA);
    rejectA(new Error('injected native load timeout'));
    await pickA;

    const pickB = instrument.selectPlugin(0, desc('retry-b'));
    await waitFor(() => resolveB);
    const staleA = buffer(111);
    const validB = buffer(222);
    await pluginBridge.acceptPluginBuffer(staleA, meta(tokenA));
    await pluginBridge.acceptPluginBuffer(validB, meta(tokenB));
    resolveB({ slot: 0, descriptor: desc('retry-b') });
    await pickB;
    const afterRetry = {
      marker: pluginBridge.stats(0)?.hop1Lag ?? null,
      staleReleased: released.includes(staleA),
      validReleased: released.includes(validB),
      slotPlugin: instrument.slotPlugins()[0]?.id ?? null,
    };

    await instrument.clearPlugin(0);
    return { afterFailedLoad, afterRetry };
  });

  console.log(JSON.stringify(result));
  assert.deepEqual(
    result,
    {
      afterFailedLoad: { marker: null, orphanReleased: true, slotPlugin: null },
      afterRetry: { marker: 222, staleReleased: true, validReleased: false, slotPlugin: 'retry-b' },
    },
    'failed-load buffers must be released, and a retry must keep the buffer from its own source load',
  );

  const retryPage = await browser.newPage();
  await retryPage.goto(process.argv.find((arg) => arg.startsWith('--url='))?.slice(6) ?? 'http://localhost:1420');
  await retryPage.waitForFunction(() => !!window.__lf);
  const moduleRetry = await retryPage.evaluate(async () => {
    const { pluginBridge } = await import('/src/audio/plugin-bridge.ts');
    const released = [];
    const audioWorklet = window.__lf.engine.ctx.audioWorklet;
    const addModule = audioWorklet.addModule.bind(audioWorklet);
    let calls = 0;
    Object.defineProperty(audioWorklet, 'addModule', {
      configurable: true,
      value: async (...args) => {
        calls++;
        if (calls === 1) throw new Error('injected worklet module failure');
        return addModule(...args);
      },
    });
    try {
      await pluginBridge.init(window.__lf.engine.ctx, { release: (ab) => released.push(ab) });
    } catch {
      // The next buffer acceptance must retry the failed cached module promise.
    }
    const loadToken = pluginBridge.beginPluginLoad(0, false);
    const capacityFrames = 1024;
    const headerBytes = 28;
    const ab = new ArrayBuffer(headerBytes + capacityFrames * Float32Array.BYTES_PER_ELEMENT);
    const header = new Uint32Array(ab, 0, headerBytes / Uint32Array.BYTES_PER_ELEMENT);
    header[0] = 333;
    header[2] = capacityFrames;
    await pluginBridge.acceptPluginBuffer(ab, {
      kind: 'plugin-audio', slot: 0, capacityFrames, headerBytes,
      sampleRate: window.__lf.engine.ctx.sampleRate, inChannels: 0, loadToken,
    });
    const result = { calls, marker: pluginBridge.stats(0)?.hop1Lag ?? null, released: released.includes(ab) };
    pluginBridge.teardownPluginSlot(0);
    return result;
  });
  console.log(JSON.stringify({ moduleRetry }));
  assert.deepEqual(
    moduleRetry,
    { calls: 2, marker: 333, released: false },
    'a failed AudioWorklet module load must be retryable without leaking the next received buffer',
  );
} finally {
  await browser.close();
}
