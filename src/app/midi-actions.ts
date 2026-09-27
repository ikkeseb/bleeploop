import { createSignal } from 'solid-js';
import { releaseController, setMidiConsumer } from '../audio/midi';
import { ACTION_LABELS, isLaneAction, isTrack, pressHold, releaseHold, runAction, targetLane, type ActionId, type Target } from './actions';

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
  /** HOLD, on a momentary REC/DUB pedal only: its release ends the capture its press started
   * (`pressHold`, `releaseHold`). */
  hold: boolean;
}

// Footswitches. A momentary pedal sends one value on press and the other on release (127, 0); a latching
// pedal sends one value per press, alternating (127, then 0 on the next press); some send the same value
// on every press. The values alone cannot tell a momentary release from a latching press, so the learn
// gesture decides, and runs nothing while it does: after the learning press the binding waits for its
// release (`waits`). The other side seen, however long the pedal was held, reads as momentary: it fires
// on each message on the press side and swallows the release (or, with HOLD, ends the capture its
// press started). The same side seen again, or no release within RELEASE_WAIT_MS, leaves it latching: it
// fires on every message, so a latching or same-value pedal fires once per press too. Level rather than
// edge on the press side, so a lost release message cannot swallow the next press. The wait belongs to
// the binding, not to the learn row: another LEARN, Esc or the panel closing while the pedal is still
// down leaves it running, so that release is still read as the release, never as a press or a new
// learn. A latching pedal pressed a second time within RELEASE_WAIT_MS of its learn reads as momentary;
// the list shows the kind, and its switch fixes a wrong read.

const STORAGE_KEY = 'lf.midiLearn';
/** How long a learned binding waits for its learning press's release before it stays latching. */
const RELEASE_WAIT_MS = 10_000;

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

// Each binding whose learning press's release may still come (Footswitches, above), with the timer that
// ends its wait. A plain Map: only the hint below is read by the UI.
const waits = new Map<MidiBinding, ReturnType<typeof setTimeout>>();
const [awaitingRelease, setAwaitingRelease] = createSignal<MidiBinding | null>(null);
/** The latest learned binding still waiting for its release, or null (the learn row's hint). */
export { awaitingRelease };

function startWait(b: MidiBinding): void {
  waits.set(b, setTimeout(() => endWait(b), RELEASE_WAIT_MS));
  setAwaitingRelease(b);
}

/** End binding `b`'s wait for its release, if it has one. */
function endWait(b: MidiBinding): void {
  const timer = waits.get(b);
  if (timer === undefined) return;
  clearTimeout(timer);
  waits.delete(b);
  if (awaitingRelease() === b) setAwaitingRelease(null);
}

// Each HOLD press, by its control, until its release ends the capture there: its target, the lane the
// web path acted on (the engine resolves its own), and the number the engine knows the control by.
const held = new Map<string, { target: Target; lane: number; control: number }>();
const controlKey = (b: MidiBinding) => `${b.port}\n${b.channel}\n${b.kind}\n${b.number}`;

/** The number HOLD's press on control `key` goes to the engine with, which its release repeats: the one
 * the control holds already (its release was lost), else the smallest no held control has, so two
 * pedals down at once are two controls to the engine and each release ends its own press. */
function holdControl(key: string): number {
  const own = held.get(key);
  if (own) return own.control;
  const taken = new Set([...held.values()].map((h) => h.control));
  let n = 0;
  while (taken.has(n)) n++;
  return n;
}

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
  endWait(b);
  held.delete(controlKey(b));
  save(bindings().map((x) => (x === b ? next : x)));
}

/** Learn the next CC or note-on, from any port, onto `action` (a lane action on `target`). */
export function learn(action: ActionId, target: Target = null): void {
  setLearning({ action, target: isLaneAction(action) ? target : null });
}

/** Stop listening. A learned pedal's wait for its release goes on (Footswitches, above). True when a learn
 * was pending (Esc spends itself on it). */
export function cancelLearn(): boolean {
  const was = learning() !== null;
  setLearning(null);
  return was;
}

/** Drop binding `b`: its messages reach the play path again. */
export function forget(b: MidiBinding): void {
  endWait(b);
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

  const b = bindings().find((x) => x.port === port && x.channel === channel && x.kind === kind && x.number === data1);
  // A learned pedal's release is its release, before anything else: even with LEARN listening again, it
  // is never a learn press.
  if (b && waits.has(b)) {
    endWait(b);
    if (high !== b.pressHigh) {
      update(b, { ...b, momentary: true });
      return true;
    }
    // The press side again with no release between: a latching or same-value pedal. It runs (or is
    // learned) below.
  }

  const pick = learning();
  // CC 120–127 are channel-mode messages (all sound off, all notes off, …), never a switch.
  if (pick !== null && (kind === 'cc' ? data1 < 120 : high)) {
    const binding: MidiBinding = {
      port, portName, channel, kind, number: data1, ...pick, pressHigh: high, momentary: false, hold: false,
    };
    // One action per message: learning a bound message again moves it (its wait ended above).
    const others = bindings().filter(
      (x) => !(x.port === port && x.channel === channel && x.kind === kind && x.number === data1),
    );
    held.delete(controlKey(binding));
    save([...others, binding]);
    if (kind === 'cc') releaseController(port, channel, data1);
    setLearning(null);
    startWait(binding);
    return true;
  }

  if (!b) return false;
  if (!b.momentary) {
    runAction(b.action, b.target);
  } else if (high === b.pressHigh) {
    if (b.hold) {
      const key = controlKey(b);
      const control = holdControl(key);
      held.set(key, { target: b.target, lane: targetLane(b.target), control });
      pressHold(b.target, control);
    } else {
      runAction(b.action, b.target);
    }
  } else {
    const press = held.get(controlKey(b));
    held.delete(controlKey(b));
    if (press) releaseHold(press.target, press.lane, press.control);
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
