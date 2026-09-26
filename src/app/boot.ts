import { engine } from '../audio/engine';
import { initAudioDeviceSettings, refreshAndPruneDevices } from '../audio/audio-devices';
import { availablePlugins, restorePlugin, resyncNativeSlots, scanForPlugins } from '../audio/instrument';
import { recallRig } from '../audio/rig-recall';
import { setNativeHostReady } from '../audio/instrument-slots';
import { pluginBridge, type PluginBufferMeta } from '../audio/plugin-bridge';
import { warm as warmCapture } from '../audio/looper/capture';
import { notifyError } from '../notify';
import { engineMode, platform, registerPluginBufferSink, releasePluginBuffer } from '../platform';
import { CONFIRM_WINDOW_MS } from '../ui/looper/shared';
import { refusalText, refuseOnLane } from '../ui/looper/gates';
import { onRefused, openEngineDevice, startEngineStore } from '../ui/state/engine-store';

/**
 * Native plugin-host boot chain (Tauri/WebView2 only — `available` is false in the browser build, so
 * this is a no-op there and the UI surfaces only the six built-in synths). Prepare the audio bridge
 * (adopt the shared ctx + load the source worklet), register the WebView2 SharedBuffer sink
 * (→ plugin-bridge), tell Rust the engine sample rate, then scan installed CLAP/VST3 plugins so the
 * slot picker can list them, and reload each slot's plugin from the last run (`rig-recall.ts`). The
 * picker drives load/editor/param from there.
 *
 * Returns a dispose fn (removes the first-gesture resume listener if it never fired). In engine mode
 * it runs `bootEngine` instead.
 */
export function bootPluginHost(): () => void {
  if (engineMode()) return bootEngine();
  if (!platform.pluginHost.available) return () => {};
  setNativeHostReady(false);
  void (async () => {
    try {
      await pluginBridge.init(engine.ctx, {
        release: releasePluginBuffer,
        onPluginConnected: warmCapture,
      });
      registerPluginBufferSink(
        (ab, meta) =>
          void pluginBridge
            .acceptPluginBuffer(ab, meta as PluginBufferMeta)
            // The sink is fire-and-forget (the WebView2 sharedbufferreceived handler's sync
            // try/catch can't see this async rejection). Catch it so a post-await wiring failure
            // is logged, not an unhandled rejection. acceptPluginBuffer releases the buffer itself
            // on failure.
            .catch((err) => console.error('[app] acceptPluginBuffer failed', err)),
      );
      await platform.pluginHost.init(engine.ctx.sampleRate);
      // Frontend-reload wedge fix: unload any native slot the host still holds after a WebView
      // reload/crash-recovery (frontend reset to defaults, native slots stranded), before the
      // user can load. Idempotent + cheap on a clean cold start (host reports nothing loaded).
      await resyncNativeSlots();
      await initAudioDeviceSettings();
      // Enumerate native devices + prune any stale persisted device id up front, so the first Arm
      // uses a valid device (or the default) without waiting for the settings popover to open.
      await refreshAndPruneDevices();
      // Publish selectable plugins only after driver, buffer and device preferences are ready.
      setNativeHostReady(true);
      await scanForPlugins();
      // Rig recall: each slot's last plugin comes back through the normal load path, never armed.
      await recallRig(availablePlugins(), restorePlugin);
    } catch (e) {
      // Never let a host-init failure become an unhandled rejection on startup; the picker just
      // stays empty (chip reads "0 found"). The built-in synths remain fully playable.
      console.error('[app] plugin host init failed', e);
      notifyError('Plugin host failed to start', e);
    }
  })();
  // Resume the suspended AudioContext on the first user gesture so a loaded plugin becomes
  // audible without first having to play a built-in synth.
  const resume = () => {
    void engine.start();
    window.removeEventListener('pointerdown', resume);
  };
  window.addEventListener('pointerdown', resume);
  return () => window.removeEventListener('pointerdown', resume);
}

/**
 * Engine mode's boot chain: subscribe to the engine's feed (which sends the saved settings), put an
 * engine refusal on its lane, start the ASIO driver when it is the saved choice, open the saved device,
 * then the plugin host as in the web chain, minus the SharedBuffer bridge and the Web Audio context.
 * The plugin host activates plugins at the device's rate. Returns the dispose fn.
 */
function bootEngine(): () => void {
  const stopFeed = startEngineStore();
  const stopRefusals = onRefused((lane, reason) =>
    refuseOnLane(lane, refusalText(reason), reason === 'ConfirmClear' ? CONFIRM_WINDOW_MS : undefined),
  );
  if (platform.pluginHost.available) setNativeHostReady(false);
  void (async () => {
    try {
      if (platform.pluginHost.available) {
        await initAudioDeviceSettings();
        await refreshAndPruneDevices();
      }
      const status = await openEngineDevice();
      if (!platform.pluginHost.available) return;
      // With no device open the host hears 48 kHz; the engine's slot owners decide what that means.
      await platform.pluginHost.init(status?.sampleRate ?? 48000);
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
    stopFeed();
    stopRefusals();
  };
}
