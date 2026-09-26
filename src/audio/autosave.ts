import { notifyError } from '../notify';
import { encodeRecovery } from './export/recovery-encode';
import { exportBase } from './export/stem-archive';
import { importSession } from './export/import';
import { webSession, type SessionSource } from './export/session-source';
import type { PeakView } from './looper/looper';
import { framesPerBar } from './quantize';

const DB_NAME = 'bleeploop';
const DB_VERSION = 1;
const STORE = 'recovery';
const LATEST = 'latest';
const POLL_MS = 500;
const QUIET_MS = 2000;

interface RecoveryRecord {
  key: typeof LATEST;
  savedAt: number;
  bytes: ArrayBuffer;
}

interface JamFingerprint {
  value: string;
  blank: boolean;
  stable: boolean;
}

const peakViews: PeakView[] = Array.from({ length: webSession.trackCount }, () => ({
  min: null,
  max: null,
  count: 0,
  version: -1,
}));

let database: Promise<IDBDatabase> | null = null;
let operation: Promise<void> = Promise.resolve();
let readyPromise: Promise<void> = Promise.resolve();
let stopActive: (() => void) | null = null;
/** The looper the recovery reads and restores into (`start` sets it: the web looper, or the engine's). */
let source: SessionSource = webSession;
/** Whether `start` ever ran: before it, `flush` must not replace a recovery nothing has tried to restore. */
let started = false;
/** Preserve the last valid archive while live state still matches a failed restore's rollback. */
let failedRestoreFingerprint = '';

function openDatabase(): Promise<IDBDatabase> {
  if (database) return database;
  database = new Promise<IDBDatabase>((resolve, reject) => {
    const request = indexedDB.open(DB_NAME, DB_VERSION);
    request.onupgradeneeded = () => {
      if (!request.result.objectStoreNames.contains(STORE)) {
        request.result.createObjectStore(STORE, { keyPath: 'key' });
      }
    };
    request.onsuccess = () => {
      request.result.onversionchange = () => {
        request.result.close();
        database = null;
      };
      resolve(request.result);
    };
    request.onerror = () => reject(request.error ?? new Error('IndexedDB open failed'));
  }).catch((error) => {
    // Cache successful/in-flight opens, never a failure that would disable saving until reload.
    database = null;
    throw error;
  });
  return database;
}

function transactionDone(tx: IDBTransaction): Promise<void> {
  return new Promise((resolve, reject) => {
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error ?? new Error('IndexedDB transaction failed'));
    tx.onabort = () => reject(tx.error ?? new Error('IndexedDB transaction aborted'));
  });
}

function requestResult<T>(request: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error ?? new Error('IndexedDB request failed'));
  });
}

async function readLatest(): Promise<RecoveryRecord | null> {
  const db = await openDatabase();
  const tx = db.transaction(STORE, 'readonly');
  const done = transactionDone(tx);
  // Observe both failures immediately: an aborted request also rejects its transaction.
  const [value] = await Promise.all([requestResult(tx.objectStore(STORE).get(LATEST)), done]);
  if (value === undefined) return null;
  const record = value as Partial<RecoveryRecord>;
  if (record.key !== LATEST || typeof record.savedAt !== 'number' || !(record.bytes instanceof ArrayBuffer)) {
    throw new Error('Saved recovery data has an invalid shape');
  }
  return record as RecoveryRecord;
}

async function writeLatest(bytes: Uint8Array<ArrayBuffer> | null): Promise<void> {
  const db = await openDatabase();
  const tx = db.transaction(STORE, 'readwrite');
  const done = transactionDone(tx);
  const store = tx.objectStore(STORE);
  if (bytes) {
    const record: RecoveryRecord = { key: LATEST, savedAt: Date.now(), bytes: bytes.buffer };
    store.put(record);
  } else {
    store.delete(LATEST);
  }
  await done;
}

/** Serialize restore/save/delete so a slow IndexedDB write can never overtake a newer snapshot. */
function serialized<T>(task: () => Promise<T>): Promise<T> {
  const next = operation.then(task, task);
  operation = next.then(() => undefined, () => undefined);
  return next;
}

/** Cheap metadata-only dirty check. PCM is copied only inside persistCurrent(). */
function inspectJam(): JamFingerprint {
  const parts = [String(source.masterFramesValue())];
  let blank = source.masterFramesValue() === 0;
  let stable = true;

  for (let i = 0; i < source.trackCount; i++) {
    const state = source.stateOf(i);
    const info = source.trackInfo(i);
    const peaks = source.peaksInto(i, peakViews[i]);
    if (state !== 'EMPTY') blank = false;
    if (state === 'RECORDING' || state === 'OVERDUBBING') stable = false;
    parts.push(
      `${i}:${state}:${info.lengthFrames}:${peaks.version}:${source.trackVolume(i)}:${Number(source.trackMuted(i))}:` +
        JSON.stringify(source.fxState(i)),
    );
  }

  return { value: parts.join('|'), blank, stable };
}

