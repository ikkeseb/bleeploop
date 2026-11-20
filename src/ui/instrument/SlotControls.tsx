import { For, Show, createEffect, createMemo, onCleanup } from 'solid-js';
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
import { inputChannelCount } from '../state/audio-devices';
import { SYNTHS } from '../state/instruments';
import { volumeDb } from '../looper/shared';
import { registerCardMeter, unregisterCardMeter } from '../looper/waveform';
import { engineDevice, engineSlotInputChannels, setEngineSlotInputChannel } from '../state/engine-store';
import type { InputPick as Pick, ReadPick } from '../../platform';
import { clearLiveError, liveBusy, liveError, toggleLive } from './live';
import { slotLetter } from '../state/slot-tones';
import { Icon } from './icons';

/**
 * The slot line's own controls (`InstrumentSlot.tsx` lays them out, `slot.css` draws them): the input
 * pick and LIVE of a source that takes input, the slot's input meter, and its one volume. The plugin
 * plate's editor and menu are `PluginControls.tsx`.
 */

/** A pick as the select keys it: '' auto, "2" a channel, "0+1" a pair (0-based). */
const pickKey = (p: Pick): string => (p === null ? '' : Array.isArray(p) ? `${p[0]}+${p[1]}` : String(p));
const keyPick = (k: string): Pick => (k === '' ? null : k.includes('+') ? (k.split('+').map(Number) as [number, number]) : Number(k));
/** "In 3", "In 1+2". */
const pickName = (p: ReadPick): string => `In ${Array.isArray(p) ? `${p[0] + 1}+${p[1] + 1}` : p + 1}`;

/** The slot's input ("In 1".."In N", the pairs "In 1+2", "In 3+4"…, or Auto), switched in place: a
 * small value window with a chevron, the native select laid over it (so the list is the system's, and a
 * pick is a pick). */
export function InputPick(props: { slot: 0 | 1 }) {
  const pick = () => engineSlotInputChannels()[props.slot];
  const channels = createMemo(() => {
    const n = inputChannelCount(engineDevice());
    const list: ReadPick[] = Array.from({ length: n }, (_, i) => i);
    for (let l = 0; l + 1 < n; l += 2) list.push([l, l + 1]);
    // A saved pick the device does not list stays visible rather than reading as Auto.
    const p = pick();
    if (p !== null && !list.some((c) => pickKey(c) === pickKey(p))) list.push(p);
    return list;
  });
  // Auto names the input the engine reads for the slot (its status), so a guitar on the other jack shows.
  const auto = () => {
    const d = engineDevice();
    const read = pick() === null && d?.inputOpen ? d.inputChannels?.[props.slot] : undefined;
    return read === undefined ? 'Auto' : `Auto · ${pickName(read)}`;
  };
  const shown = () => {
    const p = pick();
    return p === null ? auto() : pickName(p);
  };
  return (
    <span class="slot__inwin k-win" onClick={(e) => e.stopPropagation()}>
      <span class="slot__inval">{shown()}</span>
      <Icon name="drop" size={14} />
      <select
        class="slot__in"
        aria-label={`Input for slot ${slotLetter(props.slot)}`}
        title="The input this slot hears. Auto: input 2 on an interface with two or more"
        value={pickKey(pick())}
        onChange={(e) => setEngineSlotInputChannel(props.slot, keyPick(e.currentTarget.value))}
      >
        <option value="">{auto()}</option>
        <For each={channels()}>{(c) => <option value={pickKey(c)}>{pickName(c)}</option>}</For>
      </select>
    </span>
  );
}

/**
 * LIVE: the slot hears its own input, through its effect or dry while it is Off, and the engine plays
 * it at low latency. A latched key whose LED is lit while live. One accessible name; aria-pressed
 * carries the state. Locked only while a source op holds the slot; a press still in flight shows as
 * busy, and the next one queues.
 */
export function LiveButton(props: { slot: 0 | 1 }) {
  const slot = props.slot;
  const live = () => inputArmed()[slot];
  onCleanup(() => clearLiveError(slot));
  return (
    <>
      <button
        type="button"
        class="k-key k-key--latch slot__live"
        aria-pressed={live()}
        aria-label={`Live input for slot ${slotLetter(slot)}`}
        title={live() ? 'Live: this slot plays its input. Press to stop' : 'Press to play this slot\'s input, heard and recorded'}
        disabled={slotPendingCounts()[slot] > 0}
        aria-busy={liveBusy(slot)}
        onClick={(e) => {
          e.stopPropagation();
          void toggleLive(slot);
        }}
      >
        <span class="k-led" aria-hidden="true" />
        LIVE
      </button>
      <Show when={liveError(slot)}>
        <span class="pc__err" title={liveError(slot) ?? ''}>
          {liveError(slot)}
        </span>
      </Show>
    </>
  );
}

/**
 * The slot's input meter at its line's right end: an LED ladder, dark until the slot goes live, then
 * the slot's own input (its capture channel, before the plugin) over −60..0 dBFS, its held peak and a
 * CLIP lamp lit ~1.5 s after the input reached full scale. Drawn by the waveform rAF loop through style
 * writes on this element (`--lvl`, `--pk`, `is-clip`: `registerCardMeter`); Solid only mounts it and
 * (un)registers it as the slot goes live. Hidden from assistive tech: the command bar's meter speaks
 * the input, the louder slot's.
 */
