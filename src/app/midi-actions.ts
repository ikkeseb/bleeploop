import { looper } from '../ui/state/audio';
import { importLegacyBindings, setMidiActionHandler, startMidi } from '../ui/state/midi';
import { refuseOnLane } from '../ui/looper/gates';
import type { ImportReport, MidiActionId } from '../platform';
import { notifyError } from '../notify';
import { nativePressed, runNativeAction, type ActionId } from './actions';

/**
 * OWNS: the bridge between native MIDI learn and the UI's actions. Learned MIDI runs natively
 * (`src-tauri/src/engine_io/midi/`: ports, learn, the bindings and their store, the router, what a
 * binding fires); the learn row and the list call it through `src/ui/state/midi.ts`. What a binding fires
 * comes back here: a looper press's bookkeeping (`nativePressed`: the lane cue goes, the UI's CLEAR guard
 * disarms), the actions the UI owns (`runNativeAction`: GO LIVE, TAP, the stage view; native MIDI already
 * sent their `Press`), and a refused HOLD press's cue on the selected lane.
 *
 * At every start the bindings the web build kept (`lf.midiLearn`) go to native MIDI, which imports them
 * once and answers `already` after (the plan's decision 9); the key stays until a later release removes
 * it, once the native store has written them. A launch that cannot read the key hands nothing over (an
 * empty list would mark the import done for good), and the import's one answer is told: what waits for a
 * port picked in Audio Settings, and what could not be read.
 */

// The two action tables are one: native MIDI's ids are `actions.ts`'s (a drift fails the typecheck here).
const sameIds: [ActionId] extends [MidiActionId] ? ([MidiActionId] extends [ActionId] ? true : never) : never = true;
void sameIds;

const LEGACY_KEY = 'lf.midiLearn';

const plural = (n: number, one: string, many: string): string => `${n} ${n === 1 ? one : many}`;

/** The import's answer, told once: it runs once, and answers `already` after (null: this page is no
 * longer the current one, nothing ran). */
function tellImport(report: ImportReport | null): void {
  if (report === null || report.already) return;
  if (report.unreadable !== null) console.error(`[midi] the stored web bindings were unreadable: ${report.unreadable}`);
  for (const r of report.rejected) console.error(`[midi] stored web binding ${r.index} not imported: ${r.reason}`);
  if (report.blocked.length > 0) {
    const waiting = `${plural(report.blocked.length, 'MIDI binding', 'MIDI bindings')} from the previous version need a port picked in Audio Settings`;
    console.error(`[midi] ${waiting}`);
    notifyError(waiting, 'Until then they run nothing.');
  }
  if (report.unreadable !== null || report.rejected.length > 0) {
    const what = report.unreadable !== null ? 'The MIDI bindings' : plural(report.rejected.length, 'MIDI binding', 'MIDI bindings');
    notifyError(`${what} from the previous version could not be read`, 'Learn them again in Audio Settings.');
  }
}

/** The previous version's bindings did not reach native MIDI this launch (the next one tries again). */
function importFailed(log: string, err: unknown): void {
  console.error(log, err);
  notifyError('Could not read the MIDI bindings from the previous version', err);
}

function importLegacy(): void {
  let json: string;
  try {
    json = localStorage.getItem(LEGACY_KEY) ?? '[]';
  } catch (err) {
    // Not an empty list: importing one would mark the import done for good. The next launch tries again.
    importFailed('[midi] the stored web bindings could not be read; none handed over this launch', err);
    return;
  }
  importLegacyBindings(json).then(tellImport, (err: unknown) => importFailed('[midi] importing the stored web bindings failed', err));
}

/** Listen to native MIDI, run what its bindings fire, and hand it the web's bindings once. Returns the
 * stop. */
export function installMidiActions(): () => void {
  setMidiActionHandler({
    run: runNativeAction,
    pressed: nativePressed,
    refused: () => refuseOnLane(looper.selectedTrack(), 'every HOLD control is down, let a HOLD pedal go first'),
  });
  const stop = startMidi();
  importLegacy();
  return () => {
    stop();
    setMidiActionHandler(null);
  };
}
