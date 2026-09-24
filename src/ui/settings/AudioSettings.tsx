import { For, Show, createMemo, createSignal, onCleanup, onMount } from 'solid-js';
import { pluginBridge } from '../../audio/plugin-bridge';
import { availablePlugins, scanning } from '../../audio/instrument';
import {
  asioAvailable,
  asioDeviceInfo,
  asioEnabled,
  asioOffered,
  asioRetryable,
  asioStatus,
  probeAsio,
  usingAsio,
  bufferFrames,
  inputDevices,
  outputDevices,
  refreshAndPruneDevices,
  setAsioEnabled,
  setBufferSize,
} from '../../audio/audio-devices';
import { inputArmed, monitorArmed } from '../../audio/native-io';
import { midiDevices, midiStatus } from '../../audio/midi';
import { ACTION_LABELS, type ActionId } from '../../app/actions';
import { bindings, cancelLearn, forget, learn, learning, type MidiBinding } from '../../app/midi-actions';
import { platform } from '../../platform';
import {
  BUFFER_FRAMES_OPTIONS,
  readAudioDeviceSettings,
  writeAudioDeviceSettings,
  type BufferFrames,
} from '../../audio/audio-settings';
import { engine } from '../../audio/engine';
import { offsetMs, RECORD_TRIM_MAX_MS, setOffsetMs } from '../../audio/record-latency';
import './audio-settings.css';

/**
 * Global Audio Settings popover: consolidates the native input device + channel, the monitor
 * (cpal-out) output device, the live RT buffer-size control, the ASIO low-latency tier toggle (when
 * an ASIO device is available) and the MIDI learn row — all GLOBAL last-used preferences. The per-slot
 * Arm toggles stay in PluginControls and read the device choice persisted here. Mounted inside a `<Show>` in app.tsx, so it
 * re-reads persisted state each time it opens (persisted localStorage is the source of truth; these
 * local signals mirror it). The sample-rate row is a disabled placeholder for increment C2.
 */

/** Processing-block duration, not an input-to-output latency estimate. */
function bufferMs(frames: number): string {
  return `~${((frames / engine.ctx.sampleRate) * 1000).toFixed(1)} ms/block`;
}

const ACTION_IDS = Object.keys(ACTION_LABELS) as ActionId[];

/** A learned message as the bindings list shows it: "CC 64 · ch 1". */
function midiSource(b: MidiBinding): string {
  return `${b.kind === 'cc' ? 'CC' : 'note'} ${b.number} · ch ${b.channel + 1}`;
}

