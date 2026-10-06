import { For, Show, createEffect, createMemo, createSignal, on, onCleanup, onMount } from 'solid-js';
import { availablePlugins, scanning } from '../state/instrument';
import {
  asioAvailable,
  asioDeviceInfo,
  asioDrivers,
  asioEnabled,
  asioOffered,
  asioRetryable,
  asioStatus,
  probeAsio,
  usingAsio,
  bufferFrames,
  sampleRatePick,
  inputDevices,
  outputDevices,
  refreshAndPruneDevices,
  refreshAsioDrivers,
  saveSlotInputChannels,
  setAsioEnabled,
  setBufferSize,
  setSampleRatePick,
} from '../state/audio-devices';
import { midiDevices, midiStatus } from '../state/midi';
import {
  addPluginFolder,
  pluginFolders,
  pluginFoldersBusy,
  refreshPluginFolders,
  removePluginFolder,
} from '../state/plugin-folders';
import { ACTION_LABELS, isLaneAction, type ActionId, type Target } from '../../app/actions';
import {
  awaitingRelease,
  bindings,
  cancelLearn,
  forget,
  learn,
  learning,
  setHold,
  setMomentary,
  type MidiBinding,
} from '../../app/midi-actions';
import { platform } from '../../platform';
import {
  BUFFER_FRAMES_OPTIONS,
  asioBufferChoice,
  rateChoice,
  readAudioDeviceSettings,
  writeAudioDeviceSettings,
  type BufferFrames,
  type SampleRate,
} from '../state/audio-settings';
import { looper, sampleRate } from '../state/audio';
import {
  engineDevice,
  engineOpenFailure,
  engineShare,
  openEngineDevice,
  setEngineShare,
  switchEngineAsioDriver,
} from '../state/engine-store';
import { sharesInterface } from './share-target';
import './audio-settings.css';

/**
 * Global Audio Settings popover: the engine's input and output device, Share output, the buffer size,
 * the sample rate, the ASIO low-latency tier and its driver, the MIDI learn row, the plugin folders and
 * the diagnostics — all GLOBAL last-used preferences (the folder list is the native host's to keep). Mounted inside a `<Show>` in app.tsx, so it re-reads persisted state each
 * time it opens (persisted localStorage is the source of truth; these local signals mirror it).
 *
 * A device, buffer, rate or driver pick reopens the engine's device at once (a rate the loops were not
 * recorded at asks first: `openEngineDevice`); the input channel is each slot's own pick (the slot
 * header, `src/ui/instrument/SlotControls.tsx`); the ASIO driver row switches the driver live, and the
 * Buffer and Sample rate selects offer only the sizes and rates that driver takes.
 */

/** Processing-block duration, not an input-to-output latency estimate. */
function bufferMs(frames: number): string {
  return `~${((frames / sampleRate()) * 1000).toFixed(1)} ms/block`;
}

/** A rate as the Sample rate select names it: "44.1 kHz", "48 kHz". */
const kHz = (hz: number): string => `${hz / 1000} kHz`;

/** Reopen the engine's device on the saved picks, then `synced`: a switch the player declines puts back
 * the picks of the device that runs. */
function reopenEngine(synced: () => void): void {
  if (platform.engine.available) void openEngineDevice().then(synced);
}

const ACTION_IDS = Object.keys(ACTION_LABELS) as ActionId[];
const LANE_ACTION_IDS = ACTION_IDS.filter(isLaneAction);
const GLOBAL_ACTION_IDS = ACTION_IDS.filter((id) => !isLaneAction(id));

/** A learned message as the bindings list shows it: "CC 64 · ch 1". */
function midiSource(b: MidiBinding): string {
  return `${b.kind === 'cc' ? 'CC' : 'note'} ${b.number} · ch ${b.channel + 1}`;
}

