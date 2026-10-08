import { activeSlot, slotOff, slotPlugins } from '../ui/state/instrument';
import { liveInPlay } from '../ui/state/native-io';
import { sendEngine, type EngineAction } from '../platform';
import { pressGoLive } from '../ui/instrument/live';
import { nextStageView, toggleStage } from '../ui/stage/stage-store';
import { clock, looper } from '../ui/state/audio';
import { engineFade, toggleSetting } from '../ui/state/engine-store';
import { CONFIRM_WINDOW_MS } from '../ui/looper/shared';
import {
  CONFIRM_CLEAR_TEXT,
  clearGate,
  copyGate,
  dismissLaneCue,
  fadeGate,
  muteGate,
  playStopGate,
  recDubGate,
  refuseOnLane,
  reverseGate,
  tapGate,
  trimGate,
  undoGate,
  type Gate,
} from '../ui/looper/gates';

/**
 * OWNS: the named actions a hands-free press can reach, in one table. The transport keys
 * (`transport-keys.ts`) and MIDI learn (`midi-actions.ts`) dispatch through it. Each row runs the path
 * its on-screen control runs: the lane core, ▶/■, ↶ UNDO, CLR, MUTE, ↺ REV, ⧉ COPY and ✂ TRIM (halve:
 * the first half); the command bar's ▶/■ ALL, FADE, TAP, CLICK, END STOP, FIXED, RETAKE, AUTO REC and IN
 * FX's three sends; the slot's GO LIVE, the stage view's cap and its view switch.
 *
 * A lane action (`LANE`) acts on a `Target`: the SELECTED track, or a named one. A press on a named
 * track leaves the selection alone, except REC/DUB, which selects its track so the transport keys
 * follow the take. A refused press says why on its lane (gates.ts `refuseOnLane`) instead of doing
 * nothing; a refused global one says it on the selected lane, where the player is looking. In engine
 * mode every lane row, NEXT/PREV TRACK, ▶/■ ALL, FADE and the toggles (CLICK, END STOP, FIXED, RETAKE,
 * AUTO REC, the three sends) go to the engine as its `Action`: on the lane the engine has selected when
 * the press lands (never the UI's copy of the selection, which a feed frame may not have refreshed yet),
 * or `ActionOn` a named track. The engine gates them, confirms CLEAR per lane, resolves HOLD's lane,
 * switches a toggle from the value it has then (so a pedal and a click never undo each other) and names
 * a refusal on the feed (`src/app/boot.ts` puts it on the lane). The other global rows run their
 * control's facade call, which sends the control's own command, after a `Press` that tells the engine a
 * looper press came (a pending pedal CLEAR is not confirmed past it).
 */
type LaneActionId = 'recDub' | 'playStop' | 'undo' | 'clear' | 'mute' | 'reverse' | 'copy' | 'halveTrack';
type GlobalActionId =
  | 'nextTrack'
  | 'prevTrack'
  | 'playAll'
  | 'stopAll'
  | 'fadeAll'
  | 'goLive'
  | 'stageView'
  | 'stageNextView'
  | 'tapTempo'
  | 'clickToggle'
  | 'endStopToggle'
  | 'fixedToggle'
  | 'retakeToggle'
  | 'autoRecToggle'
  | 'inFxEcho'
  | 'inFxReverb'
  | 'inFxRing';
export type ActionId = LaneActionId | GlobalActionId;

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

// CLEAR's guard. A take is irreversible, so the first press only arms and says so on the lane; the
// confirming press must be the very next looper press (a named action or a digit select, see `onPress`),
// on the same track, inside the window the lane's CLR latch uses. A double press, not a hold: the keys
// drop e.repeat, and a footswitch may send no repeat.
let clearArmed: { track: number; at: number } | null = null;

function clearTrack(i: number): void {
  if (clearArmed?.track === i && performance.now() - clearArmed.at < CONFIRM_WINDOW_MS) {
    clearArmed = null;
    looper.clear(i);
    return;
  }
  clearArmed = { track: i, at: performance.now() };
  refuseOnLane(i, CONFIRM_CLEAR_TEXT, CONFIRM_WINDOW_MS);
}

/** A lane action: its control's gate and call on lane `i`, and the engine's action when it has one. */
interface LaneRow {
  gate: (i: number) => Gate;
  act: (i: number) => void;
  engine?: EngineAction;
}

/** The lane actions. A row added here is a lane action everywhere: it takes a target in the learn row. */
const LANE: Readonly<Record<LaneActionId, LaneRow>> = {
  recDub: { gate: recDubGate, act: (i) => void looper.recDub(i), engine: 'RecDub' },
  playStop: { gate: playStopGate, act: (i) => looper.playStop(i), engine: 'PlayStop' },
  undo: { gate: undoGate, act: (i) => looper.undoLastOverdub(i), engine: 'Undo' }, // a second press redoes, as ↶ UNDO does
  clear: { gate: clearGate, act: clearTrack, engine: 'Clear' },
  mute: { gate: muteGate, act: (i) => looper.toggleMute(i), engine: 'Mute' },
  reverse: { gate: reverseGate, act: (i) => looper.reverse(i), engine: 'Reverse' },
  copy: { gate: copyGate, act: (i) => void looper.copy(i), engine: 'Copy' },
  // TRIM to the first half of the loop's bars, rounded down (one UNDO away): the engine's `Halve` judges
  // the bars as the loop stands when the press lands.
  halveTrack: { gate: trimGate, act: () => {}, engine: 'Halve' },
};

