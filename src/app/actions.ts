import { activeSlot, slotOff, slotPlugins } from '../ui/state/instrument';
import { liveInPlay } from '../ui/state/native-io';
import { sendEngine, type EngineAction } from '../platform';
import { pressGoLive } from '../ui/instrument/live';
import { nextStageView, toggleStage } from '../ui/stage/stage-store';
import { clock, looper } from '../ui/state/audio';
import { dismissLaneCue, refuseOnLane, tapGate } from '../ui/looper/gates';

/**
 * OWNS: the named actions a hands-free press can reach, in one table. The transport keys
 * (`transport-keys.ts`) dispatch through it; learned MIDI runs natively from a port of this table
 * (`src-tauri/src/engine_io/midi/actions.rs`), and the rows the UI owns reach it back through
 * `midi-actions.ts` (`runNativeAction`, `nativePressed`). Each action is an on-screen control's: the lane
 * core, ▶/■, ↶ UNDO, CLR, MUTE, ↺ REV, ⧉ COPY, ↔ WIDE and ✂ TRIM (halve: the first half); the command bar's ▶/■
 * ALL, FADE, TAP, CLICK, END STOP, FIXED, RETAKE, AUTO REC and IN FX's three sends; the slot's GO LIVE,
 * the stage view's cap and its view switch.
 *
 * A lane action (`LANE`) acts on a `Target`: the SELECTED track, or a named one. A press on a named
 * track leaves the selection alone, except REC/DUB, which selects its track so the transport keys
 * follow the take. Every lane action and the engine's global ones (`ENGINE_GLOBAL`: NEXT/PREV TRACK,
 * ▶/■ ALL, FADE and the toggles CLICK, END STOP, FIXED, RETAKE, AUTO REC and the three sends) go to the
 * engine as its `Action`: on the lane the engine has selected when the press lands (never the UI's copy
 * of the selection, which a feed frame may not have refreshed yet), or `ActionOn` a named track. The
 * engine gates them, confirms CLEAR per lane (one arm for the lane's CLR, the key and a pedal alike, on
 * the feed as the lane's `clearArmed`), resolves HOLD's lane, switches a toggle from the value it
 * has then (so a pedal and a click never undo each other) and names a refusal on the feed
 * (`src/app/boot.ts` puts it on the lane). The rows the UI owns (`GLOBAL`) run their control's facade
 * call, which sends the control's own command, after a `Press` that tells the engine a looper press came
 * (a pending pedal CLEAR is not confirmed past it); a refused TAP says why on the selected lane, where
 * the player is looking (gates.ts `refuseOnLane`).
 */
type LaneActionId = 'recDub' | 'playStop' | 'undo' | 'clear' | 'mute' | 'reverse' | 'copy' | 'copyWide' | 'halveTrack';
/** The global actions the engine runs as its own. */
type EngineGlobalId =
  | 'nextTrack'
  | 'prevTrack'
  | 'playAll'
  | 'stopAll'
  | 'fadeAll'
  | 'clickToggle'
  | 'endStopToggle'
  | 'fixedToggle'
  | 'retakeToggle'
  | 'autoRecToggle'
  | 'inFxEcho'
  | 'inFxReverb'
  | 'inFxRing';
/** The global actions the UI owns: native MIDI hands them back (`runNativeAction`). */
type UiActionId = 'goLive' | 'stageView' | 'stageNextView' | 'tapTempo';
export type ActionId = LaneActionId | EngineGlobalId | UiActionId;

/** A lane action's track: 0-based, or null for the selected track. */
export type Target = number | null;

/** Each action's name on screen, in the MIDI learn picker's order (lane actions first); Help's pedal keys
 * read it too. */
export const ACTION_LABELS: Readonly<Record<ActionId, string>> = {
  recDub: 'Record / overdub',
  playStop: 'Play / stop',
  undo: 'Undo / redo',
  clear: 'Clear (press twice)',
  mute: 'Mute / unmute',
  reverse: 'Reverse / forward',
  copy: 'Copy to an empty track',
  copyWide: 'Copy wide to an empty track',
  halveTrack: 'Halve track (keep first half)',
  nextTrack: 'Next track',
  prevTrack: 'Previous track',
  playAll: 'Play all',
  stopAll: 'Stop all',
  fadeAll: 'Fade out all',
  goLive: 'Go live',
  stageView: 'Stage view',
  stageNextView: 'Stage view: next look',
  tapTempo: 'Tap tempo',
  clickToggle: 'Click on / off',
  endStopToggle: 'End stop on / off',
  fixedToggle: 'Fixed length on / off',
  retakeToggle: 'Retake on / off',
  autoRecToggle: 'Auto record on / off',
  inFxEcho: 'Input echo on / off',
  inFxReverb: 'Input reverb on / off',
  inFxRing: 'Input ring mod on / off',
};