export function AudioSettings() {
  // crossOriginIsolated is fixed for the page lifetime → read once (no signal needed).
  const isolated = self.crossOriginIsolated === true;
  const persisted = readAudioDeviceSettings();
  const [selectedDevice, setSelectedDevice] = createSignal(persisted.inputDeviceId);
  const [selectedChannel, setSelectedChannel] = createSignal(persisted.inputChannel);
  const [selectedOutput, setSelectedOutput] = createSignal(persisted.outputDeviceId);
  // The by-ear record-alignment trim (`lf.recordOffsetMs`, persisted). Not a reactive signal in
  // record-latency.ts, but this popover remounts on every open, so a local signal seeded from the live
  // value is enough to mirror it (same pattern as the persisted device settings above).
  const [recAlign, setRecAlign] = createSignal(offsetMs());
  // The action the MIDI learn row binds to. A learn still listening when the panel closes is cancelled,
  // so a later pedal press cannot bind out of sight.
  const [learnPick, setLearnPick] = createSignal<ActionId>(ACTION_IDS[0]);
  onCleanup(cancelLearn);

  // Channel count of the selected NAMED device (0 for "default input" — its id is '' so the channel
  // count is unknown). When ≥2 the channel selector appears; by design the explicit channel pick is
  // offered only for a named device (the default-device shortcut auto-picks Rust-side).
  const deviceChannels = createMemo(() => {
    if (usingAsio()) return asioDeviceInfo()?.inputChannels ?? 0;
    const id = selectedDevice();
    if (!id) return 0;
    return inputDevices().find((d) => d.id === id)?.channels ?? 0;
  });

  // A device/channel change only re-opens the cpal stream on the NEXT arm — while a slot is armed the
  // live stream keeps its current device. The selectors stay enabled (you can pre-pick for the next
  // arm); a hint flags that the change is deferred so it doesn't read as broken.
  const anyInputArmed = () => inputArmed().some(Boolean);
  const anyMonitorArmed = () => monitorArmed().some(Boolean);

  onMount(async () => {
    // Refresh the device lists + prune any persisted id no longer present (shared with the startup
    // prune so arming never sees a stale id), then re-sync the local signals from the pruned persisted
    // settings — catches a device unplugged since this popover last opened. No-op in the web build.
    await refreshAndPruneDevices();
    const s = readAudioDeviceSettings();
    setSelectedDevice(s.inputDeviceId);
    setSelectedChannel(s.inputChannel);
    setSelectedOutput(s.outputDeviceId);
  });

  // Bridge health readout (native only): 2 Hz poll of pluginBridge.stats per slot while open.
  const [bridgeHealth, setBridgeHealth] = createSignal('no plugin loaded');
  onMount(() => {
    if (!platform.pluginHost.available) return;
    const poll = () => {
      const parts: string[] = [];
      for (let slot = 0; slot < 2; slot++) {
        const st = pluginBridge.stats(slot);
        if (!st) continue;
        parts.push(`${slot === 0 ? 'A' : 'B'} queue ${st.queue} · under ${st.underruns} · drop ${st.dropped}`);
      }
      setBridgeHealth(parts.length ? parts.join('  |  ') : 'no plugin loaded');
    };
    poll();
    const timer = setInterval(poll, 500);
    onCleanup(() => clearInterval(timer));
  });

  return (
    <div class="audio-settings" role="group" aria-label="Audio settings">
      <div class="audio-settings__title">Audio settings</div>

      <div class="audio-settings__row">
        <span class="audio-settings__label">input</span>
        <select
          class="audio-settings__select"
          value={usingAsio() ? '' : selectedDevice()}
          disabled={usingAsio()}
          onChange={(e) => {
            const v = e.currentTarget.value;
            setSelectedDevice(v);
            setSelectedChannel(''); // channel index is device-specific → reset to auto on swap
            writeAudioDeviceSettings({ inputDeviceId: v, inputChannel: '' });
          }}
          aria-label="Audio input device"
        >
          <option value="">{usingAsio() ? asioDeviceInfo()?.name ?? 'Default ASIO driver' : 'System default'}</option>
          <For each={inputDevices()}>{(d) => <option value={d.id}>{d.name}</option>}</For>
        </select>
        <Show when={deviceChannels() >= 2}>
          <select
            class="audio-settings__select audio-settings__select--channel"
            value={selectedChannel()}
            onChange={(e) => {
              const v = e.currentTarget.value;
              setSelectedChannel(v);
              writeAudioDeviceSettings({ inputChannel: v });
            }}
            aria-label="Input channel"
          >
            <option value="">Auto</option>
            <For each={Array.from({ length: deviceChannels() }, (_, i) => i)}>
              {(i) => <option value={String(i)}>Ch {i + 1}</option>}
            </For>
          </select>
        </Show>
      </div>
      <Show when={anyInputArmed()}>
        <div class="audio-settings__hint" role="note">Takes effect on the next GO LIVE.</div>
      </Show>

      <div class="audio-settings__row">
        <span class="audio-settings__label">output</span>
        <select
          class="audio-settings__select"
          value={usingAsio() ? '' : selectedOutput()}
          disabled={usingAsio()}
          onChange={(e) => {
            const v = e.currentTarget.value;
            setSelectedOutput(v);
            writeAudioDeviceSettings({ outputDeviceId: v });
          }}
          aria-label="Monitor output device"
        >
          <option value="">{usingAsio() ? asioDeviceInfo()?.name ?? 'Default ASIO driver' : 'System default'}</option>
          <For each={outputDevices()}>{(d) => <option value={d.id}>{d.name}</option>}</For>
        </select>
      </div>
      <Show when={anyMonitorArmed()}>
        <div class="audio-settings__hint" role="note">Takes effect on the next GO LIVE.</div>
      </Show>
      <Show when={usingAsio()}>
        <div class="audio-settings__hint audio-settings__hint--info" role="note">ASIO drives both input and output. Turn ASIO off to pick Windows devices.</div>
      </Show>

      <div class="audio-settings__row">
        <span class="audio-settings__label">plugin buffer</span>
        <select
          class="audio-settings__select"
          value={String(bufferFrames())}
          onChange={(e) => {
            const v = Number(e.currentTarget.value) as BufferFrames;
            const select = e.currentTarget;
            void setBufferSize(v).then(() => { select.value = String(bufferFrames()); });
          }}
          aria-label="Buffer size in frames"
        >
          <For each={BUFFER_FRAMES_OPTIONS}>
            {(f) => <option value={String(f)}>{f} frames</option>}
          </For>
        </select>
        <span class="audio-settings__readout">{bufferMs(bufferFrames())}</span>
      </div>
      <div class="audio-settings__hint audio-settings__hint--info" role="note">The block plugins process in. The audio driver sets its own device buffer.</div>

      {/* Record-alignment trim — the RELEASE-BUILD surface for the by-ear record-latency offset; a shipped
          build has no other way to trim it (the DEV-only hook is `__lf.recordLatency.setOffsetMs`). It
          nudges the compensation C (record-latency.ts): POSITIVE ms shifts the recorded take EARLIER on
          the grid, negative later. Only affects natively-monitored recording (guitar through a native
          plugin monitor); synth/mic loops are untouched. This is a stopgap to be superseded by the L3
          loopback calibration wizard. PLACEMENT IS PROVISIONAL — eye-gated. */}
      <div
        class="audio-settings__row"
        title="Nudges recorded guitar alignment (native monitor only): positive ms lands the take earlier on the grid, negative later."
      >
        <span class="audio-settings__label">rec align</span>
        <input
          class="audio-settings__number"
          type="number"
          step="1"
          min={-RECORD_TRIM_MAX_MS}
          max={RECORD_TRIM_MAX_MS}
          value={recAlign()}
          onChange={(e) => {
            const n = Number(e.currentTarget.value);
            if (Number.isFinite(n)) {
              setOffsetMs(Math.round(n));
              const stored = offsetMs();
              setRecAlign(stored);
              // Signal equality makes the set above a no-op when clamping lands on the CURRENT value
              // (e.g. stored 250, typed 400) — force the DOM too so the input always shows what was stored.
              e.currentTarget.value = String(stored);
            }
          }}
          aria-label="Recording alignment in milliseconds"
        />
        <span class="audio-settings__readout">ms</span>
      </div>

      {/* The ASIO row shows the toggle whenever the binary can do ASIO; the driver itself is contacted
          only by the startup probe (saved preference on) or by turning the toggle on / Retry. The
          status line under it says why ASIO is not in use (`AsioStartupStatus` in host.ts). */}
      <div
        class="audio-settings__row"
        classList={{ 'audio-settings__row--disabled': !asioOffered() }}
      >
        <span class="audio-settings__label">ASIO®</span>
        <Show
          when={asioOffered()}
          fallback={
            <span class="audio-settings__soon">
              {asioStatus().status === 'disabled-by-flag' ? 'off for this launch (--disable-asio)' : 'unavailable'}
            </span>
          }
        >
          <label class="audio-settings__toggle">
            <input
              type="checkbox"
              checked={asioEnabled()}
              disabled={anyInputArmed() || anyMonitorArmed() || asioStatus().status === 'probing'}
              onChange={(e) => {
                const checkbox = e.currentTarget;
                void setAsioEnabled(checkbox.checked).then(() => { checkbox.checked = asioEnabled(); });
              }}
              aria-label="Use ASIO low-latency audio"
            />
            <span class="audio-settings__toggle-text">
              {!asioEnabled()
                ? 'WASAPI'
                : asioAvailable()
                  ? 'low-latency'
                  : asioStatus().status === 'probing'
                    ? 'starting driver…'
                    : 'WASAPI until the driver starts'}
            </span>
          </label>
        </Show>
      </div>
      <Show when={asioOffered() && asioEnabled() && !asioAvailable() && asioStatus().status !== 'probing'}>
        <div class="audio-settings__hint audio-settings__hint--asio" role="status">
          <span>
            {asioStatus().status === 'blocked'
              ? 'The previous ASIO start did not complete, so it was skipped this time.'
              : asioStatus().status === 'timed-out'
                ? asioStatus().detail
                : asioStatus().status === 'failed'
                  ? `ASIO could not start: ${asioStatus().detail}.`
                  : 'ASIO has not been started yet.'}
          </span>
          <Show when={asioRetryable()}>
            <button
              type="button"
              class="audio-settings__btn"
              onClick={() => void probeAsio(true)}
              aria-label="Retry starting the ASIO driver"
            >
              RETRY ASIO
            </button>
          </Show>
        </div>
      </Show>
      {/* Switching backend re-opens the cpal stream (an arm operation) and input/output arm separately,
          so the host is locked while anything is armed — preventing an input-ASIO / output-WASAPI split
          (or vice versa, which on a seizing driver would fail the second arm). Disarm to change it. */}
      <Show when={asioOffered() && (anyInputArmed() || anyMonitorArmed())}>
        <div class="audio-settings__hint" role="note">Take every slot off INPUT LIVE to switch between ASIO and WASAPI.</div>
      </Show>

      {/* Read-only: the AudioContext runs at the output device's rate; a rate selector is not built. */}
      <div class="audio-settings__row" title="BleepLoop runs at the audio device's sample rate.">
        <span class="audio-settings__label">sample rate</span>
        <span class="audio-settings__readout">{(engine.ctx.sampleRate / 1000).toFixed(1)} kHz</span>
        <span class="audio-settings__soon">set by the audio device</span>
      </div>

      {/* MIDI learn: pick an action, LEARN, and the next CC or note-on from any port runs it from then on
          (a second click or Esc cancels). A learned message never reaches the play path. The bindings,
          their persistence and the momentary/latching read live in `src/app/midi-actions.ts`. */}
      <div class="audio-settings__row" title="A MIDI footswitch or key runs this action; the track actions act on the selected track.">
        <span class="audio-settings__label">midi learn</span>
        <select
          class="audio-settings__select"
          value={learnPick()}
          disabled={learning() !== null}
          onChange={(e) => setLearnPick(e.currentTarget.value as ActionId)}
          aria-label="Action to learn"
        >
          <For each={ACTION_IDS}>{(id) => <option value={id}>{ACTION_LABELS[id]}</option>}</For>
        </select>
        <button
          type="button"
          class="audio-settings__btn"
          classList={{ 'audio-settings__btn--listening': learning() !== null }}
          aria-pressed={learning() !== null}
          disabled={learning() === null && midiStatus() !== 'connected'}
          onClick={() => (learning() === null ? learn(learnPick()) : cancelLearn())}
          aria-label="Learn a MIDI control for this action"
        >
          {learning() === null ? 'LEARN' : 'LISTENING'}
        </button>
      </div>
      <Show when={learning() !== null}>
        <div class="audio-settings__hint audio-settings__hint--info" role="status">Tap a pedal or key on a MIDI device. Esc cancels.</div>
      </Show>
      <Show when={bindings().length > 0}>
        <ul class="audio-settings__bindings" aria-label="MIDI bindings">
          <For each={bindings()}>
            {(b) => (
              <li class="audio-settings__binding" title={b.portName}>
                <span class="audio-settings__binding-action">{ACTION_LABELS[b.action]}</span>
                <span class="audio-settings__binding-src">
                  {midiSource(b)} · {b.momentary ? 'momentary' : 'latching'}
                </span>
                <button
                  type="button"
                  class="audio-settings__binding-clear"
                  onClick={() => forget(b)}
                  aria-label={`Forget ${ACTION_LABELS[b.action]} on ${midiSource(b)}, ${b.portName}`}
                >
                  ✕
                </button>
              </li>
            )}
          </For>
        </ul>
      </Show>

      {/* Diagnostics: the read-only host/isolated/plugin/midi status. The command-bar system lamp is
          their aggregate; this is the full read-out. The plugin-scan live region stays in the command
          bar (single announcer), so these rows are plain text — no role=status here. */}
      <div class="audio-settings__diag" role="group" aria-label="Diagnostics">
        <div class="audio-settings__diag-title">diagnostics</div>
        <div class="audio-settings__diag-row">
          <span>host</span>
          <b>{platform.kind}</b>
        </div>
        <div class="audio-settings__diag-row">
          <span>isolated</span>
          <b>{isolated ? 'yes' : 'no'}</b>
        </div>
        <div class="audio-settings__diag-row">
          <span>plugin</span>
          <b>
            {platform.pluginHost.available
              ? scanning()
                ? 'scanning…'
                : `${availablePlugins().length} found`
              : 'unavailable'}
          </b>
        </div>
        <div class="audio-settings__diag-row">
          <span>midi</span>
          <b>{midiStatus() === 'connected' ? midiDevices().join(', ') : midiStatus()}</b>
        </div>
        {/* Native plugin PCM bridge health per loaded slot — hop-1 lag / hop-2 fill (frames), worklet
            underruns, JS drops. Polled while the popover is open; the same numbers the looper's
            record-integrity check reads, so a rejected take can be explained here. */}
        <Show when={platform.pluginHost.available}>
          <div class="audio-settings__diag-row">
            <span>bridge</span>
            <b>{bridgeHealth()}</b>
          </div>
        </Show>
      </div>
    </div>
  );
}