/** A binding's action as the list names it: "Play / stop · Track 2" for a lane action on a named track. */
function bindingAction(b: MidiBinding): string {
  return b.target === null ? ACTION_LABELS[b.action] : `${ACTION_LABELS[b.action]} · Track ${b.target + 1}`;
}

export function AudioSettings() {
  const persisted = readAudioDeviceSettings();
  const [selectedDevice, setSelectedDevice] = createSignal(persisted.inputDeviceId);
  const [selectedOutput, setSelectedOutput] = createSignal(persisted.outputDeviceId);
  const [selectedDriver, setSelectedDriver] = createSignal(persisted.asioDriver);
  // The action the MIDI learn row binds to. A learn still listening when the panel closes is cancelled,
  // so a later pedal press cannot bind out of sight.
  const [learnPick, setLearnPick] = createSignal<ActionId>(ACTION_IDS[0]);
  const [learnTarget, setLearnTarget] = createSignal<Target>(null);
  onCleanup(cancelLearn);
  const engineReadout = () => {
    const d = engineDevice();
    if (!d) return engineOpenFailure() ? `no device open: ${engineOpenFailure()}` : 'no device open';
    const input = d.inputOpen ? d.inputName : 'no input';
    return `${d.backend === 'Asio' ? 'ASIO' : 'WASAPI'} · ${input} → ${d.outputName} · ${d.block} frames · ${d.alignFrames} frames round trip`;
  };

  const syncPicks = () => {
    const s = readAudioDeviceSettings();
    setSelectedDevice(s.inputDeviceId);
    setSelectedOutput(s.outputDeviceId);
    setSelectedDriver(s.asioDriver);
  };

  // Under ASIO: only the sizes the driver takes, showing the one it runs at (`asioBufferChoice`);
  // otherwise every option, showing the saved one.
  const bufferChoice = createMemo(() => {
    if (!usingAsio()) return { options: [...BUFFER_FRAMES_OPTIONS] as number[], shown: bufferFrames() as number, fixed: false };
    const info = asioDeviceInfo();
    const range = info?.bufferMin != null && info.bufferMax != null ? { min: info.bufferMin, max: info.bufferMax } : null;
    const running = engineDevice();
    return asioBufferChoice(bufferFrames(), range, running?.backend === 'Asio' ? running.block : null);
  });

  // What runs, or with no device running the saved preference: under ASIO the rates the driver runs,
  // under WASAPI the endpoint's own alone (`rateChoice`). A fallback the owner made on its own (ASIO
  // lost, WASAPI runs) shows WASAPI's; the saved pick stays for the next ASIO open.
  const rateOnAsio = () => {
    const running = engineDevice();
    return running ? running.backend === 'Asio' : usingAsio();
  };
  const rates = createMemo(() =>
    rateChoice(sampleRatePick(), rateOnAsio() ? (asioDeviceInfo()?.sampleRates ?? []) : [], engineDevice()?.sampleRate ?? null),
  );
  const rateValue = (rate: SampleRate | null) => (rate === null ? '' : String(rate));
  // "Device (48 kHz)" while the device's own rate runs; "Device" while none runs or a pick does.
  const deviceRateLabel = () => {
    const running = engineDevice();
    return running && rates().shown === null ? `Device (${kHz(running.sampleRate)})` : 'Device';
  };

  // The driver picker: the installed drivers, plus a saved pick that is no longer installed (the probe
  // took the automatic choice instead).
  const driverOptions = () => {
    const names = asioDrivers();
    const saved = selectedDriver();
    return saved && !names.includes(saved) ? [...names, saved] : names;
  };

  // The Share device's name when ASIO runs and it looks like the ASIO interface; null otherwise.
  const shareOnAsioInterface = createMemo(() => {
    if (!usingAsio()) return null;
    const name = outputDevices().find((d) => d.id === engineShare())?.name;
    return name && sharesInterface(asioDeviceInfo()?.name ?? '', name) ? name : null;
  });

  // The host's list of plugins it cannot host comes from the last scan: read the folders again each
  // time a scan ends (an add or remove is followed by one; so is the command bar's rescan).
  createEffect(
    on(scanning, (now, was) => {
      if (was && !now) void refreshPluginFolders();
    }),
  );

  onMount(async () => {
    void refreshPluginFolders();
    // Refresh the device lists + prune any persisted id no longer present (shared with the startup
    // prune), then re-sync the local signals from the pruned persisted settings — catches a device
    // unplugged since this popover last opened. No-op in the web build.
    await refreshAndPruneDevices();
    syncPicks();
    if (asioOffered()) await refreshAsioDrivers();
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
            writeAudioDeviceSettings({ inputDeviceId: v, inputChannel: '' });
            saveSlotInputChannels(['', '']); // the slots' picks too (a declined switch puts them back)
            reopenEngine(syncPicks);
          }}
          aria-label="Audio input device"
        >
          <option value="">{usingAsio() ? asioDeviceInfo()?.name ?? 'Default ASIO driver' : 'System default'}</option>
          <For each={inputDevices()}>
            {(d) => <option value={d.id} selected={!usingAsio() && d.id === selectedDevice()}>{d.name}</option>}
          </For>
        </select>
      </div>

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
            reopenEngine(syncPicks);
          }}
          aria-label="Output device"
        >
          <option value="">{usingAsio() ? asioDeviceInfo()?.name ?? 'Default ASIO driver' : 'System default'}</option>
          <For each={outputDevices()}>
            {(d) => <option value={d.id} selected={!usingAsio() && d.id === selectedOutput()}>{d.name}</option>}
          </For>
        </select>
      </div>

      {/* Share output: the master mirrored to a Windows device for OBS, a browser or a call,
          while the engine runs on ASIO. A device that goes away turns it off with a toast. */}
      <div class="audio-settings__row" title="Mirror the master to another Windows device while ASIO runs">
        <span class="audio-settings__label">share</span>
        <select
          class="audio-settings__select"
          value={engineShare()}
          onChange={(e) => {
            const select = e.currentTarget;
            void setEngineShare(select.value).then(() => {
              select.value = engineShare();
            });
          }}
          aria-label="Share output device"
        >
          <option value="">Off</option>
          <For each={outputDevices()}>
            {(d) => <option value={d.id} selected={d.id === engineShare()}>{d.name}</option>}
          </For>
        </select>
      </div>
      <div class="audio-settings__hint audio-settings__hint--info" role="note">
        Mirrors the master to that device while ASIO runs. Pick one you don't listen on, such as a virtual
        cable: on the interface in your ears, you hear everything twice. On WASAPI, capture BleepLoop's own
        output instead.
      </div>
      {/* F22's other half: the Share pick looks like the interface ASIO plays on (`sharesInterface`, a
          brand/model-word match: it cannot see the hardware). A caution in the amber note, never a
          block: the pick stays. */}
      <Show when={shareOnAsioInterface()}>
        {(name) => (
          <div class="audio-settings__hint" role="note">
            {name()} looks like the interface ASIO plays on: if you listen there, you will hear the master
            twice. Pick a virtual cable or a device you don't listen on.
          </div>
        )}
      </Show>
      <Show when={usingAsio()}>
        <div class="audio-settings__hint audio-settings__hint--info" role="note">ASIO drives both input and output. Turn ASIO off to pick Windows devices.</div>
      </Show>

      <div class="audio-settings__row">
        <span class="audio-settings__label">buffer</span>
        <select
          class="audio-settings__select"
          value={String(bufferChoice().shown)}
          onChange={(e) => {
            const v = Number(e.currentTarget.value);
            const select = e.currentTarget;
            // A size outside the list is the driver's own, shown because it runs: nothing to save.
            if (!(BUFFER_FRAMES_OPTIONS as readonly number[]).includes(v)) {
              select.value = String(bufferChoice().shown);
              return;
            }
            void setBufferSize(v as BufferFrames).then(() => {
              select.value = String(bufferChoice().shown);
              reopenEngine(syncPicks);
            });
          }}
          aria-label="Buffer size in frames"
        >
          <For each={bufferChoice().options}>
            {(f) => <option value={String(f)} selected={f === bufferChoice().shown}>{f} frames</option>}
          </For>
        </select>
        <span class="audio-settings__readout">{bufferMs(bufferChoice().shown)}</span>
      </div>
      <div class="audio-settings__hint audio-settings__hint--info" role="note">
        {bufferChoice().fixed
          ? "Set by the driver: change it in the driver's control panel."
          : 'Frames per device callback: smaller is lower latency, larger is safer.'}
      </div>

      <div class="audio-settings__row">
        <span class="audio-settings__label">sample rate</span>
        <select
          class="audio-settings__select"
          value={rateValue(rates().shown)}
          onChange={(e) => {
            const select = e.currentTarget;
            const rate = select.value === '' ? null : (Number(select.value) as SampleRate);
            void setSampleRatePick(rate).then(() => {
              select.value = rateValue(rates().shown);
              reopenEngine(syncPicks);
            });
          }}
          aria-label="Sample rate"
        >
          <For each={rates().options}>
            {(rate) => (
              <option value={rateValue(rate)} selected={rate === rates().shown}>
                {rate === null ? deviceRateLabel() : kHz(rate)}
              </option>
            )}
          </For>
        </select>
      </div>
      <div class="audio-settings__hint audio-settings__hint--info" role="note">
        {!rates().fixed
          ? "The engine's rate. Loops recorded at one rate wait in recovery at another."
          : rateOnAsio()
            ? "Set by the driver: change it in the driver's control panel."
            : "Set by Windows: change it in the device's Sound settings, or use ASIO to pick one here."}
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
              disabled={asioStatus().status === 'probing'}
              onChange={(e) => {
                const checkbox = e.currentTarget;
                void setAsioEnabled(checkbox.checked).then(() => {
                  checkbox.checked = asioEnabled();
                  reopenEngine(syncPicks);
                });
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
      {/* The ASIO driver: picking one switches live — the device closes, the driver
          starts, the device reopens (`switchEngineAsioDriver`). Automatic names the driver it took. */}
      <Show when={asioOffered() && asioEnabled()}>
        <div class="audio-settings__row" title="The ASIO driver the engine opens. Automatic takes the first one that starts.">
          <span class="audio-settings__label">driver</span>
          <select
            class="audio-settings__select"
            value={selectedDriver()}
            disabled={asioStatus().status === 'probing' || asioStatus().status === 'timed-out'}
            onChange={(e) => {
              const v = e.currentTarget.value;
              setSelectedDriver(v);
              void switchEngineAsioDriver(v).then(syncPicks);
            }}
            aria-label="ASIO driver"
          >
            <option value="" selected={selectedDriver() === ''}>
              {selectedDriver() === '' && asioDeviceInfo() ? `Automatic (${asioDeviceInfo()?.name})` : 'Automatic'}
            </option>
            <For each={driverOptions()}>
              {(name) => (
                <option value={name} selected={name === selectedDriver()}>
                  {asioDrivers().length === 0 || asioDrivers().includes(name) ? name : `${name} (not installed)`}
                </option>
              )}
            </For>
          </select>
        </div>
      </Show>
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
              onClick={() =>
                void probeAsio(true).then((report) => {
                  // The device runs on WASAPI meanwhile: a driver that starts takes over at once.
                  if (report.status === 'ready') reopenEngine(syncPicks);
                })
              }
              aria-label="Retry starting the ASIO driver"
            >
              RETRY ASIO
            </button>
          </Show>
        </div>
      </Show>

      {/* MIDI learn: pick an action (a track action also its track), LEARN, and the next CC or note-on from
          any port runs it from then on (a second click or Esc cancels). A learned message never reaches
          the play path. The bindings, their persistence and the momentary/latching read live in
          `src/app/midi-actions.ts`; each line can switch the kind, and a momentary REC/DUB pedal can HOLD. */}
      <div class="audio-settings__row" title="A MIDI footswitch or key runs this action">
        <span class="audio-settings__label">midi learn</span>
        <select
          class="audio-settings__select"
          value={learnPick()}
          disabled={learning() !== null}
          onChange={(e) => setLearnPick(e.currentTarget.value as ActionId)}
          aria-label="Action to learn"
        >
          <optgroup label="Track actions">
            <For each={LANE_ACTION_IDS}>{(id) => <option value={id}>{ACTION_LABELS[id]}</option>}</For>
          </optgroup>
          <optgroup label="Global actions">
            <For each={GLOBAL_ACTION_IDS}>{(id) => <option value={id}>{ACTION_LABELS[id]}</option>}</For>
          </optgroup>
        </select>
        <button
          type="button"
          class="audio-settings__btn"
          classList={{ 'audio-settings__btn--listening': learning() !== null }}
          aria-pressed={learning() !== null}
          disabled={learning() === null && midiStatus() !== 'connected'}
          onClick={() => (learning() === null ? learn(learnPick(), learnTarget()) : cancelLearn())}
          aria-label="Learn a MIDI control for this action"
        >
          {learning() === null ? 'LEARN' : 'LISTENING'}
        </button>
      </div>
      <Show when={isLaneAction(learnPick())}>
        <div class="audio-settings__row" title="The track this action acts on">
          <span class="audio-settings__label">on track</span>
          <select
            class="audio-settings__select"
            value={learnTarget() === null ? '' : String(learnTarget())}
            disabled={learning() !== null}
            onChange={(e) => setLearnTarget(e.currentTarget.value === '' ? null : Number(e.currentTarget.value))}
            aria-label="Track the action acts on"
          >
            <option value="">Selected track</option>
            <For each={Array.from({ length: looper.trackCount }, (_, i) => i)}>{(i) => <option value={String(i)}>Track {i + 1}</option>}</For>
          </select>
        </div>
      </Show>
      <Show when={learning() !== null}>
        <div class="audio-settings__hint audio-settings__hint--info" role="status">Tap a pedal or key on a MIDI device. Esc cancels.</div>
      </Show>
      <Show when={awaitingRelease()}>
        {(b) => (
          <div class="audio-settings__hint audio-settings__hint--info" role="status">
            Learned {midiSource(b())}. Let go of the pedal, and try it once this line is gone.
          </div>
        )}
      </Show>
      <Show when={bindings().length > 0}>
        <ul class="audio-settings__bindings" aria-label="MIDI bindings">
          <For each={bindings()}>
            {(b) => (
              <li class="audio-settings__binding" title={b.portName}>
                <span class="audio-settings__binding-action">{bindingAction(b)}</span>
                <span class="audio-settings__binding-src">{midiSource(b)}</span>
                <button
                  type="button"
                  class="audio-settings__chip"
                  onClick={() => setMomentary(b, !b.momentary)}
                  aria-label={`${bindingAction(b)} on ${midiSource(b)}: ${b.momentary ? 'momentary' : 'latching'} pedal, switch to ${b.momentary ? 'latching' : 'momentary'}`}
                  title="How the pedal was read. Switch it if a press runs twice, or every other press runs nothing"
                >
                  {b.momentary ? 'momentary' : 'latching'}
                </button>
                <Show when={b.action === 'recDub'}>
                  <button
                    type="button"
                    class="audio-settings__chip"
                    classList={{ 'is-on': b.hold }}
                    disabled={!b.momentary}
                    aria-pressed={b.hold}
                    onClick={() => setHold(b, !b.hold)}
                    aria-label={`Hold to record on ${midiSource(b)}`}
                    title={b.momentary ? 'Hold the pedal to record or overdub, let go to stop' : 'HOLD needs a momentary pedal'}
                  >
                    HOLD
                  </button>
                </Show>
                <button
                  type="button"
                  class="audio-settings__binding-clear"
                  onClick={() => forget(b)}
                  aria-label={`Forget ${bindingAction(b)} on ${midiSource(b)}, ${b.portName}`}
                >
                  ✕
                </button>
              </li>
            )}
          </For>
        </ul>
      </Show>

      {/* Plugin folders: what the scan walks. The host's own folders are read-only; the player's are
          added through the native folder dialog (the host opens it) and removed with ✕. Each change is
          followed by a scan (`state/plugin-folders.ts`); a folder that is gone says so and is skipped. Under
          the folders, what the last scan found and cannot host (a 32-bit VST2, say), with why. */}
      <Show when={platform.pluginHost.available}>
        <div class="audio-settings__folders" role="group" aria-label="Plugin folders">
          <div class="audio-settings__row" title="The folders the plugin scan walks">
            <span class="audio-settings__label">folders</span>
            <button
              type="button"
              class="audio-settings__btn"
              disabled={scanning() || pluginFoldersBusy()}
              onClick={() => void addPluginFolder()}
            >
              Add folder…
            </button>
          </div>
          <ul class="audio-settings__folder-list">
            <For each={pluginFolders().builtin}>
              {(folder) => (
                <li class="audio-settings__folder audio-settings__folder--builtin" title={folder.path}>
                  <span class="audio-settings__folder-path">{folder.path}</span>
                  <Show when={!folder.exists}>
                    <span class="audio-settings__folder-missing">missing</span>
                  </Show>
                </li>
              )}
            </For>
            <For each={pluginFolders().user}>
              {(folder) => (
                <li class="audio-settings__folder" title={folder.path}>
                  <span class="audio-settings__folder-path">{folder.path}</span>
                  <Show when={!folder.exists}>
                    <span class="audio-settings__folder-missing">missing</span>
                  </Show>
                  <button
                    type="button"
                    class="audio-settings__binding-clear"
                    disabled={pluginFoldersBusy()}
                    onClick={() => void removePluginFolder(folder.path)}
                    aria-label={`Remove folder ${folder.path}`}
                  >
                    ✕
                  </button>
                </li>
              )}
            </For>
          </ul>
          <Show when={pluginFolders().unsupported.length > 0}>
            <div class="audio-settings__unsupported">
              <div class="audio-settings__unsupported-summary">
                {pluginFolders().unsupported.length === 1
                  ? '1 plugin found but not supported'
                  : `${pluginFolders().unsupported.length} plugins found but not supported`}
              </div>
              <ul class="audio-settings__unsupported-list">
                <For each={pluginFolders().unsupported}>
                  {(plugin) => (
                    <li class="audio-settings__unsupported-item" title={plugin.path}>
                      <span class="audio-settings__unsupported-file">{plugin.path.split(/[\\/]/).pop()}</span>
                      <span class="audio-settings__unsupported-reason">{plugin.reason}</span>
                    </li>
                  )}
                </For>
              </ul>
            </div>
          </Show>
        </div>
      </Show>

      {/* Diagnostics: the read-only host/engine/plugin/midi status. The command-bar system lamp is
          their aggregate; this is the full read-out. The plugin-scan live region stays in the command
          bar (single announcer), so these rows are plain text — no role=status here. */}
      <div class="audio-settings__diag" role="group" aria-label="Diagnostics">
        <div class="audio-settings__diag-title">diagnostics</div>
        <div class="audio-settings__diag-row">
          <span>host</span>
          <b>{platform.kind}</b>
        </div>
        <div class="audio-settings__diag-row">
          <span>engine</span>
          <b>{engineReadout()}</b>
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
      </div>
    </div>
  );
}