export function SlotMeter(props: { slot: 0 | 1 }) {
  const slot = props.slot;
  const live = () => inputArmed()[slot];
  return (
    <div
      class="slot__meter"
      classList={{ 'is-live': live() }}
      aria-hidden="true"
      ref={(el) => {
        createEffect(() => {
          if (live()) registerCardMeter(slot, el);
          else unregisterCardMeter(slot, el);
        });
        onCleanup(() => unregisterCardMeter(slot, el));
      }}
    >
      <div class="slot__meter-track k-meter">
        <i class="slot__meter-pk" />
      </div>
      <span class="slot__meter-clip">CLIP</span>
    </div>
  );
}

/** Pixels of drag for the volume's whole travel (0..SLOT_GAIN_MAX); Shift drags four times finer. */
const DRAG_PX = 180;

/**
 * The slot's one volume (`slotLevel`): its plugin's output, an Off slot's input level or its synth's
 * level, 0..1.5 with unity at the lanes' 0 dB. A small knob and its dB value window, turned by a drag
 * (up or right) or a scroll on either; double-click resets to unity. The range input under them is the
 * control's keyboard and assistive face (arrows step it). The unity detent snaps only while the value
 * approaches it, so a keyboard step can leave it.
 */
export function SlotVolume(props: { slot: 0 | 1 }) {
  const slot = props.slot;
  const level = () => slotLevel(slot) ?? 1;
  const db = () => volumeDb(level());
  const pending = () => slotPendingCounts()[slot] > 0;
  const what = () =>
    slotPlugins()[slot]
      ? 'Plugin output'
      : slotOff()[slot]
        ? "Input level: what this slot's live input is heard and recorded at"
        : `${SYNTHS.find((s) => s.id === slotIds()[slot])?.name ?? 'Synth'} level, shared by every slot playing it`;
  const detentLimit = 0.03 + Number.EPSILON;
  const inUnityDetent = (value: number) => Math.abs(value - 1.0) <= detentLimit;
  /** Set `v`, snapped to unity while approaching it from outside the detent. */
  const set = (v: number) => {
    const clamped = Math.max(0, Math.min(SLOT_GAIN_MAX, v));
    const snapped = !inUnityDetent(level()) && inUnityDetent(clamped) ? 1.0 : clamped;
    setSlotLevel(slot, snapped);
    return snapped;
  };
  let input!: HTMLInputElement;
  function onInput() {
    const v = set(Number(input.value));
    // Solid does not update the native range when the snapped value equals the existing signal.
    input.value = String(v);
  }
  // A drag turns the value from where it was (never jumps to the pointer); the gesture's own value
  // runs unsnapped under the detent so a slow drag can pass through unity.
  let drag: { id: number; x: number; y: number; from: number } | null = null;
  const onDown = (e: PointerEvent) => {
    if (pending() || e.button !== 0) return;
    e.preventDefault();
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    drag = { id: e.pointerId, x: e.clientX, y: e.clientY, from: level() };
  };
  const onMove = (e: PointerEvent) => {
    if (!drag || drag.id !== e.pointerId) return;
    const px = e.clientX - drag.x - (e.clientY - drag.y);
    const raw = drag.from + (px / DRAG_PX) * SLOT_GAIN_MAX * (e.shiftKey ? 0.25 : 1);
    const v = Math.max(0, Math.min(SLOT_GAIN_MAX, raw));
    setSlotLevel(slot, inUnityDetent(v) ? 1.0 : Math.round(v * 100) / 100);
  };
  const onUp = (e: PointerEvent) => {
    if (drag?.id === e.pointerId) drag = null;
  };
  const wheel = (e: WheelEvent) => {
    if (pending()) return;
    e.preventDefault();
    set(Math.round((level() + (e.deltaY < 0 ? 0.02 : -0.02)) * 100) / 100);
  };
  const angle = () => `${-135 + (level() / SLOT_GAIN_MAX) * 270}deg`;
  const num = () => db().replace(/ dB$/, '');
  return (
    <div
      class="slot__vol"
      classList={{ 'is-off': pending() }}
      title={`${what()}. Drag or scroll; double-click for 0 dB`}
      onClick={(e) => e.stopPropagation()}
      onPointerDown={onDown}
      onPointerMove={onMove}
      onPointerUp={onUp}
      onPointerCancel={onUp}
      onDblClick={() => !pending() && setSlotLevel(slot, 1.0)}
      ref={(el) => {
        el.addEventListener('wheel', wheel, { passive: false });
        onCleanup(() => el.removeEventListener('wheel', wheel));
      }}
    >
      <span class="k-knob slot__knob" style={{ '--a': angle() }} aria-disabled={pending()} />
      <span class="k-win slot__vdb" classList={{ 'is-off': pending() }}>
        {num()}
        <Show when={db().endsWith(' dB')}>
          {' '}
          <span class="slot__unit">dB</span>
        </Show>
      </span>
      <input
        ref={input}
        class="slot__vbar"
        type="range"
        min={0}
        max={SLOT_GAIN_MAX}
        step={0.01}
        value={level()}
        onInput={onInput}
        aria-label={`Volume for slot ${slotLetter(slot)}`}
        aria-valuetext={db()}
        disabled={pending()}
      />
    </div>
  );
}
