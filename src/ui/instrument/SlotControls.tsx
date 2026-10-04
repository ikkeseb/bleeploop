import { For, Show, createMemo, onCleanup } from 'solid-js';
import {
  SLOT_GAIN_MAX,
  setSlotLevel,
  slotIds,
  slotLevel,
  slotOff,
  slotPendingCounts,
  slotPlugins,
} from '../state/instrument';
import { inputArmed } from '../state/native-io';
import { asioDeviceInfo, inputDevices, usingAsio } from '../state/audio-devices';
import { SYNTHS } from '../state/instruments';
import { volumeDb } from '../looper/shared';
import { engineDevice, engineSlotInputChannels, setEngineSlotInputChannel } from '../state/engine-store';
import { clearLiveError, liveBusy, liveError, toggleLive } from './live';

/**
 * The slot header's own controls (`InstrumentSlot.tsx` lays them out): the input pick and GO LIVE of a
 * source that takes input, and the slot's one volume. The plugin's EDITOR / PARAMS pair is
 * `PluginControls.tsx`.
 */

/** The capture channels the engine's input device offers; 0 = unknown (the pick then offers Auto and
 * the saved channel only). */
function inputChannelCount(): number {
  if (usingAsio()) return asioDeviceInfo()?.inputChannels ?? 0;
  const d = engineDevice();
  if (!d?.inputOpen) return 0;
  return inputDevices().find((x) => x.name === d.inputName)?.channels ?? 0;
}

/** The slot's capture channel ("In 1".."In N", or Auto), switched in place. */
export function InputPick(props: { slot: 0 | 1 }) {
  const pick = () => engineSlotInputChannels()[props.slot];
  const channels = createMemo(() => {
    const list = Array.from({ length: inputChannelCount() }, (_, i) => String(i));
    // A saved pick the device does not list stays visible rather than reading as Auto.
    if (pick() !== '' && !list.includes(pick())) list.push(pick());
    return list;
  });
  // Auto names the input the engine reads for the slot (its status), so a guitar on the other jack shows.
  const auto = () => {
    const d = engineDevice();
    const read = pick() === '' && d?.inputOpen ? d.inputChannels?.[props.slot] : undefined;
    return read === undefined ? 'Auto' : `Auto · In ${read + 1}`;
  };
  return (
    <select
      class="slot__in"
      aria-label={`Input for slot ${props.slot + 1}`}
      title="The input this slot hears. Auto: input 2 on an interface with two or more"
      value={pick()}
      onClick={(e) => e.stopPropagation()}
      onChange={(e) => setEngineSlotInputChannel(props.slot, e.currentTarget.value)}
    >
      <option value="">{auto()}</option>
      <For each={channels()}>{(c) => <option value={c}>In {Number(c) + 1}</option>}</For>
    </select>
  );
}

/**
 * GO LIVE / INPUT LIVE: the slot hears its own input, through its effect or dry while it is Off, and
 * the engine plays it at low latency. One accessible name; aria-pressed carries the state.
 */
export function LiveButton(props: { slot: 0 | 1 }) {
  const slot = props.slot;
  const live = () => inputArmed()[slot];
  onCleanup(() => clearLiveError(slot));
  return (
    <>
      <button
        type="button"
        class="tgl live"
        classList={{ 'on-green': live() }}
        aria-pressed={live()}
        aria-label={`Live input for slot ${slot + 1}`}
        disabled={slotPendingCounts()[slot] > 0 || liveBusy(slot)}
        onClick={(e) => {
          e.stopPropagation();
          void toggleLive(slot);
        }}
      >
        <Show when={live()}>
          <i class="tgl__dot" aria-hidden="true" />
        </Show>
        {live() ? 'INPUT LIVE' : 'GO LIVE'}
      </button>
      <Show when={liveError(slot)}>
        <span class="pc__err">{liveError(slot)}</span>
      </Show>
    </>
  );
}

/**
 * The slot's one volume (`slotLevel`): its plugin's output, an Off slot's input level or its synth's
 * level, 0..1.5 with unity at the lane faders' tick and read-out. The unity detent snaps only while the
 * thumb approaches it, so a keyboard step can leave it; double-click resets to unity.
 */
export function SlotVolume(props: { slot: 0 | 1 }) {
  const slot = props.slot;
  const level = () => slotLevel(slot) ?? 1;
  const db = () => volumeDb(level());
  const what = () =>
    slotPlugins()[slot]
      ? 'Plugin output'
      : slotOff()[slot]
        ? "Input level: what this slot's live input is heard and recorded at"
        : `${SYNTHS.find((s) => s.id === slotIds()[slot])?.name ?? 'Synth'} level, shared by every slot playing it`;
  const set = (v: number) => setSlotLevel(slot, v);
  function onInput(input: HTMLInputElement) {
    let v = Number(input.value);
    const detentLimit = 0.03 + Number.EPSILON;
    const inUnityDetent = (value: number) => Math.abs(value - 1.0) <= detentLimit;
    if (!inUnityDetent(level()) && inUnityDetent(v)) v = 1.0; // snap only while approaching unity
    set(v);
    // Solid does not update the native range when the snapped value equals the existing signal.
    // Keep the thumb aligned with the stored value inside the unity detent.
    input.value = String(v);
  }
  return (
    <div class="slot__vol" title={what()} onClick={(e) => e.stopPropagation()}>
      <div class="slot__vbar-wrap">
        {/* unity (0 dB) tick behind the thumb, as on the lane faders */}
        <div class="slot__vbar-unity" />
        <input
          class="lf-range slot__vbar"
          type="range"
          min={0}
          max={SLOT_GAIN_MAX}
          step={0.01}
          value={level()}
          style={{ '--fill': `${(level() / SLOT_GAIN_MAX) * 100}%` }}
          onInput={(ev) => onInput(ev.currentTarget)}
          onDblClick={() => set(1.0)}
          aria-label={`Volume for slot ${slot + 1}`}
          aria-valuetext={db()}
          disabled={slotPendingCounts()[slot] > 0}
        />
      </div>
      <span class="slot__vdb">{db()}</span>
    </div>
  );
}
