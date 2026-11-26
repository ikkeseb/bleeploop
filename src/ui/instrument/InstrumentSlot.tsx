import { For, Show, createMemo, createSignal } from 'solid-js';
import {
  activeSlot,
  availablePlugins,
  selectOff,
  selectPlugin,
  selectSynth,
  scanning,
  setActiveSlot,
  slotIds,
  slotLevel,
  slotOff,
  slotPendingCounts,
  slotPlugins,
  slotUnset,
} from '../state/instrument';
import {
  pluginDescriptorKey,
  pluginPickerLabel,
  samePluginDescriptor,
} from '../state/plugin-descriptor';
import { SYNTHS } from '../state/instruments';
import { jackHost, platform } from '../../platform';
import { engineDevice, engineOpenFailure } from '../state/engine-store';
import { PluginParams, PluginPlate, createPluginEditor } from './PluginControls';
import { InputPick, LiveButton, SlotMeter, SlotVolume } from './SlotControls';
import { PlateMenu } from './PlateMenu';
import { Icon } from './icons';
import { fxFolded, setShownSlot, shownSlot, slotsFolded } from './row-fit';
import { InputFx } from '../transport/InputFx';
import './slot.css';
import { liveShown } from './live';
import { openSettings, type SettingsPage } from '../../settings-opener';
import { revealKeyboardForSynth } from '../layout/layout-store';
import { inputArmed } from '../state/native-io';
import { appliedHead, chainShown, slotChain } from '../state/chain';
import { ChainLine } from '../units/ChainLine';

/**
 * One instrument slot (A / B) as a signal line in the slot row (`slot.css` draws it): its letter (the
 * MIDI slot's lit; also the keyboard/SR activation control) · the input pick and LIVE of a source that
 * takes input → its unit plate on the signal line (a loaded plugin, a built-in synth, or an empty plate
 * that picks one) and, in slot A, the four input effects (`InputFx.tsx`) → at its right end its input
 * meter and its volume. The source picker is the plate's name: a transparent select laid over it (Off,
 * the built-in instruments, the scanned plugins); the plate's menu holds what its keys did (the editor,
 * the parameters drawer, a change of sound, Remove). Under the line, in flow: the install hint and the
 * parameters drawer.
 * On a guitar-first install a slot that has never held a source (`slotUnset`) asks for one: slot A's
 * picker reads `Pick your amp-sim` over its Off (whose input pick, LIVE and meter the guitar needs
 * next), and slot B folds to its letter and a picker reading `Add a second sound`, so A takes the row.
 * Where the row cannot hold both lines (`row-fit.ts`), the slot not shown folds to one plate at the
 * row's end; a press on it shows its line instead.
 * THE SEAM: while the platform's chain host answers (`chainShown`: the DEV browser fake now, the native
 * engine after its next step), the line draws the slot's chain from the feed instead (`ChainLine.tsx`:
 * its units' plates and the `+` that adds them; slot A's input effects are units there, unit contract
 * §6) and no slot folds to its first-run ask; otherwise it is exactly the fixed topology above.
 * app.tsx mounts two of these (slot 0 / slot 1). The per-slot `paramsOpen` disclosure is state LOCAL to
 * each instance, and a SIBLING of the looper region, so opening it never remounts the looper's RAF
 * canvases.
 */
