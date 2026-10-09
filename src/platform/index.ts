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

/** How long a native call the UI waits on in order (an `input_send`, the subscribe's answer, a MIDI learn
 * call) may take before the UI gives up on it and goes on: one that never settled must not stall what
 * comes after it. Its fate is then unknown: it may still run. */
export const NATIVE_WAIT_MS = 2000;

class TimedOut extends Error {}

/** `promise`, or a `TimedOut` rejection once `ms` passed with no answer. */
export function within<T>(promise: Promise<T>, ms: number, what: string): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const late = new Promise<never>((_, reject) => {
    timer = setTimeout(() => reject(new TimedOut(`${what}: no answer in ${ms} ms`)), ms);
  });
  return Promise.race([promise, late]).finally(() => clearTimeout(timer));
}

/** The most items that wait for the next batch while one is on its way. Past it, a fresh note-on or an
 * engine command is dropped (one release-log line per overflow); a release, a blur, a panic and the note
 * target always wait. */
const WAITING_MAX = 256;

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
/** The waiting batch overflowed and the release log said so; cleared when it leaves. */
let overflowTold = false;

/** This page's input epoch (`subscribeMidi`; before it, a promise that never settles): the outbox sends
 * nothing before the subscribe answered (or `NATIVE_WAIT_MS` passed: then 0, which native refuses input
 * events under, until a late answer comes), so no input of this page reaches native code under an epoch it
 * was not given. 0 when the subscribe failed (native MIDI does not run: every send then fails as it would
 * anyway). */
let inputEpoch: Promise<number> = new Promise<number>(() => {});
/** Settles when a subscribe replaces `inputEpoch`, so a reader waiting on the one before stops waiting. */
let resubscribed: { settled: Promise<void>; settle: () => void } = signal();

function signal(): { settled: Promise<void>; settle: () => void } {
  let settle!: () => void;
  return { settled: new Promise<void>((resolve) => (settle = resolve)), settle };
}

/** `epoch` is the page's input epoch from now on. */
function adopt(epoch: Promise<number>): void {
  inputEpoch = epoch;
  const replaced = resubscribed;
  resubscribed = signal();
  replaced.settle();
}

/** The page's input epoch as it is when it is read: a later subscribe wins over an earlier one still on
 * its way, whichever answers first. Every native call of this page presents it (the outbox's batches, the
 * learn calls of `src/ui/state/midi.ts`). */
export async function pageEpoch(): Promise<number> {
  for (;;) {
    const current = inputEpoch;
    const epoch = await Promise.race([current, resubscribed.settled.then(() => null)]);
    if (epoch !== null && current === inputEpoch) return epoch;
  }
}

/**
 * Subscribe to native MIDI's events (`MidiHost.subscribe`), first thing in the document's boot: its answer
 * is the input epoch every outbox batch presents, so nothing queued before it leaves until it answered. A
 * later subscribe (a remount) gives the next batches its own epoch.
 */
export function subscribeMidi(onEvent: (event: MidiEvent) => void): MidiSubscription {
  const subscription = platform.midi.subscribe(onEvent);
  const answered = subscription.epoch.catch(() => 0);
  const epoch: Promise<number> = within(answered, NATIVE_WAIT_MS, 'native MIDI subscribe').catch((err: unknown) => {
    console.error('[platform] native MIDI did not answer the subscribe in time; input goes on without it', err);
    notifyError('The app lost contact with MIDI', err);
    // A late answer still becomes the page's epoch, unless a later subscribe replaced this one.
    void answered.then((late) => {
      if (inputEpoch === epoch && late > 0) adopt(Promise.resolve(late));
    });
    return 0;
  });
  adopt(epoch);
  return subscription;
}

/** A note's press or release in a batch, or null. */
const noteOf = (item: InputItem): { on: boolean } | null =>
  'input' in item && typeof item.input === 'object' && 'note' in item.input ? item.input.note : null;

/** A release in a batch: what a lost batch must not leave holding. */
const releases = (item: InputItem): boolean =>
  'input' in item && (item.input === 'blur' || item.input === 'allNotesOff' || noteOf(item)?.on === false);

/** A press: a note-on, or a looper press (an engine command that sets no setting). What native MIDI drops
 * with no device running. */
const press = (item: InputItem): boolean =>
  noteOf(item)?.on === true || ('engine' in item && !(typeof item.engine === 'object' && Object.keys(item.engine)[0].startsWith('Set')));

