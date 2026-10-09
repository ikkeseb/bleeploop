/**
 * OWNS: the JSON between the UI and native MIDI (`src-tauri/src/engine_io/midi/mod.rs`, `MidiHost`):
 * the events it tells the UI on the `midi_subscribe` channel (`MidiEvent`), the input events every UI
 * note source sends through `input_send` (`InputEvent`) and what `midi_import_legacy` answers
 * (`ImportReport`). The Rust shapes are the module's serde types (`MidiEvent`, `ListedBinding`,
 * `PortInfo`, `Binding`, `store.rs` `ImportReport`): camelCase names, externally tagged enums. One
 * fixture, `verify/fixtures/midi-wire.json`, pins this side (`verify/guards/midi-wire.mjs`).
 *
 * As `engine-wire.ts`: what comes back passes a decoder that throws on a variant or a field it cannot
 * read, so a drift fails at the first event; unknown extra fields are ignored. Pure: a Node guard
 * imports this file directly.
 */
import { decodeNoteTarget, type NoteTarget } from './engine-wire.ts'; // explicit .ts: Node guards import this file
import { array, bool, fail, int, nullable, obj, oneOf, str, tagged } from './wire-read.ts';

/** The named actions a binding runs, as `src/app/actions.ts` names them (Rust `ActionId`), in the learn
 * picker's order. */
export type MidiActionId =
  | 'recDub'
  | 'playStop'
  | 'undo'
  | 'clear'
  | 'mute'
  | 'reverse'
  | 'copy'
  | 'halveTrack'
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
const ACTION_IDS: readonly MidiActionId[] = [
  'recDub', 'playStop', 'undo', 'clear', 'mute', 'reverse', 'copy', 'halveTrack', 'nextTrack', 'prevTrack', 'playAll',
  'stopAll', 'fadeAll', 'goLive', 'stageView', 'stageNextView', 'tapTempo', 'clickToggle', 'endStopToggle', 'fixedToggle',
  'retakeToggle', 'autoRecToggle', 'inFxEcho', 'inFxReverb', 'inFxRing',
];

/** One learned message (Rust `Binding`). `portId` is the port's identity, matched by equality; `portName`
 * is for display. `target`: a lane action's track (0-based), null for the selected track (always null for a
 * global action). `pressHigh`: the learning press sent a CC value of 64 or more, or a note-on. */
export interface MidiBinding {
  portId: string;
  portName: string;
  channel: number;
  kind: 'cc' | 'note';
  number: number;
  action: MidiActionId;
  target: number | null;
  pressHigh: boolean;
  momentary: boolean;
  hold: boolean;
}

/** How a stored binding stands on the present ports (Rust `BindingState`): `live` runs; `blocked` waits for
 * the player's assignment (`ListedBinding.blocked` says why); `noPort`: no present port answers to it;
 * `severalPorts`: several present ports carry its name; `severalAbsent`: another stored, absent port
 * carries its name too. */
export type BindingState = 'live' | 'blocked' | 'noPort' | 'severalPorts' | 'severalAbsent';
const BINDING_STATES: readonly BindingState[] = ['live', 'blocked', 'noPort', 'severalPorts', 'severalAbsent'];

/** A stored binding as the list shows it (Rust `ListedBinding`, `Listed` flattened into it). */
export interface ListedBinding {
  binding: MidiBinding;
  /** Learned natively, or imported from the web's `lf.midiLearn`. */
  origin: 'native' | 'legacy';
  /** Its port id is a legacy run's ordinal: the port name is its identity until it resolves. */
  ordinal: boolean;
  /** Why it waits for the player's assignment, when it does. */
  blocked: string | null;
  /** The port as the player knows it. */
  displayName: string;
  state: BindingState;
}

/** `open`; `busy`: its last open failed, most likely another program holds it; `closed`: not open. */
export type MidiPortState = 'open' | 'busy' | 'closed';
/** A present input port (Rust `PortInfo`). `id` is what `MidiHost.assign` takes. */
export interface MidiPort {
  id: string;
  name: string;
  state: MidiPortState;
}

/** What the next CC or note-on will be learned onto (Rust `LearnPick`). */
export interface LearnPick {
  action: MidiActionId;
  target: number | null;
}

/** An action a binding fired that the UI runs (Rust `UiAction`). */
export type MidiUiAction = 'goLive' | 'stageView' | 'stageNextView' | 'tapTempo';
const UI_ACTIONS: readonly MidiUiAction[] = ['goLive', 'stageView', 'stageNextView', 'tapTempo'];

/** Why MIDI learn consumed a press and ran nothing (Rust `LearnRefusal`): a HOLD press while every one of
 * the engine's HOLD control numbers is held. */
export type LearnRefusal = 'holdControlsTaken';
const LEARN_REFUSALS: readonly LearnRefusal[] = ['holdControlsTaken'];

