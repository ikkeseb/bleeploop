import { createSignal } from 'solid-js';
import {
  platform,
  type LearnPick,
  type LearnRefusal,
  type ListedBinding,
  type MidiActionId,
  type MidiBinding,
  type MidiEvent,
  type MidiPort,
  type MidiUiAction,
  type StoreProblem,
} from '../../platform';
import { notifyError } from '../../notify';

/**
 * OWNS: native MIDI as the UI shows it, built from native MIDI's events (`platform.midi`): the input
 * ports, the stored bindings and how each stands, MIDI learn's state, the latest learned binding waiting
 * for its release, the notes held down from every source (the on-screen keyboard lights them), and the
 * toasts for an unplugged port and a store that cannot save. MIDI itself, ports, parse, learn, the
 * bindings and the one note router, is native (`src-tauri/src/engine_io/midi/mod.rs`); the calls below
 * only ask it. A binding's press that the UI runs (GO LIVE, TAP, the stage view) and the press's
 * bookkeeping reach `src/app/midi-actions.ts` through `setMidiActionHandler` (`src/ui/state/` never
 * imports `src/app/`).
 */

const [ports, setPorts] = createSignal<readonly MidiPort[]>([]);
/** The input ports present now, in the system's order. */
export { ports };

const [bindings, setBindings] = createSignal<readonly ListedBinding[]>([]);
/** Every stored binding, in list order (the index the edits below take), and how it stands. */
export { bindings };

const [learning, setLearning] = createSignal<LearnPick | null>(null);
/** What the next CC or note-on will be learned onto, or null. Set at once by `learn` and `cancelLearn`
 * (so Esc ends LISTENING without a round trip), then by native MIDI's `learning` events. */
export { learning };

const [awaitingRelease, setAwaitingRelease] = createSignal<MidiBinding | null>(null);
/** The latest learned binding still waiting for its learning press's release, or null (the learn row's
 * hint; native MIDI's timer ends the wait). */
export { awaitingRelease };

const [heldNotes, setHeldNotes] = createSignal<ReadonlySet<number>>(new Set());
/** The notes held down now, by any source (not the ones a pedal sustains): native MIDI's held set. */
export { heldNotes };

/** A port can learn: at least one is open. */
export const anyPortOpen = (): boolean => ports().some((p) => p.state === 'open');

/** The device list as the diagnostics name it: each port, and when it is not open, why. */
export function portsSummary(): string {
  const list = ports();
  if (list.length === 0) return 'no devices';
  return list
    .map((p) => (p.state === 'open' ? p.name : p.state === 'busy' ? `${p.name} (held by another program)` : `${p.name} (closed)`))
    .join(', ');
}

/** What the app layer does with a binding's press: `run` an action the UI owns, `pressed` for every
 * looper press (before its `run`), `refused` when MIDI learn consumed a press and ran nothing. */
export interface MidiActionHandler {
  run(action: MidiUiAction): void;
  pressed(): void;
  refused(reason: LearnRefusal): void;
}

let handler: MidiActionHandler | null = null;

/** Install the app layer's handler (`null` removes it). One at a time: `src/app/midi-actions.ts`. */
export function setMidiActionHandler(h: MidiActionHandler | null): void {
  handler = h;
}

function storeProblemText(problem: StoreProblem): [string, string] {
  if ('readOnly' in problem) return ['MIDI bindings cannot be saved this session', problem.readOnly.why];
  if ('conflict' in problem) return ['MIDI bindings were changed by another BleepLoop; this session does not save over them', problem.conflict.why];
  if ('failed' in problem) return ['MIDI bindings could not be saved; the next change tries again', problem.failed.why];
  const n = problem.rejected.count;
  return [`${n} stored MIDI binding${n === 1 ? '' : 's'} could not be read`, 'They stay in the file, unused.'];
}

function onEvent(ev: MidiEvent): void {
  switch (ev.type) {
    case 'ports':
      setPorts(ev.ports);
      break;
    case 'gone':
      // Native MIDI released what the port held; the player should know why the notes stopped.
      for (const name of ev.names) {
        console.error(`[midi] input disconnected: ${name}`);
        notifyError(`MIDI device disconnected — ${name}`, 'Held notes were released.');
      }
      break;
    case 'bindings':
      setBindings(ev.bindings);
      break;
    case 'learning':
      setLearning(ev.learning);
      break;
    case 'awaitingRelease':
      setAwaitingRelease(ev.binding);
      break;
    case 'learned':
      // The `bindings` and `awaitingRelease` events that follow show it.
      break;
    case 'refused':
      handler?.refused(ev.reason);
      break;
    case 'run':
      handler?.run(ev.action);
      break;
    case 'pressed':
      handler?.pressed();
      break;
    case 'store': {
      const [message, detail] = storeProblemText(ev.problem);
      console.error(`[midi] bindings store: ${message}: ${detail}`);
      notifyError(message, detail);
      break;
    }
    case 'held':
      setHeldNotes(new Set(ev.notes));
      break;
  }
}

/** Listen to native MIDI; its first events bring the state. Returns the stop. */
export function startMidi(): () => void {
  return platform.midi.subscribe(onEvent);
}

function failed(what: string, err: unknown): void {
  console.error(`[midi] ${what} failed`, err);
  notifyError(`MIDI ${what} failed`, err);
}

/** Learn the next CC or note-on, from any port, onto `action` (a lane action on track `target`, null the
 * selected track; a global action takes none). */
export function learn(action: MidiActionId, target: number | null): void {
  setLearning({ action, target });
  platform.midi.learn(action, target).catch((err: unknown) => {
    setLearning(null);
    failed('learn', err);
  });
}

/** Stop listening. True when a learn was pending (Esc spends itself on it); the UI stops listening at once,
 * native MIDI a call later. A learned pedal's wait for its release goes on. */
export function cancelLearn(): boolean {
  if (learning() === null) return false;
  setLearning(null);
  platform.midi.cancelLearn().catch((err: unknown) => failed('cancel learn', err));
  return true;
}

/** Drop listed binding `index`: its messages reach the play path again. */
export function forget(index: number): void {
  platform.midi.forget(index).catch((err: unknown) => failed('forget', err));
}

/** Read listed binding `index`'s pedal as momentary or latching (the list's switch). */
export function setMomentary(index: number, momentary: boolean): void {
  platform.midi.setMomentary(index, momentary).catch((err: unknown) => failed('pedal switch', err));
}

/** HOLD on or off for listed binding `index`. */
export function setHold(index: number, hold: boolean): void {
  platform.midi.setHold(index, hold).catch((err: unknown) => failed('HOLD switch', err));
}

/** Run listed binding `index` on the present port `portId` from now on. */
export function assign(index: number, portId: string): void {
  platform.midi.assign(index, portId).catch((err: unknown) => failed('assign', err));
}
