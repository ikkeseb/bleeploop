import { hasDeviceSettings, initAudioDeviceSettings, refreshAndPruneDevices } from '../ui/state/audio-devices';
import {
  availablePlugins,
  legacyRigSnapshot,
  restorePlugin,
  resyncNativeSlots,
  scanForPlugins,
  showRigLevels,
} from '../ui/state/instrument';
import { importRig, openRig, recallRig, savedRigPaths, type SavedRig } from '../ui/state/rig-recall';
import { boundKinds } from '../ui/state/rig-record';
import { bindingsKnown } from '../ui/state/midi';
import { nativeHostReady, setNativeHostReady } from '../ui/state/instrument-slots';
import { notifyError } from '../notify';
import { jackHost, mintUnitId, platform, type PluginDescriptor } from '../platform';
import { announceLooper, refusalText, refuseOnLane } from '../ui/looper/gates';
import {
  engineDevice,
  holdDeviceOpens,
  onEngineEvent,
  openEngineDevice,
  restoreEngineShare,
  startEngineStore,
  storedInputSends,
  whenDevice,
} from '../ui/state/engine-store';
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
 * child up to 20 s that the saved amp-sim no longer waits for. In chain mode (`platform.chain.available`)
 * the rig record, its restore and its crash marker are the native host's (`platform.rig.available`, every
 * native chain-mode page): the page opens the record first, and no device opens before that open settled
 * (`openRig`, `holdDeviceOpens`; a failure never stops the audio boot), and only when native has none, once
 * the device and the plugin host run (if there is one), scans the legacy recall's plugins, upgrades the old
 * keys and imports the result once while the device still runs (`importRig`). So a launch whose device does
 * not open never recalls the rig, and cannot forget it on the loads that would fail. The browser build has no engine (unless a DEV probe forces the fake on): it boots nothing
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
  // The native rig's open goes first, and no device opens before it settled: boot's, nor one Audio Settings
  // asks for meanwhile (`holdDeviceOpens`; SEAT-B). It never rejects (a refusal or a timeout is null).
  const rigNative = platform.chain.available && platform.rig.available;
  const opening = rigNative ? openRig({ levels: showRigLevels }) : Promise.resolve(null);
  if (rigNative) holdDeviceOpens(opening);
  void (async () => {
    try {
      if (hasDeviceSettings()) {
        await initAudioDeviceSettings();
        await refreshAndPruneDevices();
      }
      const opened = await opening;
      await openEngineDevice();
      await whenDevice();
      if (disposed) return;
      if (!jackHost) restoreEngineShare();
      stopAutosave = autosave.start(session);
      if (platform.chain.available) {
        // The native record's handover instead of legacy `recallRig`, on every chain-mode launch: a SYNTH or
        // an empty rig needs no plugin host. With one, the full scan fills the `+` picker meanwhile and the
        // upgrade scans the legacy plugins' own paths; without one, a plugin unit goes over unscanned.
        const host = platform.pluginHost.available;
        if (host) {
          await platform.pluginHost.init();
          await resyncNativeSlots();
          setNativeHostReady(true);
        }
        const full = host ? scanForPlugins() : Promise.resolve();
        const scan = (paths: string[]) =>
          !host || paths.length === 0
            ? Promise.resolve([])
            : platform.pluginHost.scanPluginPaths(paths).catch((e: unknown) => {
                // The restore waits for the full scan instead.
                console.error('[app] rig plugin scan failed', e);
                return full.then(() => availablePlugins());
              });
        const legacy = async (found: readonly PluginDescriptor[], saved: SavedRig) => ({
          ...legacyRigSnapshot(found, saved),
          sends: storedInputSends(),
          bound: boundKinds(await bindingsKnown()),
        });
        const ready = () => engineDevice() !== null && (!host || nativeHostReady());
        const handedOver = importRig(opened, { levels: showRigLevels, scan, snapshot: legacy, mint: mintUnitId, ready });
        await full;
        await handedOver;
        return;
      }
      if (!platform.pluginHost.available) return;
      await platform.pluginHost.init();
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
