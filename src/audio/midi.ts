import { createSignal } from 'solid-js';
import { inputRouter } from './input-router';
import { ensureActive } from './instrument';
import { engine } from './engine';
import { platform } from '../platform';
import { notifyError } from '../notify';

/**
 * Web MIDI manager. Calls platform.midi.requestAccess() (goes through the capability
 * boundary — never touches navigator.requestMIDIAccess directly here). Hardware MIDI
 * absent or unsupported must NOT affect the on-screen keyboard or computer keyboard input.
 */

export type MidiStatus = 'idle' | 'unsupported' | 'denied' | 'error' | 'no-devices' | 'connected';

const [midiStatus, setMidiStatus] = createSignal<MidiStatus>('idle');
const [midiDevices, setMidiDevices] = createSignal<string[]>([]);

/** Reactive MIDI status, including unsupported, permission-denied and request-error states. */
export { midiStatus };

/** Reactive list of connected MIDI input device names. */
export { midiDevices };

let _access: MIDIAccess | null = null;
let startInFlight: Promise<void> | null = null;

/** Standard pitch-bend range: the wheel's full throw = ±2 semitones. */
const PITCH_BEND_RANGE_SEMITONES = 2;

function midiOwner(port: string, channel: number): string { return JSON.stringify([port, channel]); }

/**
 * Sees every 3-byte channel message before the play path and returns true to claim it; a claimed message
 * reaches nothing below (no note, no CC64/1/123 branch, no bend). `port` is the input's id, `portName` its
 * display name. MIDI learn installs it from the app layer (`src/app/midi-actions.ts`): `src/audio/` never
 * imports `src/app/`, so the action table stays out of this file.
 */
type MidiConsumer = (port: string, portName: string, status: number, data1: number, data2: number) => boolean;

let consumer: MidiConsumer | null = null;

/** Install the consume-first hook (`null` removes it). One at a time: MIDI learn is its only user. */
export function setMidiConsumer(fn: MidiConsumer | null): void {
  consumer = fn;
}

/**
 * The consume-first hook takes `controller` on this port and channel over, so its next values never
 * reach the branch below: let go of what it last set there, as an unplug does. A pedal held down while it
 * was learned would otherwise sustain that channel for good, and a learned mod wheel would keep its
 * vibrato on (a wheel set to 0 instead would override another port's wheel as the last one moved).
 */
export function releaseController(port: string, channel: number, controller: number): void {
  const owner = midiOwner(port, channel);
  if (controller === 64) inputRouter.setSustain(false, owner);
  else if (controller === 1) inputRouter.dropModulation(owner);
}

function parseMidiMessage(ev: Event, port: string, portName: string): void {
  const msg = ev as MIDIMessageEvent;
  const data = msg.data;
  if (!data || data.length === 0) return;

  // Single-byte realtime messages (0xF8 timing clock, start/stop/sensing) fall out here: the
  // AudioContext is the only tempo authority (invariant 1), so external MIDI clock is not tracked.
  if (data.length < 3) return;

  if (consumer?.(port, portName, data[0], data[1], data[2])) return;

  const status = data[0];
  const note   = data[1];
  const vel    = data[2];

  const type = status & 0xf0;
  const owner = midiOwner(port, status & 0x0f);

  if (type === 0x90) {
    // Note-on; velocity 0 is treated as note-off per MIDI spec
    if (vel === 0) {
      inputRouter.handle({ type: 'off', note, velocity: 0, source: 'midi', owner });
    } else {
      // Mirror the keyboard play path (Keyboard.tsx press(): `engine.start()` then `ensureActive()`):
      // resume the engine + route the active slot BEFORE dispatch. A MIDI note played before any
      // on-screen/slot interaction would otherwise hit a null active sink → silent first notes, plus a
      // phantom held-note entry if a slot is picked mid-hold. MIDI controller is the PRIMARY play path,
      // so this cold-start is the common case. ensureActive is idempotent + cheap (stable engine/sink
      // refs → no held-note flush on repeat).
      void engine.start();
      ensureActive();
      inputRouter.handle({ type: 'on', note, velocity: vel, source: 'midi', owner });
    }
  } else if (type === 0x80) {
    // Note-off
    inputRouter.handle({ type: 'off', note, velocity: 0, source: 'midi', owner });
  } else if (type === 0xb0) {
    // Sustain/modulation drive built-in synths; CC123 releases this port/channel's held keys.
    // Native plugins receive individual note-offs, but raw controller forwarding needs native support.
    const controller = data[1];
    const value = data[2];
    if (controller === 64) {
      // Sustain pedal: value >= 64 = down. The deferred note-offs live in input-router.
      inputRouter.setSustain(value >= 64, owner);
    } else if (controller === 1) {
      // Mod wheel → vibrato depth (0..1).
      inputRouter.setModulation(value / 127, owner);
    } else if (controller === 123) {
      inputRouter.releaseSource(owner);
    }
  } else if (type === 0xe0) {
    // Pitch bend: 14-bit little-endian (LSB then MSB), center 8192. Scale to ±range semitones.
    const raw = ((data[2] << 7) | data[1]) - 8192;
    inputRouter.setPitchBend((raw / 8192) * PITCH_BEND_RANGE_SEMITONES, owner);
  }
}

