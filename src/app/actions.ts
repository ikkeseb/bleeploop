import { activeSlot, slotPlugins } from '../audio/instrument';
import { engineMode, sendEngine, type EngineAction, type InputSendId } from '../platform';
import { pressGoLive } from '../ui/instrument/PluginControls';
import { toggleStage } from '../ui/stage/stage-store';
import { clock, looper } from '../ui/state/audio';
import { engineInputSends, trimLane } from '../ui/state/engine-store';
import { CONFIRM_WINDOW_MS } from '../ui/looper/shared';
import {
  CONFIRM_CLEAR_TEXT,
  clearGate,
  copyGate,
  dismissLaneCue,
  fixedGate,
  inputFxGate,
  loopWholeBars,
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
 * the first half); the command bar's
 * ▶/■ ALL, TAP, CLICK, END STOP, FIXED and IN FX's two sends; the slot's GO LIVE and the stage view's cap.
 *
 * A lane action (`LANE`) acts on a `Target`: the SELECTED track, or a named one. A press on a named
 * track leaves the selection alone, except REC/DUB, which selects its track so the transport keys
 * follow the take. A refused press says why on its lane (gates.ts `refuseOnLane`) instead of doing
 * nothing; a refused global one says it on the selected lane, where the player is looking. In engine
 * mode the rows with an engine `Action` go to the engine, which gates them, confirms CLEAR per lane and
 * names a refusal on the feed (`src/app/boot.ts` puts it on the lane); the others run their control's
 * facade call, which sends the control's own command.
 */
type LaneActionId = 'recDub' | 'playStop' | 'undo' | 'clear' | 'mute' | 'reverse' | 'copy' | 'halveTrack';
type GlobalActionId =
  | 'nextTrack'
  | 'prevTrack'
  | 'playAll'
  | 'stopAll'
  | 'goLive'
  | 'stageView'
  | 'tapTempo'
  | 'clickToggle'
  | 'endStopToggle'
  | 'fixedToggle'
  | 'inFxEcho'
  | 'inFxReverb';
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
  goLive: 'Go live',
  stageView: 'Stage view',
  tapTempo: 'Tap tempo',
  clickToggle: 'Click on / off',
  endStopToggle: 'End stop on / off',
  fixedToggle: 'Fixed length on / off',
  inFxEcho: 'Input echo on / off',
  inFxReverb: 'Input reverb on / off',
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
  mute: { gate: muteGate, act: (i) => looper.setMute(i, !looper.trackMuted(i)) },
  reverse: { gate: reverseGate, act: (i) => looper.reverse(i) },
  copy: { gate: copyGate, act: (i) => void looper.copy(i) },
  // TRIM to the first half of the loop's bars, rounded down (one UNDO away). The web path has no TRIM.
  halveTrack: { gate: trimGate, act: (i) => trimLane(i, Math.floor(loopWholeBars() / 2)) },
};

export function isLaneAction(id: ActionId): id is LaneActionId {
  return Object.hasOwn(LANE, id);
}

/** Is `v` a named track a lane action can aim at? */
export function isTrack(v: unknown): v is number {
  return Number.isInteger(v) && (v as number) >= 0 && (v as number) < looper.trackCount;
}

/** The lane a press on `target` acts on now. */
export function targetLane(target: Target): number {
  return target ?? looper.selectedTrack();
}

/** Step the selected track by `d`, wrapping at both ends. */
const step = (d: number) => (): void =>
  looper.selectTrack((looper.selectedTrack() + d + looper.trackCount) % looper.trackCount);

/** GO LIVE's slot: the active one, unless only the other slot holds an effect plugin (the amp-sim a
 * guitarist goes live on while the active slot plays a synth layer). */
function goLiveSlot(): 0 | 1 {
  const active = activeSlot();
  const other = active === 0 ? 1 : 0;
  const isFx = (s: 0 | 1) => slotPlugins()[s]?.isEffect === true;
  return !isFx(active) && isFx(other) ? other : active;
}

/** Run `act` when `gate` lets a global press through, else say why on the selected lane. */
const gated =
  (gate: () => Gate, act: () => void) =>
  (): void => {
    const g = gate();
    if (g.ok) act();
    else refuseOnLane(looper.selectedTrack(), g.reason);
  };

const toggleSend = (id: InputSendId) => (): void => engineInputSends.setOn(id, !engineInputSends.on(id));

const GLOBAL: Readonly<Record<GlobalActionId, () => void>> = {
  nextTrack: step(1),
  prevTrack: step(-1),
  playAll: () => looper.playAll(),
  stopAll: () => looper.stopAll(),
  goLive: () => void pressGoLive(goLiveSlot()),
  stageView: toggleStage,
  tapTempo: gated(tapGate, () => clock.tap()),
  clickToggle: () => clock.setMetronome(!clock.metronomeOn()),
  endStopToggle: () => looper.setLoopEndStopEnabled(!looper.loopEndStopEnabled()),
  fixedToggle: gated(fixedGate, () => looper.setFixedLengthEnabled(!looper.fixedLengthEnabled())),
  inFxEcho: gated(inputFxGate, toggleSend('echo')),
  inFxReverb: gated(inputFxGate, toggleSend('reverb')),
};

/** The global rows the engine runs as its own hands-free actions. */
const ENGINE_GLOBAL: Readonly<Partial<Record<GlobalActionId, EngineAction>>> = {
  nextTrack: 'NextTrack',
  prevTrack: 'PrevTrack',
  playAll: 'PlayAll',
  stopAll: 'StopAll',
};

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
  if (engineMode() && row.engine) {
    sendEngine(named ? { ActionOn: [i, row.engine] } : { Action: row.engine });
    return;
  }
  const g = row.gate(i);
  if (g.ok) row.act(i);
  else refuseOnLane(i, g.reason);
}

/** Run action `id`, a lane action on `target`. Toggling the stage view is not a looper press: a pending
 * CLEAR and a lane cue outlive it, in engine mode (whose engine never hears the toggle) and web mode
 * alike. */
export function runAction(id: ActionId, target: Target = null): void {
  if (id !== 'stageView') onPress(id);
  if (isLaneAction(id)) {
    if (target === null) {
      runOnLane(id, looper.selectedTrack(), false);
      return;
    }
    if (id === 'recDub') looper.selectTrack(target);
    runOnLane(id, target, true);
    return;
  }
  const action = engineMode() ? ENGINE_GLOBAL[id] : undefined;
  if (action) sendEngine({ Action: action });
  else GLOBAL[id]();
}

/** HOLD's release (`midi-actions.ts`): REC/DUB again on lane `i`, where its press acted, while that lane
 * still captures. A take that closed itself meanwhile (FIXED) stays closed instead of starting an
 * overdub. */
export function releaseHold(i: number): void {
  const s = looper.track(i)().state;
  if (s !== 'RECORDING' && s !== 'OVERDUBBING') return;
  onPress('recDub');
  runOnLane('recDub', i, true);
}

/** Select track `i` outright (the digit keys). Not a table row, since it names its track, but a looper
 * press all the same. */
export function selectTrack(i: number): void {
  onPress();
  looper.selectTrack(i);
}
