import { looper } from '../ui/state/audio';
import { setMidiActionHandler, startMidi } from '../ui/state/midi';
import { refuseOnLane } from '../ui/looper/gates';
import { platform, type MidiActionId } from '../platform';
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
 * it, once the native store has written them.
 */

// The two action tables are one: native MIDI's ids are `actions.ts`'s (a drift fails the typecheck here).
const sameIds: [ActionId] extends [MidiActionId] ? ([MidiActionId] extends [ActionId] ? true : never) : never = true;
void sameIds;

const LEGACY_KEY = 'lf.midiLearn';

function importLegacy(): void {
  let json = '[]';
  try {
    json = localStorage.getItem(LEGACY_KEY) ?? '[]';
  } catch {
    /* unreadable storage: nothing to import */
  }
  platform.midi.importLegacy(json).then(
    (report) => {
      if (report.already) return;
      if (report.unreadable !== null) console.error(`[midi] the stored web bindings were unreadable: ${report.unreadable}`);
      for (const r of report.rejected) console.error(`[midi] stored web binding ${r.index} not imported: ${r.reason}`);
    },
    (err: unknown) => console.error('[midi] importing the stored web bindings failed', err),
  );
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