/**
 * The input ports we currently listen on, port id → display name. A device unplugged mid-note sends no
 * note-offs, so its voices ring forever; comparing this map against the live port list on every
 * statechange releases that port's notes/controllers and shows a named toast. A disconnected port may either vanish
 * from `access.inputs` or linger there with `state === 'disconnected'` (the spec allows both) — both
 * count as gone.
 */
const attachedInputs = new Map<string, string>();

function attachInputs(access: MIDIAccess): void {
  const names: string[] = [];
  const present = new Set<string>();
  access.inputs.forEach((input) => {
    if (input.state === 'disconnected') return;
    const name = input.name ?? '(unknown)';
    input.onmidimessage = (event) => {
      if (input.state !== 'disconnected') parseMidiMessage(event, input.id, name);
    };
    present.add(input.id);
    attachedInputs.set(input.id, name);
    names.push(name);
  });

  const gone: string[] = [];
  for (const [id, name] of attachedInputs) {
    if (present.has(id)) continue;
    attachedInputs.delete(id);
    for (let channel = 0; channel < 16; channel++) inputRouter.releaseSource(midiOwner(id, channel), true);
    gone.push(name);
  }
  if (gone.length > 0) {
    // Whatever was held on the vanished controller can only be released from this side.
    for (const name of gone) {
      console.error(`[midi] input disconnected: ${name}`);
      notifyError(`MIDI device disconnected — ${name}`, 'Held notes were released.');
    }
  }

  setMidiDevices(names);
  setMidiStatus(names.length > 0 ? 'connected' : 'no-devices');
}

/**
 * Request Web MIDI access and start listening. Safe to call multiple times (idempotent).
 * Errors and lack of hardware are handled gracefully.
 */
async function requestAccess(): Promise<void> {
  try {
    const access = await platform.midi.requestAccess();
    if (!access) {
      setMidiStatus('unsupported');
      return;
    }
    _access = access;
    attachInputs(access);

    // Hot-plug: re-enumerate whenever a device connects/disconnects.
    access.onstatechange = () => attachInputs(access);
  } catch (e) {
    const name = e instanceof DOMException ? e.name : '';
    const denied = name === 'SecurityError' || name === 'NotAllowedError';
    // A permission denial is the environment's answer (a headless rig, a locked-down WebView), not a
    // fault: it warns. Anything else is an error and reaches the release log.
    if (denied) console.warn('[midi] requestMIDIAccess denied', e);
    else console.error('[midi] requestMIDIAccess failed', e);
    setMidiStatus(denied ? 'denied' : 'error');
  }
}

export function start(): Promise<void> {
  if (_access) return Promise.resolve(); // already started
  if (startInFlight) return startInFlight;
  startInFlight = requestAccess();
  const pending = startInFlight;
  void pending.finally(() => {
    if (startInFlight === pending) startInFlight = null;
  });
  return pending;
}

/**
 * Retry a failed permission/request attempt. Concurrent retries share the same request. Only
 * `verify/probes/instrument-controls.mjs` calls it, hence `@public` for knip.
 * @public
 */
export function retry(): Promise<void> {
  const status = midiStatus();
  return status === 'denied' || status === 'error' ? start() : Promise.resolve();
}
