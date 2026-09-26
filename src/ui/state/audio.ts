import { looper as webLooper } from '../../audio/looper/looper';
import { clock as webClock } from '../../audio/clock';
import { master as webMaster } from '../../audio/master';
import { engine } from '../../audio/engine';
import { webSession, type SessionSource } from '../../audio/export/session-source';
import { engineMode } from '../../platform';
import { engineClock, engineLooper, engineMaster, engineSampleRate, engineSession } from './engine-store';

/**
 * OWNS: which audio implementation the UI talks to — the web modules or engine mode's store
 * (`engine-store.ts`), picked per access by `engineMode()`, which is settled before the app renders and
 * never changes while it runs. Components import `looper`, `clock`, `master` and `sampleRate` from
 * here, and hand `session` to export, import and recovery; `__lf` and the verify ports keep the web
 * modules.
 */

export type { PeakView, TrackState } from '../../audio/looper/looper';

/** What the UI reads of the looper: the web facade's members that the engine store also provides
 * (two of the web's are raw Solid setters; the UI only ever passes them a boolean). */
type LooperView = {
  setLoopEndStopEnabled(on: boolean): void;
  setRetakeEnabled(on: boolean): void;
} & Pick<
  typeof webLooper,
  | 'trackCount'
  | 'recDub'
  | 'playStop'
  | 'stop'
  | 'undoLastOverdub'
  | 'reverse'
  | 'copy'
  | 'clear'
  | 'stopAll'
  | 'playAll'
  | 'clearAll'
  | 'loopEndStopEnabled'
  | 'retakeEnabled'
  | 'selectedTrack'
  | 'selectTrack'
  | 'recDubSelected'
  | 'playStopSelected'
  | 'track'
  | 'trackInfo'
  | 'masterLengthFrames'
  | 'inputArmed'
  | 'toggleInput'
  | 'inputArmRequested'
  | 'peaksInto'
  | 'phaseValue'
  | 'levelValue'
  | 'stateOf'
  | 'mutedOf'
  | 'waitingOf'
  | 'recHeadFrac'
  | 'masterFramesValue'
  | 'fxState'
  | 'setFxBypass'
  | 'setFxParam'
  | 'setVolume'
  | 'setMute'
  | 'trackVolume'
  | 'trackMuted'
  | 'fixedLengthEnabled'
  | 'setFixedLengthEnabled'
  | 'fixedLengthBars'
  | 'nextTakeMaxBars'
  | 'setFixedLengthBars'
  | 'autoRecordEnabled'
  | 'setAutoRecordEnabled'
  | 'autoRecordSensitivity'
  | 'setAutoRecordSensitivity'
>;

type ClockView = Pick<
  typeof webClock,
  | 'bpm'
  | 'setBpm'
  | 'tap'
  | 'running'
  | 'bpmLocked'
  | 'beat'
  | 'countLeft'
  | 'metronomeOn'
  | 'setMetronome'
  | 'clickVolume'
  | 'setClickVolume'
>;

type MasterView = Pick<typeof webMaster, 'volume' | 'setVolume' | 'muted' | 'setMuted' | 'init'>;

/** Delegate every member access to the engine side in engine mode, else to the web side. */
function byMode<T extends object>(web: T, native: T): T {
  return new Proxy(web, { get: (_web, key) => Reflect.get(engineMode() ? native : web, key) });
}

export const looper: LooperView = byMode<LooperView>(webLooper, engineLooper);
export const clock: ClockView = byMode<ClockView>(webClock, engineClock);
export const master: MasterView = byMode<MasterView>(webMaster, engineMaster);
export const session: SessionSource = byMode<SessionSource>(webSession, engineSession);

/** The audio clock's rate: the engine's device in engine mode, else the AudioContext's. */
export function sampleRate(): number {
  return engineMode() ? engineSampleRate() : engine.ctx.sampleRate;
}
