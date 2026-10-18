import { initAudioDeviceSettings, refreshAndPruneDevices } from '../ui/state/audio-devices';
import { availablePlugins, restorePlugin, resyncNativeSlots, scanForPlugins } from '../ui/state/instrument';
import { recallRig, savedRigPaths } from '../ui/state/rig-recall';
import { setNativeHostReady } from '../ui/state/instrument-slots';
import { notifyError } from '../notify';
import { platform } from '../platform';
import { announceLooper, refusalText, refuseOnLane } from '../ui/looper/gates';
import { onEngineEvent, openEngineDevice, restoreEngineShare, startEngineStore, whenDevice } from '../ui/state/engine-store';
import { session } from '../ui/state/audio';
import { autosave } from '../session/autosave';

/**
 * The boot chain: subscribe to the engine's feed (its reset frame brings the engine's settings), put an
 * engine refusal on its lane, start the ASIO driver when it is the saved choice and open the saved
 * device. Everything that needs a running engine waits for the first device: Share output, local
 * recovery (its restore loads into an engine at the device's rate) and the plugin host, activated at
 * the device's rate: it resyncs the slots a WebView reload stranded, reloads each slot's plugin from the
 * last run (`rig-recall.ts`) from a scan of those plugins' own paths, and meanwhile scans every
 * installed CLAP/VST3/VST2 plugin for the slot picker, whose new or updated bundles may each take a scan
 * child up to 20 s that the saved amp-sim no longer waits for. So a
 * launch whose device does not open never recalls the rig, and cannot forget it on the loads that
 * would fail. The browser build has no engine (unless a DEV probe forces the fake on): it boots nothing
 * and stays silent. Returns the dispose fn.
 */
export function bootEngine(): () => void {
  if (!platform.engine.available) return () => {};
  const stopFeed = startEngineStore();
  const stopRefusals = onEngineEvent((ev) => {
    if (ev.type !== 'Refused') return;
    // CLEAR's first press is no refusal on the lane, which shows the engine's arm itself
    // (`LaneInfo.clearArmed`): the screen reader hears it.
    if (ev.reason === 'ConfirmClear') announceLooper(`Track ${ev.lane + 1}: ${refusalText(ev.reason)}`);
    else refuseOnLane(ev.lane, refusalText(ev.reason));
  });
  let stopAutosave: (() => void) | null = null;
  let disposed = false;
  if (platform.pluginHost.available) setNativeHostReady(false);
  void (async () => {
    try {
      if (platform.pluginHost.available) {
        await initAudioDeviceSettings();
        await refreshAndPruneDevices();
      }
      await openEngineDevice();
      const status = await whenDevice();
      if (disposed) return;
      restoreEngineShare();
      stopAutosave = autosave.start(session);
      if (!platform.pluginHost.available) return;
      await platform.pluginHost.init(status.sampleRate);
      await resyncNativeSlots();
      setNativeHostReady(true);
      const paths = savedRigPaths();
      const rig = paths.length === 0 ? [] : await platform.pluginHost.scanPluginPaths(paths).catch((e: unknown) => {
        // The recall waits for the full scan instead.
        console.error('[app] rig plugin scan failed', e);
        return null;
      });
      const recalled = rig && recallRig(rig, restorePlugin);
      await scanForPlugins();
      await (recalled ?? recallRig(availablePlugins(), restorePlugin));
    } catch (e) {
      console.error('[app] engine boot failed', e);
      notifyError('Could not start the audio engine. Restart BleepLoop.', e);
    }
  })();
  return () => {
    disposed = true;
    stopAutosave?.();
    stopFeed();
    stopRefusals();
  };
}
