/**
 * Runtime platform selection. `@tauri-apps/api` is pure JS, so importing it in a browser is safe
 * — `isTauri()` returns false and we hand back the web implementation. Under the Tauri shell
 * it returns true and we hand back `tauriPlatform`; nothing else in the app changes.
 */
import { isTauri } from '@tauri-apps/api/core';
import type { Platform } from './host';
import type { EngineCommand, NoteTarget } from './engine-wire';
import { encodeInput, type InputEvent } from './midi-wire';
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

/** A run of one kind in the outbox, and the promise its callers hold. */
type Batch =
  | { kind: 'engine'; items: EngineCommand[]; settle: (ok: boolean) => void; submitted: Promise<boolean> }
  | { kind: 'input'; items: InputEvent[]; settle: (ok: boolean) => void; submitted: Promise<boolean> };

let outbox: Batch[] = [];

/**
 * Queue `items` behind everything this task queued before. The outbox leaves a microtask later as one
 * `engine_send` or `input_send` call per run of one kind, issued back to back in the order they were
 * queued, so a slot pick and the note after it, or a looper press and a note, reach native code in the
 * order they happened (both calls run on the main thread natively, in arrival order). Nothing is sent
 * while the platform has no engine (the browser build without the DEV fake).
 */
function enqueue(kind: 'engine', items: EngineCommand[]): Promise<boolean>;
function enqueue(kind: 'input', items: InputEvent[]): Promise<boolean>;
function enqueue(kind: Batch['kind'], items: (EngineCommand | InputEvent)[]): Promise<boolean> {
  if (!platform.engine.available) return Promise.resolve(false);
  if (items.length === 0) return Promise.resolve(true);
  if (outbox.length === 0) queueMicrotask(flush);
  let last = outbox.at(-1);
  if (last?.kind !== kind) {
    let settle!: (ok: boolean) => void;
    const submitted = new Promise<boolean>((resolve) => (settle = resolve));
    last = { kind, items: [], settle, submitted } as Batch;
    outbox.push(last);
  }
  (last.items as (EngineCommand | InputEvent)[]).push(...items);
  return last.submitted;
}

function flush(): void {
  const batches = outbox;
  outbox = [];
  for (const batch of batches) {
    const call = batch.kind === 'engine' ? platform.engine.send(batch.items) : platform.input.send(batch.items);
    call.then(
      () => batch.settle(true),
      (err: unknown) => {
        if (batch.kind === 'engine') {
          console.error('[platform] engine command batch failed', err);
          notifyError('The audio engine did not take a command', err);
        } else {
          console.error('[platform] input batch failed', err);
          notifyError('The audio engine did not take a note', err);
        }
        batch.settle(false);
      },
    );
  }
}

/**
 * Queue commands for the engine. What one task sends leaves a microtask later, in one ordered `engine_send`
 * batch per run between input events (`enqueue`), so a gesture's commands reach the same block together. A
 * batch the host could not take is logged and toasted. Notes, wheels, the note target and the panic go
 * through `input`, never here: native MIDI's router refuses them on `engine_send`.
 *
 * Resolves with the SUBMISSION of the batch these commands left in: true once the host took all of it,
 * false when it did not wholly take it (or there is no engine). False is no per-command answer: the
 * native host pushes a batch command by command and stops at the first it refuses, keeping the prefix it
 * took, so a command of a failed batch may still reach the engine. Never an acknowledgement either: the
 * engine applies a command later, and its outcome arrives on the feed. Most callers ignore it; a mix
 * gesture drops its overlay on false, and a command the engine did take brings the value back through
 * its lane's `Mix` (`engine-store.ts`).
 */
export function sendEngine(...commands: EngineCommand[]): Promise<boolean> {
  return enqueue('engine', commands);
}

/**
 * The UI's note sources into native MIDI's one router (`InputHost`), queued in one order with
 * `sendEngine`. What sounds, sustain, the wheels and which owner holds a note are the router's
 * (`src-tauri/src/engine_io/midi/router.rs`); the UI says what its pointers and keys did.
 */
export const input = {
  /** Pointer or key `owner` (`pointer:<id>`, `key:<code>`) pressed (`on`) or let go of `note`, at MIDI
   * velocity 0..127. */
  note(owner: string, note: number, velocity: number, on: boolean): void {
    void enqueue('input', [encodeInput.note(owner, note, velocity, on)]);
  },
  /** The window lost focus: this document's pointers and keys are up. */
  blur(): void {
    void enqueue('input', [encodeInput.blur()]);
  },
  /** Route the notes to `target`, picked on `slot` (null: none). What sounds is released first; the same
   * slot and target again change nothing (natively), so a call per press is cheap. */
  selectTarget(slot: 0 | 1 | null, target: NoteTarget): void {
    void enqueue('input', [encodeInput.selectTarget(slot, target)]);
  },
  /** Panic: every note that sounds is released and forgotten. */
  allNotesOff(): void {
    void enqueue('input', [encodeInput.allNotesOff()]);
  },
};

/** The web engine fake a probe scripts through `__lf.native`; null under Tauri. */
export const engineFake: EngineFake | null = underTauri ? null : webEngineFake;

export * from './host';
export * from './engine-wire';
export * from './midi-wire';