/** Why the bindings are not, or not all, on disk (Rust `StoreProblem`), in its wire shape. */
export type StoreProblem =
  | { readOnly: { why: string } }
  | { conflict: { why: string } }
  | { failed: { why: string } }
  | { rejected: { count: number } };

/** What native MIDI tells the UI (Rust `MidiEvent`), decoded to a `type` and its fields. The first events
 * of a subscription are the resync: the ports, the bindings, the learn's state, the store's problem and
 * the held notes. */
export type MidiEvent =
  | { type: 'ports'; ports: MidiPort[] }
  /** Ports that went away; their notes and HOLD presses were released. */
  | { type: 'gone'; names: string[] }
  | { type: 'bindings'; bindings: ListedBinding[] }
  /** Null once a learn captured or was cancelled. */
  | { type: 'learning'; learning: LearnPick | null }
  /** The latest learned binding still waiting for its release; null once the wait ended. */
  | { type: 'awaitingRelease'; binding: MidiBinding | null }
  | { type: 'learned'; binding: MidiBinding }
  | { type: 'refused'; reason: LearnRefusal }
  | { type: 'run'; action: MidiUiAction }
  /** A binding fired a looper press (native sent its `Press` or action). */
  | { type: 'pressed' }
  | { type: 'store'; problem: StoreProblem }
  /** The notes held down now, from every source (not the sustained ones). */
  | { type: 'held'; notes: number[]; changes: number };

/** One UI input event for the native router (`input_send`, Rust `InputEvent`), in its wire shape. `owner`
 * is the physical source (`pointer:<id>`, `key:<code>`); velocity 0..127, 0 a release. `selectTarget` moves
 * the notes to `target`, picked on `slot` (null: no slot); `allNotesOff` is the panic. */
export type InputEvent =
  | { note: { owner: string; note: number; velocity: number; on: boolean } }
  | 'blur'
  | { selectTarget: { slot: number | null; target: NoteTarget } }
  | 'allNotesOff';

/** What the one-time import of the web's `lf.midiLearn` did, by each record's position in that list. */
export interface ImportReport {
  /** The import had already run: nothing changed. */
  already: boolean;
  /** The document was not a JSON array: kept as one rejected entry. */
  unreadable: string | null;
  imported: number[];
  /** Imported, waiting for the player's assignment. */
  blocked: { index: number; why: string }[];
  /** Not readable; kept as text. */
  rejected: { index: number; reason: string }[];
  /** The store already held a binding on that control. */
  skipped: number[];
}

// ── Decoders ───────────────────────────────────────────────────────────────────────────────────────

const u8 = (v: unknown, what: string, max = 255) => int(v, what, 0, max);

function decodeBinding(v: unknown, what: string): MidiBinding {
  const o = obj(v, what);
  return {
    portId: str(o.portId, `${what}.portId`),
    portName: str(o.portName, `${what}.portName`),
    channel: u8(o.channel, `${what}.channel`, 15),
    kind: oneOf(o.kind, ['cc', 'note'] as const, `${what}.kind`),
    number: u8(o.number, `${what}.number`, 127),
    action: oneOf(o.action, ACTION_IDS, `${what}.action`),
    target: nullable(o.target, (t) => u8(t, `${what}.target`)),
    pressHigh: bool(o.pressHigh, `${what}.pressHigh`),
    momentary: bool(o.momentary, `${what}.momentary`),
    hold: bool(o.hold, `${what}.hold`),
  };
}

function decodeListed(v: unknown, what: string): ListedBinding {
  const o = obj(v, what);
  return {
    binding: decodeBinding(o.binding, `${what}.binding`),
    origin: oneOf(o.origin, ['native', 'legacy'] as const, `${what}.origin`),
    ordinal: bool(o.ordinal, `${what}.ordinal`),
    blocked: nullable(o.blocked, (b) => str(b, `${what}.blocked`)),
    displayName: str(o.displayName, `${what}.displayName`),
    state: oneOf(o.state, BINDING_STATES, `${what}.state`),
  };
}

function decodePort(v: unknown, what: string): MidiPort {
  const o = obj(v, what);
  return {
    id: str(o.id, `${what}.id`),
    name: str(o.name, `${what}.name`),
    state: oneOf(o.state, ['open', 'busy', 'closed'] as const, `${what}.state`),
  };
}

function decodePick(v: unknown, what: string): LearnPick {
  const o = obj(v, what);
  return { action: oneOf(o.action, ACTION_IDS, `${what}.action`), target: nullable(o.target, (t) => u8(t, `${what}.target`)) };
}