/** The lane actions, as the engine's. A row added here is a lane action everywhere: it takes a target in
 * the learn row. */
const LANE: Readonly<Record<LaneActionId, EngineAction>> = {
  recDub: 'RecDub',
  playStop: 'PlayStop',
  undo: 'Undo', // a second press redoes, as ↶ UNDO does
  clear: 'Clear',
  mute: 'Mute',
  reverse: 'Reverse',
  copy: 'Copy',
  copyWide: 'CopyWide',
  // TRIM to the first half of the loop's bars, rounded down (one UNDO away): the engine's `Halve` judges
  // the bars as the loop stands when the press lands.
  halveTrack: 'Halve',
};

export function isLaneAction(id: ActionId): id is LaneActionId {
  return Object.hasOwn(LANE, id);
}

/** GO LIVE's slot: the active one, unless only the other slot has GO LIVE in play (`liveInPlay`: a tone
 * reload holds it live while its plugin is briefly gone, or a press waits for its op) or a source made
 * for input — an effect plugin (the amp-sim a guitarist goes live on while the active slot plays a synth
 * layer) or Off (raw input). */
function goLiveSlot(): 0 | 1 {
  const active = activeSlot();
  const other = active === 0 ? 1 : 0;
  const takesInput = (s: 0 | 1) =>
    liveInPlay(s) || (slotPlugins()[s] ? slotPlugins()[s]?.isEffect === true : slotOff()[s]);
  return !takesInput(active) && takesInput(other) ? other : active;
}

/** The global rows the UI owns: their control's facade call. */
const GLOBAL: Readonly<Record<UiActionId, () => void>> = {
  goLive: () => void pressGoLive(goLiveSlot()),
  stageView: toggleStage,
  // Steps the stage view's look; does nothing while that view is closed (stage-store.ts).
  stageNextView: nextStageView,
  tapTempo: () => {
    const g = tapGate();
    if (g.ok) clock.tap();
    else refuseOnLane(looper.selectedTrack(), g.reason);
  },
};

const isUiAction = (id: ActionId): id is UiActionId => Object.hasOwn(GLOBAL, id);

/** The global actions the engine runs as its own hands-free actions. */
const ENGINE_GLOBAL: Readonly<Record<EngineGlobalId, EngineAction>> = {
  nextTrack: 'NextTrack',
  prevTrack: 'PrevTrack',
  playAll: 'PlayAll',
  stopAll: 'StopAll',
  fadeAll: 'FadeAll',
  clickToggle: { Toggle: 'Click' },
  endStopToggle: { Toggle: 'EndStop' },
  fixedToggle: { Toggle: 'Fixed' },
  retakeToggle: { Toggle: 'Retake' },
  autoRecToggle: { Toggle: 'AutoRec' },
  inFxEcho: { Toggle: { Send: 'echo' } },
  inFxReverb: { Toggle: { Send: 'reverb' } },
  inFxRing: { Toggle: { Send: 'ring' } },
};

/** The stage view's own controls. Not looper presses: the engine never hears them, and a pending CLEAR
 * and a lane cue outlive them. */
const STAGE_ACTIONS: ReadonlySet<ActionId> = new Set<ActionId>(['stageView', 'stageNextView']);

/** Every looper press passes here first: it takes the last lane cue down (a newer press makes its reason
 * stale). */
function onPress(): void {
  dismissLaneCue();
}

/** Run action `id`, a lane action on `target`. The stage view's own actions (`STAGE_ACTIONS`) are not
 * looper presses. */
export function runAction(id: ActionId, target: Target = null): void {
  const press = !STAGE_ACTIONS.has(id);
  if (press) onPress();
  if (isLaneAction(id)) {
    if (target === null) {
      sendEngine({ Action: LANE[id] });
      return;
    }
    if (id === 'recDub') looper.selectTrack(target);
    sendEngine({ ActionOn: [target, LANE[id]] });
    return;
  }
  if (isUiAction(id)) {
    if (press) sendEngine('Press');
    GLOBAL[id]();
    return;
  }
  sendEngine({ Action: ENGINE_GLOBAL[id] });
}

/** A learned MIDI binding fired a looper press natively (native MIDI sent its `Press` or action): the
 * press's bookkeeping here, as a click's (`onPress`). Native MIDI judges CLEAR's confirmation itself. */
export function nativePressed(): void {
  onPress();
}

/** A learned MIDI binding fired an action the UI owns (GO LIVE, TAP, the stage view and its next look):
 * its row, with no second `Press` (native MIDI sent it, and `nativePressed` ran first). */
export function runNativeAction(id: UiActionId): void {
  GLOBAL[id]();
}

/** Select track `i` outright (the digit keys). Not a table row, since it names its track, but a looper
 * press all the same. */
export function selectTrack(i: number): void {
  onPress();
  looper.selectTrack(i);
}