async function persistCurrent(snapshot = inspectJam()): Promise<void> {
  if (failedRestoreFingerprint && snapshot.value === failedRestoreFingerprint) return;
  if (snapshot.blank) {
    await writeLatest(null);
    failedRestoreFingerprint = '';
    return;
  }
  // The tempo is locked while loops exist, so reading it beside the snapshot is safe.
  const bpm = source.bpm();
  // "Nothing committed" is decided BEFORE the grid is validated. A first take in flight is RECORDING, so
  // the fingerprint is not blank, yet no loop is committed and the master grid is still 0 frames — the
  // whole-bar check below would reject that as a save failure and the close guard would warn about losing
  // loops that never existed. No committed audio means there is nothing to save and no grid to validate.
  const committed = await source.exportSnapshot();
  if (committed.tracks.length === 0) {
    await writeLatest(null);
    failedRestoreFingerprint = '';
    return;
  }
  const masterFrames = committed.masterLengthFrames;
  const perBar = framesPerBar(bpm, committed.sampleRate);
  const bars = masterFrames / perBar;
  if (!Number.isInteger(bars) || bars < 1) {
    throw new Error(`Current loop grid is not whole bars (${masterFrames} frames at ${bpm} BPM)`);
  }
  // The snapshot holds copies of the PCM with each lane's state as it was read. The serialized save
  // queue keeps a slow encode/write from overtaking a newer save or CLEAR ALL.
  const bytes = await encodeRecovery({ snapshot: committed, meta: { bpm, bars }, base: exportBase() });
  await writeLatest(bytes);
  failedRestoreFingerprint = '';
}

async function restoreLatestImpl(): Promise<boolean> {
  try {
    const saved = await readLatest();
    if (!saved || !inspectJam().blank) {
      failedRestoreFingerprint = '';
      return false;
    }
    await importSession(saved.bytes, source);
    failedRestoreFingerprint = '';
    console.info(`[autosave] restored local recovery from ${new Date(saved.savedAt).toISOString()}`);
    return true;
  } catch (error) {
    const current = inspectJam();
    // Protect the previous archive only while the failed restore left the looper blank. If live
    // user work won a startup race, that jam must replace the old recovery on the next save.
    failedRestoreFingerprint = current.blank ? current.value : '';
    throw error;
  }
}

function reportFailure(message: string, error: unknown): void {
  console.error(`[autosave] ${message}`, error);
  notifyError(message, error);
}

/** Start recovery for `from` (default the web looper): restore the saved jam, then save on each quiet
 * change. Returns the stop. */
function start(from: SessionSource = webSession): () => void {
  if (stopActive) return stopActive;
  source = from;
  started = true;
  let stopped = false;
  let timer: ReturnType<typeof setInterval> | null = null;
  let observed = '';
  let saved = '';
  let quietSince = performance.now();
  let saving = false;
  let failedFingerprint = '';
  let saveInitial = false;

  readyPromise = serialized(restoreLatestImpl)
    .then((restored) => {
      // If a user-created/imported jam beat restore to the all-empty precondition, make that live
      // state the next autosave candidate instead of assuming the older IndexedDB record matches it.
      saveInitial = !restored && !inspectJam().blank;
    })
    .catch((error) => {
      // A user gesture can legitimately beat startup restore to the all-empty precondition. In that
      // case their live work wins and the next stable commit replaces the old recovery.
      saveInitial = !inspectJam().blank;
      if (!saveInitial) reportFailure('Jam recovery could not be restored', error);
    })
    .then(() => {
      if (stopped) return;
      const initial = inspectJam();
      observed = initial.value;
      saved = saveInitial ? '' : initial.value;
      quietSince = performance.now();

      timer = setInterval(() => {
        if (saving) return;
        const current = inspectJam();
        if (current.value !== observed) {
          observed = current.value;
          quietSince = performance.now();
          return;
        }
        if (
          !current.stable ||
          current.value === saved ||
          current.value === failedFingerprint ||
          performance.now() - quietSince < QUIET_MS
        ) return;

        saving = true;
        const fingerprint = current.value;
        void serialized(() => persistCurrent(current))
          .then(() => {
            saved = fingerprint;
            failedFingerprint = '';
          })
          .catch((error) => {
            // Do not toast/retry every 500 ms. A later state change gets one fresh attempt.
            failedFingerprint = fingerprint;
            reportFailure('Jam autosave failed', error);
          })
          .finally(() => {
            saving = false;
          });
      }, POLL_MS);
    });

  stopActive = () => {
    stopped = true;
    if (timer !== null) clearInterval(timer);
    timer = null;
    stopActive = null;
  };
  return stopActive;
}

/** Force the latest coherent committed state to disk, including deletion after CLEAR ALL. A no-op before
 * `start`: an empty looper that never tried the restore must not delete the saved jam (engine mode starts
 * recovery only once its device runs). */
async function flush(): Promise<void> {
  if (!started) return;
  await readyPromise;
  await serialized(() => persistCurrent());
}

export const autosave = {
  start,
  /** Resolves when the one startup restore attempt has completed. */
  ready: () => readyPromise,
  flush,
  restoreLatest: () => serialized(restoreLatestImpl),
  clearSaved: () => serialized(async () => {
    await writeLatest(null);
    failedRestoreFingerprint = '';
  }),
  hasSaved: async () => (await readLatest()) !== null,
} as const;
