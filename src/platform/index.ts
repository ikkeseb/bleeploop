/**
 * Runtime platform selection. `@tauri-apps/api` is pure JS, so importing it in a browser is safe
 * — `isTauri()` returns false and we hand back the web implementation. Under the Tauri shell
 * it returns true and we hand back `tauriPlatform`; nothing else in the app changes.
 */
import { isTauri } from '@tauri-apps/api/core';
import type { MidiSubscription, Platform } from './host';
import type { EngineCommand, NoteTarget } from './engine-wire';
import { encodeInput, encodeItem, type Dropped, type InputItem, type MidiEvent } from './midi-wire';
import { tauriInstallFrontendLogPipe } from './logging';
import { webEngineFake, webPlatform, type EngineFake } from './host.web';
import { notifyError } from '../notify';
import { reportTauriDiagnostics, tauriConfirmClose, tauriOnCloseRequested, tauriPlatform } from './host.tauri';

const underTauri = isTauri();

export const platform: Platform = underTauri ? tauriPlatform : webPlatform;

/**
 * DEV-only: report WebView2-internal facts (secure context, user agent) to `tauri dev` stdout —
 * the only way to verify them headlessly (no Playwright into WebView2). No-op in the browser build.
 * Informational; does not touch the plugin host.
 */
export const reportDiagnostics: () => Promise<void> = underTauri
  ? reportTauriDiagnostics
  : async () => {};

/**
 * Pipe the frontend's `console.error` + uncaught errors/rejections into the native log.
 * Release WebView2 has no visible console, so this is the only way a friend's crash reaches a log
 * file. Call once, early, at startup. No-op in the browser build.
 */
export const installFrontendLogPipe: () => void = underTauri
  ? tauriInstallFrontendLogPipe
  : () => {};

/**
 * Close guard (native builds): register a callback for the vetoed OS window close. Rust intercepts
 * `CloseRequested`, prevents it, and emits an event; the app confirms with the user (jam in
 * progress?) and calls `confirmNativeClose` to really close. No-op in the browser build — there the
 * app uses the standard `beforeunload` veto instead.
 */
export const onNativeCloseRequested: (cb: () => void) => void = underTauri
  ? tauriOnCloseRequested
  : () => {};

/** Approve the close: Rust lifts its guard and closes the window. No-op in the browser build. */
export const confirmNativeClose: () => Promise<void> = underTauri
  ? tauriConfirmClose
  : async () => {};

// ── The outbox: the engine's commands and the UI's input events, in one order ───────────────────────

/** What waits for the next send, and the promise its callers hold. */
interface Batch {
  items: InputItem[];
  settle: (took: boolean) => void;
  submitted: Promise<boolean>;
}

/** The items queued since the last send left. */
let waiting: Batch | null = null;
/** A send is in flight, or about to start: the next one waits for it. */
let sending = false;

/** This document's input epoch (`subscribeMidi`): the outbox sends nothing before the subscribe answered,
 * so no input of this document reaches native code under an epoch it was not given. 0 when the subscribe
 * failed (native MIDI does not run: every send then fails as it would anyway). */
let resolveFirstEpoch: ((epoch: Promise<number>) => void) | null = null;
let inputEpoch: Promise<number> = new Promise((resolve) => (resolveFirstEpoch = resolve));

/**
 * Subscribe to native MIDI's events (`MidiHost.subscribe`), first thing in the document's boot: its answer
 * is the input epoch every outbox batch presents, so nothing queued before it leaves until it answered. A
 * later subscribe (a remount) gives the next batches its own epoch.
 */
export function subscribeMidi(onEvent: (event: MidiEvent) => void): MidiSubscription {
  const subscription = platform.midi.subscribe(onEvent);
  const epoch = subscription.epoch.catch(() => 0);
  resolveFirstEpoch?.(epoch);
  resolveFirstEpoch = null;
  inputEpoch = epoch;
  return subscription;
}

/**
 * Queue `items` behind everything queued before. The outbox sends one `input_send` batch at a time,
 * carrying every item queued since the last one left, in order, and starts the next only once that one
 * settled: Tauri's IPC does not keep two calls in the order they were made, so a slot pick and the note
 * after it, or a looper press and a note, reach native code in the order they happened only this way.
 * Nothing is sent while the platform has no engine (the browser build without the DEV fake).
 */
function enqueue(items: InputItem[]): Promise<boolean> {
  if (!platform.engine.available) return Promise.resolve(false);
  if (items.length === 0) return Promise.resolve(true);
  if (!waiting) {
    let settle!: (took: boolean) => void;
    const submitted = new Promise<boolean>((resolve) => (settle = resolve));
    waiting = { items: [], settle, submitted };
    if (!sending) {
      sending = true;
      queueMicrotask(() => void drain());
    }
  }
  waiting.items.push(...items);
  return waiting.submitted;
}

/** Send what waits, one batch at a time, until nothing does. */
async function drain(): Promise<void> {
  try {
    while (waiting) {
      const epoch = await inputEpoch;
      const batch = waiting;
      waiting = null;
      batch.settle(await submit(epoch, batch.items));
    }
  } finally {
    sending = false;
  }
}