export function InstrumentSlot(props: { slot: 0 | 1; returnFocus?: (el: HTMLElement | undefined) => void }) {
  const slotIdx = props.slot;
  const isActive = () => activeSlot() === slotIdx;
  const plugin = () => slotPlugins()[slotIdx];
  const pending = () => slotPendingCounts()[slotIdx] > 0;
  const letter = String.fromCharCode(65 + slotIdx); // A / B
  const drawerId = `slot-${slotIdx}-params`;
  const showPlugins = () => platform.pluginHost.available && availablePlugins().length > 0;
  // Per-slot accordion disclosure for the plugin params drawer. Created once (the component mounts once
  // per slot); a sibling of the looper region, so opening it never remounts the looper.
  const [paramsOpen, setParamsOpen] = createSignal(false);
  // The loaded plugin's editor state, held here and not in its plate: a fold of the row unmounts the
  // plate while the native window stays open, so the next press must still close it. One per descriptor
  // (the memo of the descriptor settles on an unchanged one), so a swap starts closed, as the keyed plate
  // below remounts.
  const loaded = createMemo(() => slotPlugins()[slotIdx]);
  const editor = createMemo(() => {
    const desc = loaded();
    return desc ? createPluginEditor(slotIdx, desc) : null;
  });
  // One-shot: the descriptor key the user just picked in the picker. The PluginPlate that mounts for it
  // auto-starts (live + editor) and clears this; a reload resync never sets it, so it never reopens
  // windows. Plain variable — it is read once at PluginPlate mount, nothing renders from it.
  let freshPickKey: string | null = null;
  // The source picker, opened by a plate menu's Change sound.
  let sourceSelect: HTMLSelectElement | undefined;
  // Never held a source on a guitar-first install, and nothing is on its way: the picker asks for one.
  const chained = chainShown();
  const unset = () => !chained && slotUnset()[slotIdx] && slotOff()[slotIdx] && !plugin() && !pending();
  // Slot B folds while unset, unless it went live meanwhile (a pedal's GO LIVE on it while active).
  const folded = () => slotIdx === 1 && unset() && !inputArmed()[1];
  // The row too narrow for both lines: this one shows as a plate at the row's end (never slot B's
  // first-run fold, which is already one plate, nor while it is the other's only company).
  const otherFirstRun = () => !chained && slotIdx === 0 && slotUnset()[1] && slotOff()[1] && !slotPlugins()[1] && !inputArmed()[1];
  const rowFolded = () => slotsFolded() && !folded() && !otherFirstRun() && shownSlot() !== slotIdx;
  const live = () => inputArmed()[slotIdx];
  const synth = () => (slotOff()[slotIdx] ? undefined : SYNTHS.find((s) => s.id === slotIds()[slotIdx]));
  // The picker's value: '' (its hidden "Updating…" entry) while a source change is queued, 'unset' (its
  // hidden ask) while `unset`.
  const source = () =>
    pending()
      ? ''
      : plugin()
        ? pluginDescriptorKey(plugin()!)
        : unset()
          ? 'unset'
          : slotOff()[slotIdx]
            ? 'off'
            : slotIds()[slotIdx];
  const ask = slotIdx === 1 ? 'Add a second sound' : jackHost ? 'Pick a sound' : 'Pick your amp-sim';
  // What the plate's name reads: the plugin's or synth's name, else the picker's own words.
  const sourceName = () =>
    pending() ? 'Updating…' : plugin() ? plugin()!.name : unset() ? ask : (synth()?.name ?? 'Off');
  // The folded slot's plate with the chain shown: its head unit and how many follow.
  const foldName = () => {
    if (!chained) return sourceName();
    const units = slotChain(slotIdx).units;
    if (units.length === 0) return 'Empty';
    const head = appliedHead(slotIdx) ?? units[0];
    const name = head.kind === 'synth' ? (SYNTHS[('params' in head ? head.params?.instrument : 0) ?? 0]?.name ?? head.name) : head.name;
    return units.length > 1 ? `${name} +${units.length - 1}` : name;
  };
  const pick = (value: string) => {
    if (value === 'off' || SYNTHS.some((s) => s.id === value)) {
      freshPickKey = null;
      // A built-in synth plays from keys: a keyboard still waiting on a guitar-first install shows.
      if (value !== 'off') revealKeyboardForSynth();
      // selectSynth activates the slot itself once its queued work lands (not when a failed unload
      // keeps the plugin); Off leaves the MIDI slot where it is.
      if (value === 'off') selectOff(slotIdx);
      else selectSynth(slotIdx, value);
      return;
    }
    const desc = availablePlugins().find((p) => pluginDescriptorKey(p) === value);
    if (!desc) return;
    freshPickKey = value;
    // A failed load mounts no PluginPlate — drop the flag so a later resync can't inherit it.
    void selectPlugin(slotIdx, desc).then(() => {
      if (!samePluginDescriptor(slotPlugins()[slotIdx], desc)) freshPickKey = null;
    });
  };
  // The source picker: Off first (no notes; LIVE passes the slot's input dry), the built-in instruments,
  // then the native plugins (Tauri only; ONE list scales to any count). Laid over the plate's name.
  const picker = (chevron: boolean) => (
    <span class="plate__pick" onClick={(e) => e.stopPropagation()}>
      <span class="plate__name">{sourceName()}</span>
      <Show when={chevron}>
        <Icon name="drop" size={14} />
      </Show>
      <select
        ref={sourceSelect}
        class="slot__source"
        classList={{ 'slot__source--ask': unset() }}
        aria-label={`Source for slot ${letter}`}
        aria-busy={pending()}
        disabled={pending()}
        value={source()}
        title={plugin()?.path}
        onChange={(e) => {
          e.stopPropagation();
          pick(e.currentTarget.value);
        }}
      >
        <option value="" hidden selected={source() === ''}>
          Updating…
        </option>
        <option value="unset" hidden selected={source() === 'unset'}>
          {ask}
        </option>
        <option value="off" selected={source() === 'off'}>
          Off
        </option>
        <optgroup label="Built-in">
          <For each={SYNTHS}>
            {(s) => (
              <option value={s.id} selected={source() === s.id}>
                {s.name}
              </option>
            )}
          </For>
        </optgroup>
        <Show when={showPlugins()}>
          <optgroup label="Plugins">
            <For each={availablePlugins()}>
              {(p) => (
                <option value={pluginDescriptorKey(p)} title={p.path} selected={source() === pluginDescriptorKey(p)}>
                  {pluginPickerLabel(p, availablePlugins())}
                </option>
              )}
            </For>
          </optgroup>
        </Show>
      </select>
    </span>
  );
  const changeSound = () => sourceSelect?.showPicker?.();
  return (
    // Presentational group (not itself a button — it holds buttons). Whole-slot onClick is a mouse
    // convenience; the keyboard/SR activation control is the A/B identity button.
    <div
      class={`slot slot--${letter.toLowerCase()}`}
      classList={{ 'slot--active': isActive(), 'slot--folded': folded(), 'slot--fold': rowFolded() }}
      role="group"
      aria-label={`Slot ${letter}${isActive() ? ', active' : ''}`}
      aria-busy={pending()}
      onClick={() => setActiveSlot(slotIdx)}
    >
      <Show
        when={!rowFolded()}
        fallback={
          <div class="slot__line">
            <button
              type="button"
              class="plate slot__fold"
              classList={{ 'plate--idle': !live() }}
              aria-label={`Show slot ${letter}: ${foldName()}`}
              title={`Slot ${letter}: ${foldName()}. Press to show its line`}
              onClick={(e) => {
                e.stopPropagation();
                setShownSlot(slotIdx);
              }}
            >
              <span class="plate__lamp" classList={{ 'is-on': live() || (chained ? appliedHead(slotIdx)?.source === true : !!synth()) }} aria-hidden="true" />
              <span class="slot__fold-id">{letter}</span>
              <span class="plate__name">{foldName()}</span>
              {/* its meter stays in sight folded: a hot or clipping input reads from either slot */}
              <Show when={liveShown(slotIdx)}>
                <SlotMeter slot={slotIdx} />
              </Show>
              <Icon name="swap" size={16} />
            </button>
          </div>
        }
      >
        <div class="slot__line">
          <button
            class="slot__id"
            aria-pressed={isActive()}
            aria-label={`Activate slot ${letter}`}
            title={`Slot ${letter}${isActive() ? ': the keys and MIDI play it' : ''}`}
            onClick={(e) => {
              e.stopPropagation();
              setActiveSlot(slotIdx);
            }}
          >
            {letter}
          </button>
          <Show when={liveShown(slotIdx) && !folded()}>
            <InputPick slot={slotIdx} />
            <LiveButton slot={slotIdx} />
          </Show>
          <div class="slot__chain" onClick={(e) => e.stopPropagation()}>
            <Show when={!folded()}>
              <i class="slot__sig" classList={{ 'is-dim': liveShown(slotIdx) && !live() }} aria-hidden="true" />
            </Show>
            <Show when={chained}>
              <ChainLine slot={slotIdx} live={live()} returnFocus={props.returnFocus} />
            </Show>
            {/* The plate. `keyed` on the plugin so a swap remounts it (resets editor/live). */}
            <Show when={!chained}>
            <Show
              keyed
              when={slotPlugins()[slotIdx]}
              fallback={
                <Show
                  when={synth() && !pending()}
                  fallback={
                    <div class="plate plate--empty" classList={{ 'plate--ask': unset() }}>
                      {picker(true)}
                    </div>
                  }
                >
                  <div class="plate plate--idle plate--synth">
                    <span class="plate__lamp is-on" aria-hidden="true" />
                    {picker(false)}
                    <span class="plate__what" title="A built-in instrument: it plays from the keys and MIDI">
                      <Icon name="keys" size={18} />
                    </span>
                    <PlateMenu
                      id={`slot-${slotIdx}-menu`}
                      label={`Menu for slot ${letter}`}
                      menuLabel={`Slot ${letter}: ${synth()?.name ?? ''}`}
                      returnFocus={props.returnFocus}
                      groups={[
                        [
                          { label: 'Change sound', icon: 'swap', disabled: pending(), onSelect: changeSound },
                          { label: 'Remove', icon: 'x', disabled: pending(), onSelect: () => pick('off') },
                        ],
                      ]}
                    />
                  </div>
                </Show>
              }
            >
              {(desc) => (
                <PluginPlate
                  slot={slotIdx}
                  descriptor={desc}
                  name={picker(false)}
                  live={live()}
                  paramsOpen={paramsOpen()}
                  onToggleParams={() => setParamsOpen((v) => !v)}
                  drawerId={drawerId}
                  onChangeSound={changeSound}
                  onRemove={() => pick('off')}
                  returnFocus={props.returnFocus}
                  autoStart={freshPickKey === pluginDescriptorKey(desc)}
                  onAutoStartDone={() => (freshPickKey = null)}
                  editor={editor()!}
                />
              )}
            </Show>
            </Show>
            <Show when={slotIdx === 0 && !chained}>
              <InputFx folded={fxFolded()} returnFocus={props.returnFocus} />
            </Show>
          </div>
          {/* A folded slot is its letter and its picker only. */}
          <Show when={!folded()}>
            <div class="slot__out">
              <Show when={liveShown(slotIdx)}>
                <SlotMeter slot={slotIdx} />
              </Show>
              <Show when={slotLevel(slotIdx) !== null}>
                <SlotVolume slot={slotIdx} />
              </Show>
            </div>
          </Show>
        </div>
        <Show when={!folded() && platform.pluginHost.available && availablePlugins().length === 0}>
          <span class="slot__plugin-note" role="note">
            {scanning() ? (
              'Scanning plugins…'
            ) : !engineDevice() && engineOpenFailure() ? (
              <>
                No audio device open. See <SettingsLink page="device" />.
              </>
            ) : (
              <>
                No plugins found ·{' '}
                <span class="slot__plugin-note-more">the standard CLAP, VST3 and VST2 folders are scanned · </span>
                add your own in{' '}
                <SettingsLink page="plugins" /> · rescan ⟳ in the command bar
              </>
            )}
          </span>
        </Show>
        {/* Params drawer (accordion). Mounted whenever a plugin is loaded (so onParamChanged tracks live
            positions even while collapsed); open/close is CSS-only, so it never remounts on toggle. */}
        <Show keyed when={!chained && slotPlugins()[slotIdx]}>
          {(_desc) => (
            <div
              class="slot__drawer"
              classList={{ 'slot__drawer--open': paramsOpen() }}
              id={drawerId}
              role="region"
              aria-label={`Parameters for slot ${letter}`}
              onClick={(e) => e.stopPropagation()}
            >
              <PluginParams slot={slotIdx} />
            </div>
          )}
        </Show>
      </Show>
    </div>
  );
}

/** "Audio Settings" in the slot's note, opening the panel on the page the note is about (the slot's own
 * click, which picks the slot, does not take it). */
function SettingsLink(props: { page: SettingsPage }) {
  return (
    <button
      type="button"
      class="lf-link"
      onClick={(e) => {
        e.stopPropagation();
        openSettings(props.page);
      }}
    >
      Audio Settings
    </button>
  );
}
