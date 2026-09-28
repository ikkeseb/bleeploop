import { initAudioDeviceSettings, refreshAndPruneDevices } from '../ui/state/audio-devices';
import { availablePlugins, restorePlugin, resyncNativeSlots, scanForPlugins } from '../ui/state/instrument';
import { recallRig } from '../ui/state/rig-recall';
import { setNativeHostReady } from '../ui/state/instrument-slots';
import { notifyError } from '../notify';
import { platform } from '../platform';
import { CONFIRM_WINDOW_MS } from '../ui/looper/shared';
import { refusalText, refuseOnLane } from '../ui/looper/gates';
import { onEngineEvent, openEngineDevice, restoreEngineShare, startEngineStore, whenDevice } from '../ui/state/engine-store';
import { session } from '../ui/state/audio';
import { autosave } from '../session/autosave';

/**
 * The boot chain: subscribe to the engine's feed (its reset frame brings the engine's settings), put an
 * engine refusal on its lane, start the ASIO driver when it is the saved choice and open the saved
 * device. Everything that needs a running engine waits for the first device: Share output, local
 * recovery (its restore loads into an engine at the device's rate) and the plugin host, activated at
 * the device's rate: it resyncs the slots a WebView reload stranded, scans the installed CLAP/VST3
 * plugins for the slot picker and reloads each slot's plugin from the last run (`rig-recall.ts`). So a
 * launch whose device does not open never recalls the rig, and cannot forget it on the loads that
 * would fail. The browser build has no engine (unless a DEV probe forces the fake on): it boots nothing
 * and stays silent. Returns the dispose fn.
 */
export function bootEngine(): () => void {
  if (!platform.engine.available) return () => {};
  const stopFeed = startEngineStore();
  const stopRefusals = onEngineEvent((ev) => {
    if (ev.type !== 'Refused') return;
    refuseOnLane(ev.lane, refusalText(ev.reason), ev.reason === 'ConfirmClear' ? CONFIRM_WINDOW_MS : undefined);
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
      await scanForPlugins();
      await recallRig(availablePlugins(), restorePlugin);
    } catch (e) {
      console.error('[app] engine boot failed', e);
      notifyError('The audio engine failed to start', e);
    }
  })();
  return () => {
    disposed = true;
    stopAutosave?.();
    stopFeed();
    stopRefusals();
  };
}
