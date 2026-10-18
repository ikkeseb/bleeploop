/**
 * Two device picks queued on the open queue (`src/ui/state/engine-store.ts` `openEngineDevice`), the
 * first of which fails, on the web engine fake (`src/platform/host.web.ts`) with its `open` wrapped: the
 * output device B's open is held, then refused; C is picked while it is held.
 *
 * - B's failure does not put A's picks back over C: the queued open asks for C, and C stays saved;
 * - a lone failed pick still puts the picks of the device that runs back (A, saved).
 *
 * Cannot see the native device owner (its reopen of the device that ran: `engine_io/owner.rs` tests),
 * cpal or Audio Settings' selects (`pnpm probe sample-rate` drives those).
 * Run: pnpm probe device-queue
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

await probe(async ({ open }) => {
  const { page, consumeConsoleErrors } = await open({ init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)) });
  await page.waitForFunction(() => window.__lf.native.opened.length >= 1, undefined, { timeout: 10000 });
  const result = await page.evaluate(async () => {
    const { platform } = await import('/src/platform/index.ts');
    const store = await import('/src/ui/state/engine-store.ts');
    const settings = await import('/src/ui/state/audio-settings.ts');
    const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
    const asked = [];
    let releaseB = null;
    const openFake = platform.engine.open.bind(platform.engine);
    platform.engine.open = async (request, force) => {
      asked.push(request.output);
      if (request.output === 'out-b') {
        await new Promise((resolve) => (releaseB = resolve));
        throw new Error('probe: device B would not open');
      }
      return openFake(request, force);
    };
    settings.writeAudioDeviceSettings({ outputDeviceId: 'out-a' });
    await store.openEngineDevice();

    // B is picked and its open held; C is picked meanwhile; B fails.
    asked.length = 0;
    settings.writeAudioDeviceSettings({ outputDeviceId: 'out-b' });
    const first = store.openEngineDevice();
    while (!releaseB) await pause(5);
    settings.writeAudioDeviceSettings({ outputDeviceId: 'out-c' });
    const second = store.openEngineDevice();
    releaseB();
    await first;
    await second;
    const queued = { asked: asked.slice(), saved: settings.readAudioDeviceSettings().outputDeviceId };

    // A lone failed pick: the picks of the device that runs (C) go back.
    asked.length = 0;
    releaseB = null;
    settings.writeAudioDeviceSettings({ outputDeviceId: 'out-b' });
    const lone = store.openEngineDevice();
    while (!releaseB) await pause(5);
    releaseB();
    await lone;
    return { queued, lone: { asked: asked.slice(), saved: settings.readAudioDeviceSettings().outputDeviceId } };
  });
  console.log(JSON.stringify(result));
  assert.equal(consumeConsoleErrors('[engine] device open failed').length, 2, 'both failed opens reach the release log');
  assert.deepEqual(result.queued, { asked: ['out-b', 'out-c'], saved: 'out-c' }, "a failed open does not put the device that ran back over a later pick");
  assert.deepEqual(result.lone, { asked: ['out-b'], saved: 'out-c' }, 'a lone failed open puts the picks of the device that runs back');
});