/** A note's press or release in a batch, or null. */
const noteOf = (item: InputItem): { on: boolean } | null =>
  'input' in item && typeof item.input === 'object' && 'note' in item.input ? item.input.note : null;

/** A release in a batch: what a lost batch must not leave holding. */
const releases = (item: InputItem): boolean =>
  'input' in item && (item.input === 'blur' || item.input === 'allNotesOff' || noteOf(item)?.on === false);

/** What native MIDI may drop: a note's press, or an engine command (a looper press with no device running,
 * a setting only when the engine had no room). */
const fresh = (item: InputItem): boolean => 'engine' in item || noteOf(item)?.on === true;

/** The reason the player was last told about a dropped input: told once, until a batch with something
 * native MIDI could have dropped goes through whole (a device runs again). */
let toldDropped: Dropped | null = null;

const DROPPED_DETAIL: Record<Dropped, string> = {
  noDevice: 'No audio device is running.',
  rebuilding: 'The audio device is restarting.',
  full: 'The engine had no room for it.',
};

/**
 * Send one batch. A rejection means none of it ran (native refuses a batch whole or runs it), so it is sent
 * once more; failing again, it is lost: logged and toasted, and when it held a release, a blur follows, which
 * releases every hold of this document natively (a note must not stick). What native MIDI dropped of a batch
 * that ran (a press with no device running) is logged and toasted once (`toldDropped`). True when the batch
 * ran and nothing of it was dropped.
 */
async function submit(epoch: number, items: InputItem[]): Promise<boolean> {
  let dropped: Dropped | null;
  try {
    dropped = await platform.input.send(epoch, items).catch(() => platform.input.send(epoch, items));
  } catch (err) {
    console.error('[platform] input batch failed', err);
    notifyError('The audio engine did not take a command', err);
    if (items.some(releases)) {
      await platform.input.send(epoch, [encodeItem.input(encodeInput.blur())]).catch((blurErr: unknown) => {
        console.error('[platform] the blur after a lost release failed', blurErr);
      });
    }
    return false;
  }
  if (dropped !== null && dropped !== toldDropped) {
    console.error(`[platform] native MIDI dropped input: ${dropped}`);
    notifyError('The audio engine did not take a command', DROPPED_DETAIL[dropped]);
    toldDropped = dropped;
  } else if (dropped === null && items.some(fresh)) {
    toldDropped = null;
  }
  return dropped === null;
}

/**
 * Queue commands for the engine. What one task sends leaves with the next outbox batch (`enqueue`), so a
 * gesture's commands reach the same block together, in order with the notes around them. A batch the host
 * could not take is logged and toasted. Notes, wheels, the note target and the panic go through `input`,
 * never here: native MIDI refuses them as engine commands.
 *
 * Resolves with the SUBMISSION of the batch these commands left in: true once the host ran all of it, false
 * when it did not (it was lost, or native MIDI dropped some of it: a press with no device running), or there
 * is no engine. False is no per-command answer: a command of a batch native MIDI partly dropped did reach
 * the engine (a setting always does). Never an acknowledgement either: the engine applies a command later,
 * and its outcome arrives on the feed. Most callers ignore it; a mix gesture drops its overlay on false, and
 * a command the engine did take brings the value back through its lane's `Mix` (`engine-store.ts`).
 */
export function sendEngine(...commands: EngineCommand[]): Promise<boolean> {
  return enqueue(commands.map(encodeItem.engine));
}

/**
 * The UI's note sources into native MIDI's one router, queued in one order with `sendEngine`. What
 * sounds, sustain, the wheels and which owner holds a note are the router's
 * (`src-tauri/src/engine_io/midi/router.rs`); the UI says what its pointers and keys did.
 */
export const input = {
  /** Pointer or key `owner` (`pointer:<id>`, `key:<code>`) pressed (`on`) or let go of `note`, at MIDI
   * velocity 0..127. */
  note(owner: string, note: number, velocity: number, on: boolean): void {
    void enqueue([encodeItem.input(encodeInput.note(owner, note, velocity, on))]);
  },
  /** The window lost focus: this document's pointers and keys are up. */
  blur(): void {
    void enqueue([encodeItem.input(encodeInput.blur())]);
  },
  /** Route the notes to `target`, picked on `slot` (null: none). What sounds is released first; the same
   * slot and target again change nothing (natively), so a call per press is cheap. */
  selectTarget(slot: 0 | 1 | null, target: NoteTarget): void {
    void enqueue([encodeItem.input(encodeInput.selectTarget(slot, target))]);
  },
  /** Panic: every note that sounds is released and forgotten. */
  allNotesOff(): void {
    void enqueue([encodeItem.input(encodeInput.allNotesOff())]);
  },
};

/** The web engine fake a probe scripts through `__lf.native`; null under Tauri. */
export const engineFake: EngineFake | null = underTauri ? null : webEngineFake;

export * from './host';
export * from './engine-wire';
export * from './midi-wire';
