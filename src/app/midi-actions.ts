import { createSignal } from 'solid-js';
import { releaseController, setMidiConsumer } from '../audio/midi';
import { ACTION_LABELS, isLaneAction, isTrack, releaseHold, runAction, targetLane, type ActionId, type Target } from './actions';

/**
 * OWNS: MIDI learn. A learned CC or note, on one input port and channel, runs a named action
 * (`actions.ts`), a lane action on its target track, and is consumed before the play path through
 * `setMidiConsumer` (`src/audio/midi.ts`): a CC learned onto 64 does not sustain (a pedal held while it
 * was learned is let go), and a learned note does not sound (its note-off is consumed too). Unlearned
 * traffic reaches the play path exactly as before. The bindings persist in localStorage. A port is keyed
 * by its Web MIDI input id, which the spec asks browsers to keep across restarts and replugs; its name
 * is kept for the list only.
 */

/** One learned message. `pressHigh`: the learning press sent a CC value ≥ 64, or a note-on. */
export interface MidiBinding {
  port: string;
  portName: string;
  channel: number;
  kind: 'cc' | 'note';
  number: number;
  action: ActionId;
  /** A lane action's track; always null (the selected track) for a global action. */
  target: Target;
  pressHigh: boolean;
  momentary: boolean;
  /** HOLD, on a momentary REC/DUB pedal only: its release runs REC/DUB again (`releaseHold`). */
  hold: boolean;
}

// Footswitches. A momentary pedal sends one value on press and the other on release (127, 0); a latching
// pedal sends one value per press, alternating (127, then 0 on the next press); some send the same value
// on every press. The values alone cannot tell a momentary release from a latching press, so the learn
// gesture decides, and runs nothing while it does: after the learning press the binding waits for its
// release (`awaitingRelease`). The other side seen, however long the pedal was held, reads as momentary:
// it fires on each message on the press side and swallows the release (or, with HOLD, runs REC/DUB on
// it). The same side seen again, or the learn row moving on (another LEARN, Esc, the panel closing),
// leaves it latching: it fires on every message, so a latching or same-value pedal fires once per press
// too. Level rather than edge on the press side, so a lost release message cannot swallow the next
// press. A latching pedal pressed a second time before the row moves on reads as momentary; the list
// shows the kind, and its switch fixes a wrong read.

const STORAGE_KEY = 'lf.midiLearn';

/** A persisted binding as it may come back: saved before targets and HOLD, or hand-edited. Null when
 * malformed or naming an action that no longer exists. */
function fromStored(v: unknown): MidiBinding | null {
  const b = v as Partial<MidiBinding> | null;
  const valid =
    typeof b === 'object' && b !== null &&
    typeof b.port === 'string' && typeof b.portName === 'string' &&
    Number.isInteger(b.channel) && (b.channel as number) >= 0 && (b.channel as number) < 16 &&
    (b.kind === 'cc' || b.kind === 'note') &&
    Number.isInteger(b.number) && (b.number as number) >= 0 && (b.number as number) < 128 &&
    typeof b.action === 'string' && Object.hasOwn(ACTION_LABELS, b.action) &&
    typeof b.pressHigh === 'boolean' && typeof b.momentary === 'boolean' &&
    (b.target === undefined || b.target === null || isTrack(b.target)) &&
    (b.hold === undefined || typeof b.hold === 'boolean');
  if (!valid) return null;
  const action = b.action as ActionId;
  const momentary = b.momentary as boolean;
  return {
    port: b.port as string,
    portName: b.portName as string,
    channel: b.channel as number,
    kind: b.kind as 'cc' | 'note',
    number: b.number as number,
    action,
    target: isLaneAction(action) ? (b.target ?? null) : null,
    pressHigh: b.pressHigh as boolean,
    momentary,
    hold: b.hold === true && momentary && action === 'recDub',
  };
}

/** The persisted bindings; anything malformed is dropped. */
function load(): MidiBinding[] {
  try {
    const list: unknown = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? '[]');
    return Array.isArray(list) ? list.map(fromStored).filter((b) => b !== null) : [];
  } catch {
    return [];
  }
}

const [bindings, setBindings] = createSignal<readonly MidiBinding[]>(load());
/** The learned bindings, in learn order. */
export { bindings };

const [learning, setLearning] = createSignal<{ action: ActionId; target: Target } | null>(null);
/** What the next CC or note-on will be learned onto, or null. */
export { learning };

