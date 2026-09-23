import { engine } from '../audio/engine';
import { initAudioDeviceSettings, refreshAndPruneDevices } from '../audio/audio-devices';
import { availablePlugins, resyncNativeSlots, scanForPlugins, selectPlugin } from '../audio/instrument';
import { recallRig } from '../audio/rig-recall';
import { setNativeHostReady } from '../audio/instrument-slots';
import { pluginBridge, type PluginBufferMeta } from '../audio/plugin-bridge';
import { warm as warmCapture } from '../audio/looper/capture';
import { notifyError } from '../notify';
import { platform, registerPluginBufferSink, releasePluginBuffer } from '../platform';

/**
 * Native plugin-host boot chain (Tauri/WebView2 only — `available` is false in the browser build, so
 * this is a no-op there and the UI surfaces only the six built-in synths). Prepare the audio bridge
 * (adopt the shared ctx + load the source worklet), register the WebView2 SharedBuffer sink
 * (→ plugin-bridge), tell Rust the engine sample rate, then scan installed CLAP/VST3 plugins so the
 * slot picker can list them, and reload each slot's plugin from the last run (`rig-recall.ts`). The
 * picker drives load/editor/param from there.
 *
 * Returns a dispose fn (removes the first-gesture resume listener if it never fired).
 */
export function bootPluginHost(): () => void {
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
      await recallRig(availablePlugins(), selectPlugin);
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
