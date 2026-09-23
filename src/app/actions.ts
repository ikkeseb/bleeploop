import { activeSlot, slotPlugins } from '../audio/instrument';
import { looper } from '../audio/looper/looper';
import { pressGoLive } from '../ui/instrument/PluginControls';
import { CONFIRM_WINDOW_MS } from '../ui/looper/shared';
import {
  clearGate,
  dismissLaneCue,
  playStopGate,
  recDubGate,
  refuseOnLane,
  undoGate,
  type Gate,
} from '../ui/looper/gates';

/**
 * OWNS: the named actions a hands-free press can reach, in one table. The transport keys dispatch
 * through it (`transport-keys.ts`), and MIDI learn will (`docs/plans/pedalboard.md` piece 3). Each row
 * runs the path its on-screen control runs: the lane core, ▶/■, ↶ DUB, CLR, the command bar's ▶/■ ALL
 * and the slot's GO LIVE. The lane actions act on the SELECTED track, and a refused one says why on that
 * lane (gates.ts `refuseOnLane`) instead of doing nothing.
 */
export type ActionId =
  | 'recDub'
  | 'playStop'
  | 'undo'
  | 'clear'
  | 'nextTrack'
  | 'prevTrack'
  | 'playAll'
  | 'stopAll'
  | 'goLive';

/** Act on the selected lane when `gate` lets the press through, else show why on that lane. */
const onSelected =
  (gate: (i: number) => Gate, act: (i: number) => void) =>
  (): void => {
    const i = looper.selectedTrack();
    const g = gate(i);
    if (g.ok) act(i);
    else refuseOnLane(i, g.reason);
  };

// CLEAR's guard. A take is irreversible, so the first press only arms and says so on the lane; the
// confirming press must be the very next action, on the same track, inside the window the lane's CLR
// latch uses. A double press, not a hold: the keys drop e.repeat, and a footswitch may send no repeat.
let clearArmed: { track: number; at: number } | null = null;

function clearTrack(i: number): void {
  if (clearArmed?.track === i && performance.now() - clearArmed.at < CONFIRM_WINDOW_MS) {
    clearArmed = null;
    looper.clear(i);
    return;
  }
  clearArmed = { track: i, at: performance.now() };
  refuseOnLane(i, 'press again to clear', CONFIRM_WINDOW_MS);
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

const ACTIONS: Readonly<Record<ActionId, () => void>> = {
  recDub: onSelected(recDubGate, () => looper.recDubSelected()),
  playStop: onSelected(playStopGate, () => looper.playStopSelected()),
  undo: onSelected(undoGate, (i) => looper.undoLastOverdub(i)), // a second press redoes, as ↶ DUB does
  clear: onSelected(clearGate, clearTrack),
  nextTrack: step(1),
  prevTrack: step(-1),
  playAll: () => looper.playAll(),
  stopAll: () => looper.stopAll(),
  goLive: () => void pressGoLive(goLiveSlot()),
};

/** Run action `id`. Any other action disarms a pending CLEAR, and every press takes the last lane cue
 * down (a newer press makes its reason stale). */
export function runAction(id: ActionId): void {
  if (id !== 'clear') clearArmed = null;
  dismissLaneCue();
  ACTIONS[id]();
}
