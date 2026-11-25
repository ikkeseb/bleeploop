import { initAudioDeviceSettings, refreshAndPruneDevices } from '../ui/state/audio-devices';
import {
  adoptRigVolume,
  availablePlugins,
  legacyRigSnapshot,
  restorePlugin,
  resyncNativeSlots,
  scanForPlugins,
  showRigLevels,
} from '../ui/state/instrument';
import { importRig, openRig, recallChains, recallRig, savedRigPaths, type SavedRig } from '../ui/state/rig-recall';
import { boundKinds } from '../ui/state/rig-record';
import { bindingsKnown, firstPageOfLaunch } from '../ui/state/midi';
import { setNativeHostReady } from '../ui/state/instrument-slots';
import { notifyError } from '../notify';
import { mintUnitId, platform, type PluginDescriptor } from '../platform';
import { announceLooper, refusalText, refuseOnLane } from '../ui/looper/gates';
import {
  legacySendsOff,
  onEngineEvent,
  openEngineDevice,
  reportedSlotGains,
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
 * the recall is the rig record's (`recallChains`), with or without a plugin host: the upgrade of a v0.8.9
 * profile on its first run, then the restore of each chain unit the reset frame's `chains` lack, from the
 * same scan of their paths (the native host keeps its chains across a WebView reload, so a reload adds
 * nothing; native MIDI's epoch tells a reload from a new launch, `firstPageOfLaunch`). With the native rig
 * record (`platform.rig.available`, every native chain-mode page) none of that runs: the page opens the
 * record before the device (`openRig`; a failure never stops the audio boot), and only when native has none,
 * once the device and the plugin host run, scans the legacy recall's plugins, upgrades the old keys and
 * imports the result once (`importRig`); the restore is native's. So a
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
      const rigNative = platform.chain.available && platform.rig.available;
      const opened = rigNative ? await openRig({ levels: showRigLevels }) : null;
      await openEngineDevice();
      await whenDevice();
      if (disposed) return;
      restoreEngineShare();
      stopAutosave = autosave.start(session);
      if (platform.chain.available) {
        // The rig record's recall instead of legacy `recallRig`, on every chain-mode launch: a SYNTH or an
        // empty rig needs no plugin host. With one, the full scan fills the `+` picker meanwhile and the
        // restore (or the upgrade) scans its plugins' own paths; without one, a plugin unit is kept in the
        // record, not added. With the native record, its handover; the WebView's recall stays out.
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
        const restored = rigNative
          ? importRig(opened, { levels: showRigLevels, scan, snapshot: legacy, mint: mintUnitId })
          : recallChains({
              scan,
              snapshot: async (found, saved) => ({ ...(await legacy(found, saved)), reportedVolumes: reportedSlotGains() }),
              setVolume: adoptRigVolume,
              sendsOff: legacySendsOff,
              firstPage: firstPageOfLaunch,
              add: (slot, unit, spec, at) => platform.chain.add(slot, unit, spec, at),
              mint: mintUnitId,
            });
        await full;
        await restored;
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