export function isLaneAction(id: ActionId): id is LaneActionId {
  return Object.hasOwn(LANE, id);
}

/** Is `v` a named track a lane action can aim at? */
export function isTrack(v: unknown): v is number {
  return Number.isInteger(v) && (v as number) >= 0 && (v as number) < looper.trackCount;
}

/** Step the selected track by `d`, wrapping at both ends. */
const step = (d: number) => (): void =>
  looper.selectTrack((looper.selectedTrack() + d + looper.trackCount) % looper.trackCount);

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

/** Run `act` when `gate` lets a global press through, else say why on the selected lane. */
const gated =
  (gate: () => Gate, act: () => void) =>
  (): void => {
    const g = gate();
    if (g.ok) act();
    else refuseOnLane(looper.selectedTrack(), g.reason);
  };

const GLOBAL: Readonly<Record<GlobalActionId, () => void>> = {
  nextTrack: step(1),
  prevTrack: step(-1),
  playAll: () => looper.playAll(),
  stopAll: () => looper.stopAll(),
  // The engine runs FADE as its own action (ENGINE_GLOBAL); this row is its UI path.
  fadeAll: gated(fadeGate, () => engineFade.fadeAll()),
  goLive: () => void pressGoLive(goLiveSlot()),
  stageView: toggleStage,
  // Steps the stage view's look; does nothing while that view is closed (stage-store.ts).
  stageNextView: nextStageView,
  tapTempo: gated(tapGate, () => clock.tap()),
  // The toggles are the engine's (ENGINE_GLOBAL); these rows are their controls' path, the same toggle.
  clickToggle: () => toggleSetting('Click'),
  endStopToggle: () => toggleSetting('EndStop'),
  fixedToggle: () => toggleSetting('Fixed'),
  retakeToggle: () => toggleSetting('Retake'),
  autoRecToggle: () => toggleSetting('AutoRec'),
  inFxEcho: () => toggleSetting({ Send: 'echo' }),
  inFxReverb: () => toggleSetting({ Send: 'reverb' }),
  inFxRing: () => toggleSetting({ Send: 'ring' }),
};

/** The global rows the engine runs as its own hands-free actions. */
const ENGINE_GLOBAL: Readonly<Partial<Record<GlobalActionId, EngineAction>>> = {
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

/** Every looper press passes here first: any press but CLEAR disarms a pending CLEAR, and every press
 * takes the last lane cue down (a newer press makes its reason stale). */
function onPress(id?: ActionId): void {
  if (id !== 'clear') clearArmed = null;
  dismissLaneCue();
}

/** Lane action `id` on lane `i`. `named`: the press named its track, so the engine is told the lane
 * rather than reading its own selection. */
function runOnLane(id: LaneActionId, i: number, named: boolean): void {
  const row = LANE[id];
  if (row.engine) {
    sendEngine(named ? { ActionOn: [i, row.engine] } : { Action: row.engine });
    return;
  }
  const g = row.gate(i);
  if (g.ok) row.act(i);
  else refuseOnLane(i, g.reason);
}

/** Run action `id`, a lane action on `target`. The stage view's own actions (`STAGE_ACTIONS`) are not
 * looper presses. */
export function runAction(id: ActionId, target: Target = null): void {
  const press = !STAGE_ACTIONS.has(id);
  if (press) onPress(id);
  if (isLaneAction(id)) {
    if (target === null) {
      runOnLane(id, looper.selectedTrack(), false);
      return;
    }
    if (id === 'recDub') looper.selectTrack(target);
    runOnLane(id, target, true);
    return;
  }
  const action = ENGINE_GLOBAL[id];
  if (action) {
    sendEngine({ Action: action });
    return;
  }
  if (press) sendEngine('Press');
  GLOBAL[id]();
}

/** HOLD's press (`midi-actions.ts`): REC/DUB on `target`, as the engine's `Hold` by `control`, the
 * pedal's number while it is down: the engine remembers the lane an accepted press acted on for that
 * control's release, and nothing for a refused one. A named track is selected, as REC/DUB selects it. */
export function pressHold(target: Target, control: number): void {
  onPress('recDub');
  if (target === null) {
    sendEngine({ Action: { Hold: control } });
    return;
  }
  looper.selectTrack(target);
  sendEngine({ ActionOn: [target, { Hold: control }] });
}

/** HOLD's release: end the capture its press started, while that lane still captures. A take that closed
 * itself meanwhile (FIXED) stays closed instead of starting an overdub. The engine judges it on its own
 * state, on the lane its `Hold` by `control` acted on (none after a refused press). */
export function releaseHold(control: number): void {
  sendEngine({ Action: { Release: control } });
}

/** Select track `i` outright (the digit keys). Not a table row, since it names its track, but a looper
 * press all the same. */
export function selectTrack(i: number): void {
  onPress();
  looper.selectTrack(i);
}
