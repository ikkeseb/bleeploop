import { createSignal } from 'solid-js';
import { releaseController, setMidiConsumer } from '../audio/midi';
import { ACTION_LABELS, runAction, type ActionId } from './actions';

/**
 * OWNS: MIDI learn. A learned CC or note, on one input port and channel, runs a named action
 * (`actions.ts`) and is consumed before the play path through `setMidiConsumer` (`src/audio/midi.ts`):
 * a CC learned onto 64 does not sustain (a pedal held while it was learned is let go), and a learned
 * note does not sound (its note-off is consumed too). Unlearned traffic reaches the play path exactly as
 * before. The bindings persist in localStorage. A port is keyed by its Web MIDI input id, which the spec
 * asks browsers to keep across restarts and replugs; its name is kept for the list only.
 */

/** One learned message. `pressHigh`: the learning press sent a CC value ≥ 64, or a note-on. */
export interface MidiBinding {
  port: string;
  portName: string;
  channel: number;
  kind: 'cc' | 'note';
  number: number;
  action: ActionId;
  pressHigh: boolean;
  momentary: boolean;
}

// Footswitches. A momentary pedal sends one value on press and the other on release (127, 0); a latching
// pedal sends one value per press, alternating (127, then 0 on the next press); some send the same value
// on every press. The values alone cannot tell a momentary release from a latching press, so the learn
// gesture decides: a pedal that sends the other side within RELEASE_MS of its learning press was seen to
// release. It is momentary: it fires on each message on the press side and swallows the release. Every
// other binding fires on every message, so a latching or same-value pedal fires once per press too. Level
// rather than edge on the press side, so a lost release message cannot swallow the next press. A pedal
// held past the window while learning reads as latching (its release would fire as well); the list shows
// the kind, and learning it again with a tap fixes it.
const RELEASE_MS = 1000;

const STORAGE_KEY = 'lf.midiLearn';

function isBinding(v: unknown): v is MidiBinding {
  const b = v as Partial<MidiBinding> | null;
  return (
    typeof b === 'object' && b !== null &&
    typeof b.port === 'string' && typeof b.portName === 'string' &&
    Number.isInteger(b.channel) && (b.channel as number) >= 0 && (b.channel as number) < 16 &&
    (b.kind === 'cc' || b.kind === 'note') &&
    Number.isInteger(b.number) && (b.number as number) >= 0 && (b.number as number) < 128 &&
    typeof b.action === 'string' && Object.hasOwn(ACTION_LABELS, b.action) &&
    typeof b.pressHigh === 'boolean' && typeof b.momentary === 'boolean'
  );
}

/** The persisted bindings; anything malformed (or naming an action that no longer exists) is dropped. */
function load(): MidiBinding[] {
  try {
    const list: unknown = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? '[]');
    return Array.isArray(list) ? list.filter(isBinding) : [];
  } catch {
    return [];
  }
}

const [bindings, setBindings] = createSignal<readonly MidiBinding[]>(load());
/** The learned bindings, in learn order. */
export { bindings };

const [learning, setLearning] = createSignal<ActionId | null>(null);
/** The action the next CC or note-on will be learned onto, or null. */
export { learning };

function save(list: readonly MidiBinding[]): void {
  setBindings(list);
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(list));
  } catch {
    /* persistence is best-effort */
  }
}

// The binding just learned, while its learning press may still be followed by a release (RELEASE_MS).
let tail: { binding: MidiBinding; until: number } | null = null;

/** Learn the next CC or note-on, from any port, onto `action`. */
export function learn(action: ActionId): void {
  setLearning(action);
}

/** Stop listening. True when a learn was pending (Esc spends itself on it). */
export function cancelLearn(): boolean {
  const was = learning() !== null;
  setLearning(null);
  return was;
}

/** Drop binding `b`: its messages reach the play path again. */
export function forget(b: MidiBinding): void {
  if (tail?.binding === b) tail = null;
  save(bindings().filter((x) => x !== b));
}

function consume(port: string, portName: string, status: number, data1: number, data2: number): boolean {
  const type = status & 0xf0;
  const kind = type === 0xb0 ? 'cc' : type === 0x90 || type === 0x80 ? 'note' : null;
  if (kind === null) return false;
  const channel = status & 0x0f;
  // A note-on at velocity 0 is a note-off, as in midi.ts.
  const high = kind === 'cc' ? data2 >= 64 : type === 0x90 && data2 > 0;

  const action = learning();
  // CC 120–127 are channel-mode messages (all sound off, all notes off, …), never a switch.
  if (action !== null && (kind === 'cc' ? data1 < 120 : high)) {
    const binding: MidiBinding = { port, portName, channel, kind, number: data1, action, pressHigh: high, momentary: false };
    // One action per message: learning a bound message again moves it.
    const others = bindings().filter(
      (b) => !(b.port === port && b.channel === channel && b.kind === kind && b.number === data1),
    );
    save([...others, binding]);
    if (kind === 'cc') releaseController(port, channel, data1);
    tail = { binding, until: performance.now() + RELEASE_MS };
    setLearning(null);
    return true;
  }

  const b = bindings().find((x) => x.port === port && x.channel === channel && x.kind === kind && x.number === data1);
  if (!b) return false;
  if (tail?.binding === b) {
    const released = high !== b.pressHigh && performance.now() < tail.until;
    tail = null;
    if (released) {
      save(bindings().map((x) => (x === b ? { ...b, momentary: true } : x)));
      return true;
    }
  }
  if (!b.momentary || high === b.pressHigh) runAction(b.action);
  return true;
}

/** Put MIDI learn in front of the play path. Returns the uninstall. */
export function installMidiActions(): () => void {
  setMidiConsumer(consume);
  return () => {
    setMidiConsumer(null);
    setLearning(null);
  };
}