function decodeStoreProblem(v: unknown, what: string): StoreProblem {
  const [name, payload] = tagged(v, what);
  const o = obj(payload, `${what}.${name}`);
  switch (name) {
    case 'readOnly':
    case 'conflict':
    case 'failed':
      return { [name]: { why: str(o.why, `${what}.${name}.why`) } } as StoreProblem;
    case 'rejected':
      return { rejected: { count: int(o.count, `${what}.rejected.count`) } };
    default:
      fail(`unknown ${what} variant`, v);
  }
}

/** Read one event off the `midi_subscribe` channel. Throws on anything this side cannot read. */
export function decodeMidiEvent(raw: unknown): MidiEvent {
  const [name, payload] = tagged(raw, 'MidiEvent');
  if (name === 'pressed') {
    if (payload !== undefined) fail('MidiEvent pressed is a unit variant', raw);
    return { type: 'pressed' };
  }
  const what = `MidiEvent.${name}`;
  const o = obj(payload, what);
  switch (name) {
    case 'ports':
      return { type: 'ports', ports: array(o.ports, `${what}.ports`).map((p, i) => decodePort(p, `${what}.ports[${i}]`)) };
    case 'gone':
      return { type: 'gone', names: array(o.names, `${what}.names`).map((n, i) => str(n, `${what}.names[${i}]`)) };
    case 'bindings':
      return {
        type: 'bindings',
        bindings: array(o.bindings, `${what}.bindings`).map((b, i) => decodeListed(b, `${what}.bindings[${i}]`)),
      };
    case 'learning':
      return { type: 'learning', learning: nullable(o.learning, (p) => decodePick(p, `${what}.learning`)) };
    case 'awaitingRelease':
      return { type: 'awaitingRelease', binding: nullable(o.binding, (b) => decodeBinding(b, `${what}.binding`)) };
    case 'learned':
      return { type: 'learned', binding: decodeBinding(o.binding, `${what}.binding`) };
    case 'refused':
      return { type: 'refused', reason: oneOf(o.reason, LEARN_REFUSALS, `${what}.reason`) };
    case 'run':
      return { type: 'run', action: oneOf(o.action, UI_ACTIONS, `${what}.action`) };
    case 'store':
      return { type: 'store', problem: decodeStoreProblem(o.problem, `${what}.problem`) };
    case 'held':
      return {
        type: 'held',
        notes: array(o.notes, `${what}.notes`).map((n, i) => u8(n, `${what}.notes[${i}]`, 127)),
        changes: int(o.changes, `${what}.changes`),
      };
    default:
      fail('unknown MidiEvent variant', raw);
  }
}

/** Check one input event as the Rust side reads it (the browser fake records only what passes). */
export function decodeInputEvent(raw: unknown): InputEvent {
  const [name, payload] = tagged(raw, 'InputEvent');
  switch (name) {
    case 'note': {
      const o = obj(payload, 'InputEvent.note');
      str(o.owner, 'InputEvent.note.owner');
      u8(o.note, 'InputEvent.note.note', 127);
      u8(o.velocity, 'InputEvent.note.velocity', 127);
      bool(o.on, 'InputEvent.note.on');
      break;
    }
    case 'selectTarget': {
      const o = obj(payload, 'InputEvent.selectTarget');
      nullable(o.slot, (s) => u8(s, 'InputEvent.selectTarget.slot'));
      decodeNoteTarget(o.target, 'InputEvent.selectTarget.target');
      break;
    }
    case 'blur':
    case 'allNotesOff':
      if (payload !== undefined && payload !== null) fail(`InputEvent ${name} is a unit variant`, raw);
      break;
    default:
      fail('unknown InputEvent variant', raw);
  }
  return raw as InputEvent;
}

/** Read `midi_import_legacy`'s answer. */
export function decodeImportReport(raw: unknown): ImportReport {
  const o = obj(raw, 'ImportReport');
  const indices = (v: unknown, what: string) => array(v, `ImportReport.${what}`).map((i, n) => int(i, `ImportReport.${what}[${n}]`));
  return {
    already: bool(o.already, 'ImportReport.already'),
    unreadable: nullable(o.unreadable, (u) => str(u, 'ImportReport.unreadable')),
    imported: indices(o.imported, 'imported'),
    blocked: array(o.blocked, 'ImportReport.blocked').map((b, i) => {
      const e = obj(b, `ImportReport.blocked[${i}]`);
      return { index: int(e.index, `ImportReport.blocked[${i}].index`), why: str(e.why, `ImportReport.blocked[${i}].why`) };
    }),
    rejected: array(o.rejected, 'ImportReport.rejected').map((r, i) => {
      const e = obj(r, `ImportReport.rejected[${i}]`);
      return { index: int(e.index, `ImportReport.rejected[${i}].index`), reason: str(e.reason, `ImportReport.rejected[${i}].reason`) };
    }),
    skipped: indices(o.skipped, 'skipped'),
  };
}