/** What an overflowing queue may drop: a note-on or an engine command (a setting so dropped resolves its
 * `sendEngine` false: a mix gesture shows the engine's value again). */
const droppable = (item: InputItem): boolean => noteOf(item)?.on === true || 'engine' in item;

/**
 * Queue `items` behind everything queued before. The outbox sends one `input_send` batch at a time,
 * carrying every item queued since the last one left, in order, and starts the next only once that one
 * settled (or `NATIVE_WAIT_MS` passed): Tauri's IPC does not keep two calls in the order they were made, so
 * a slot pick and the note after it, or a looper press and a note, reach native code in the order they
 * happened only this way. Nothing is sent while the platform has no engine (the browser build without the
 * DEV fake). False when an item was dropped (the waiting batch was full).
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
  let kept = true;
  for (const item of items) {
    if (waiting.items.length >= WAITING_MAX && droppable(item)) {
      kept = false;
      if (!overflowTold) {
        overflowTold = true;
        console.error(`[platform] input waits on a native call: past ${WAITING_MAX} queued items, presses and engine commands are dropped until it settles`);
      }
      continue;
    }
    waiting.items.push(item);
  }
  return kept ? waiting.submitted : Promise.resolve(false);
}

/** Send what waits, one batch at a time, until nothing does. */
async function drain(): Promise<void> {
  try {
    while (waiting) {
      await pageEpoch();
      const batch = waiting;
      waiting = null;
      overflowTold = false;
      batch.settle(await submit(batch.items));
    }
  } finally {
    sending = false;
  }
}

/** One `input_send` under the page's epoch of now: its answer, or why it was lost. */
type Sent = { epoch: number; dropped: Dropped | null } | { lost: unknown; timedOut: boolean };

async function sendOnce(items: InputItem[]): Promise<Sent> {
  const epoch = await pageEpoch();
  try {
    return { epoch, dropped: await within(platform.input.send(epoch, items), NATIVE_WAIT_MS, 'input_send') };
  } catch (err) {
    return { lost: err, timedOut: err instanceof TimedOut };
  }
}

const BLUR: InputItem = encodeItem.input(encodeInput.blur());
/** A blur a lost release still owes native MIDI: it rides at the head of every batch until one is taken. */
let pendingBlur = false;

/** The reason the player was last told about a dropped press: told once, until a press goes through. */
let toldDropped: Dropped | null = null;
/** The epoch whose refused input the release log last named. */
let toldStale = 0;

const DROPPED_DETAIL: Record<Exclude<Dropped, 'stale'>, string> = {
  noDevice: 'No audio device is running.',
  rebuilding: 'The audio device is restarting.',
  full: 'The engine had no room for it.',
};

/**
 * Send one batch. Refused (a rejection: native refuses a batch whole or runs it), it is sent once more; one
 * that got no answer in time is not (it may still run, and twice would repeat a looper press). Lost, it is
 * logged and toasted, and when it held a release a blur follows, which releases every hold of this page
 * natively (a note must not stick); a blur that is lost too rides at the head of the next batch. What
 * native MIDI dropped of a batch that ran (a press with no device running) is logged and toasted once
 * (`toldDropped`); input of a page native MIDI no longer counts as current is logged once per epoch. True
 * when the batch ran and nothing of it was dropped.
 */
async function submit(items: InputItem[]): Promise<boolean> {
  const batch = pendingBlur ? [BLUR, ...items] : items;
  let sent = await sendOnce(batch);
  if ('lost' in sent && !sent.timedOut) sent = await sendOnce(batch);
  if ('lost' in sent) {
    console.error('[platform] input batch failed', sent.lost);
    notifyError('The audio engine did not take a command', sent.lost);
    if (pendingBlur || items.some(releases)) {
      pendingBlur = true;
      const blur = await sendOnce([BLUR]);
      if ('lost' in blur) console.error('[platform] the blur after a lost release failed; the next batch carries it', blur.lost);
      else pendingBlur = false;
    }
    return false;
  }
  pendingBlur = false;
  const { epoch, dropped } = sent;
  if (dropped === 'stale') {
    if (toldStale !== epoch) {
      toldStale = epoch;
      console.error(`[platform] native MIDI refused this page's input: epoch ${epoch} is not the current page's`);
    }
  } else if (dropped !== null && dropped !== toldDropped) {
    console.error(`[platform] native MIDI dropped input: ${dropped}`);
    notifyError('The audio engine did not take a command', DROPPED_DETAIL[dropped]);
    toldDropped = dropped;
  } else if (dropped === null && items.some(press)) {
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