const [awaitingRelease, setAwaitingRelease] = createSignal<MidiBinding | null>(null);
/** The binding just learned while its release may still come (Footswitches, above), or null. */
export { awaitingRelease };

// The lane each HOLD press acted on, by its control, until its release ends the capture there.
const held = new Map<string, number>();
const controlKey = (b: MidiBinding) => `${b.port}\n${b.channel}\n${b.kind}\n${b.number}`;

function save(list: readonly MidiBinding[]): void {
  setBindings(list);
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(list));
  } catch {
    /* persistence is best-effort */
  }
}

/** Replace binding `b` with `next`, in place. A wait for `b`'s release ends: its kind is settled. */
function update(b: MidiBinding, next: MidiBinding): void {
  if (awaitingRelease() === b) setAwaitingRelease(null);
  held.delete(controlKey(b));
  save(bindings().map((x) => (x === b ? next : x)));
}

/** Learn the next CC or note-on, from any port, onto `action` (a lane action on `target`). */
export function learn(action: ActionId, target: Target = null): void {
  setAwaitingRelease(null);
  setLearning({ action, target: isLaneAction(action) ? target : null });
}

/** Stop listening, and stop waiting for a learned pedal's release. True when a learn was pending (Esc
 * spends itself on it). */
export function cancelLearn(): boolean {
  const was = learning() !== null;
  setLearning(null);
  setAwaitingRelease(null);
  return was;
}

/** Drop binding `b`: its messages reach the play path again. */
export function forget(b: MidiBinding): void {
  if (awaitingRelease() === b) setAwaitingRelease(null);
  held.delete(controlKey(b));
  save(bindings().filter((x) => x !== b));
}

/** Read binding `b`'s pedal as momentary or latching (the list's switch). A latching pedal has no HOLD. */
export function setMomentary(b: MidiBinding, momentary: boolean): void {
  update(b, { ...b, momentary, hold: b.hold && momentary });
}

/** HOLD on or off for binding `b`: on for a momentary REC/DUB pedal only. */
export function setHold(b: MidiBinding, hold: boolean): void {
  if (hold && !(b.momentary && b.action === 'recDub')) return;
  update(b, { ...b, hold });
}

function consume(port: string, portName: string, status: number, data1: number, data2: number): boolean {
  const type = status & 0xf0;
  const kind = type === 0xb0 ? 'cc' : type === 0x90 || type === 0x80 ? 'note' : null;
  if (kind === null) return false;
  const channel = status & 0x0f;
  // A note-on at velocity 0 is a note-off, as in midi.ts.
  const high = kind === 'cc' ? data2 >= 64 : type === 0x90 && data2 > 0;

  const pick = learning();
  // CC 120–127 are channel-mode messages (all sound off, all notes off, …), never a switch.
  if (pick !== null && (kind === 'cc' ? data1 < 120 : high)) {
    const binding: MidiBinding = {
      port, portName, channel, kind, number: data1, ...pick, pressHigh: high, momentary: false, hold: false,
    };
    // One action per message: learning a bound message again moves it.
    const others = bindings().filter(
      (b) => !(b.port === port && b.channel === channel && b.kind === kind && b.number === data1),
    );
    held.delete(controlKey(binding));
    save([...others, binding]);
    if (kind === 'cc') releaseController(port, channel, data1);
    setLearning(null);
    setAwaitingRelease(binding);
    return true;
  }

  const b = bindings().find((x) => x.port === port && x.channel === channel && x.kind === kind && x.number === data1);
  if (!b) return false;
  if (awaitingRelease() === b) {
    setAwaitingRelease(null);
    if (high !== b.pressHigh) {
      update(b, { ...b, momentary: true });
      return true;
    }
    // The press side again with no release between: a latching or same-value pedal. It runs below.
  }
  if (!b.momentary) {
    runAction(b.action, b.target);
  } else if (high === b.pressHigh) {
    if (b.hold) held.set(controlKey(b), targetLane(b.target));
    runAction(b.action, b.target);
  } else {
    const lane = held.get(controlKey(b));
    held.delete(controlKey(b));
    if (lane !== undefined) releaseHold(lane);
  }
  return true;
}

/** Put MIDI learn in front of the play path. Returns the uninstall. */
export function installMidiActions(): () => void {
  setMidiConsumer(consume);
  return () => {
    setMidiConsumer(null);
    cancelLearn();
  };
}
