/**
 * OWNS: local recovery of the jam (IndexedDB): the one restore at start, a save after each quiet change
 * of the committed loops, and what empties or keeps a saved jam.
 *
 * - A save holds the committed loops only, and lands while the player keeps capturing: a take in flight
 *   has no loop yet, and a lane mid-overdub is saved as its loop before the layer (the source's
 *   snapshot). It waits for `QUIET_MS` without a committed change, so a long dub costs one save.
 * - Only a player's clear empties the recovery (`SessionSource.playerCleared`). A looper that went empty
 *   because it was replaced (engine mode: another sample rate, a fault) keeps its last jam for the next
 *   launch.
 * - Jams are kept per sample rate, two deep. The engine cannot play loops at another rate (no
 *   resampling), so when a jam at a new rate first saves, the `LATEST` one at the old rate moves to
 *   `KEPT` instead of being overwritten, and a launch at its rate restores it. A clear empties only the
 *   running rate's jam.
 */
import { notifyError, notifyInfo } from '../notify';
import { encodeRecovery } from './export/recovery-encode';
import { exportBase } from './export/stem-archive';
import { importSession } from './export/import';
import { webSession, type SessionSource } from './export/session-source';
import { parseZip } from './export/unzip';
import { framesPerBar } from './quantize';

const DB_NAME = 'bleeploop';
const DB_VERSION = 1;
const STORE = 'recovery';
/** The jam this looper saves, and a clear deletes. */
const LATEST = 'latest';
/** The jam at another rate than `LATEST`'s, moved aside when a jam at a new rate first saved. */
const KEPT = 'kept';
const POLL_MS = 500;
const QUIET_MS = 2000;

type Slot = typeof LATEST | typeof KEPT;

interface RecoveryRecord {
  key: Slot;
  savedAt: number;
  /** The jam's sample rate. Records from before it was stored name it only in their archive. */
  rate?: number;
  bytes: ArrayBuffer;
}

interface JamFingerprint {
  /** The committed jam, cheaply: what a save would hold changes only when this does. */
  value: string;
  /** Every lane EMPTY: a restore may load into it. */
  blank: boolean;
  /** Some lane holds a committed loop. */
  committed: boolean;
}

/** The outcome of the startup restore; `keptAt`: the latest jam's rate when it is not this device's. */
interface Restore {
  restored: boolean;
  keptAt: number | null;
}

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
/** The rate of the jam in `LATEST`: null when there is none, undefined until read (or unreadable). */
let latestRate: number | null | undefined;

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

async function readRecord(slot: Slot): Promise<RecoveryRecord | null> {
  const db = await openDatabase();
  const tx = db.transaction(STORE, 'readonly');
  const done = transactionDone(tx);
  // Observe both failures immediately: an aborted request also rejects its transaction.
  const [value] = await Promise.all([requestResult(tx.objectStore(STORE).get(slot)), done]);
  if (value === undefined) return null;
  const record = value as Partial<RecoveryRecord>;
  if (
    record.key !== slot ||
    typeof record.savedAt !== 'number' ||
    !(record.bytes instanceof ArrayBuffer) ||
    (record.rate !== undefined && typeof record.rate !== 'number')
  ) {
    throw new Error('Saved recovery data has an invalid shape');
  }
  return record as RecoveryRecord;
}

/** A record's sample rate: stored, or read from its archive's session.json (undefined if unreadable). */
function recordRate(record: RecoveryRecord): number | undefined {
  if (record.rate !== undefined) return record.rate;
  try {
    const entry = parseZip(new Uint8Array(record.bytes)).find((e) => e.name.endsWith('-session.json'));
    const rate: unknown = entry && (JSON.parse(new TextDecoder().decode(entry.data)) as { sampleRate?: unknown }).sampleRate;
    return typeof rate === 'number' ? rate : undefined;
  } catch {
    return undefined;
  }
}

/** A known rate that is not `rate`: that jam cannot play here, and waits for a launch at its own. */
const elsewhere = (known: number | null | undefined, rate: number): known is number => typeof known === 'number' && known !== rate;

async function latestRateNow(): Promise<number | null | undefined> {
  if (latestRate === undefined) {
    const latest = await readRecord(LATEST);
    latestRate = latest ? recordRate(latest) : null;
  }
  return latestRate;
}

/** Save a jam at `rate` as the latest. One at another rate there moves to `KEPT` first (the transaction
 * reads it back, the only save that copies the old archive). */
async function saveLatest(bytes: Uint8Array<ArrayBuffer>, rate: number): Promise<void> {
  const from = await latestRateNow();
  const db = await openDatabase();
  const tx = db.transaction(STORE, 'readwrite');
  const done = transactionDone(tx);
  const store = tx.objectStore(STORE);
  const record: RecoveryRecord = { key: LATEST, savedAt: Date.now(), rate, bytes: bytes.buffer };
  if (elsewhere(from, rate)) {
    const previous = store.get(LATEST);
    previous.onsuccess = () => {
      const old = previous.result as RecoveryRecord | undefined;
      if (old) store.put({ ...old, key: KEPT, rate: from } satisfies RecoveryRecord);
      store.put(record);
    };
  } else {
    store.put(record);
  }
  await done;
  latestRate = rate;
}

/** A player's clear at `rate`: delete the latest jam, unless it is one at another rate (never theirs to
 * clear here). */
async function deleteLatest(rate: number): Promise<void> {
  if (elsewhere(await latestRateNow(), rate)) return;
  const db = await openDatabase();
  const tx = db.transaction(STORE, 'readwrite');
  const done = transactionDone(tx);
  tx.objectStore(STORE).delete(LATEST);
  await done;
  latestRate = null;
}

/** Swap the two slots: the kept jam, just restored, becomes the latest; the latest waits in `KEPT`. */
async function swapSlots(latest: RecoveryRecord | null, kept: RecoveryRecord): Promise<void> {
  const db = await openDatabase();
  const tx = db.transaction(STORE, 'readwrite');
  const done = transactionDone(tx);
  const store = tx.objectStore(STORE);
  store.put({ ...kept, key: LATEST } satisfies RecoveryRecord);
  if (latest) store.put({ ...latest, key: KEPT, rate: latestRate ?? latest.rate } satisfies RecoveryRecord);
  else store.delete(KEPT);
  await done;
}

/** Serialize restore/save/delete so a slow IndexedDB write can never overtake a newer snapshot. */
function serialized<T>(task: () => Promise<T>): Promise<T> {
  const next = operation.then(task, task);
  operation = next.then(() => undefined, () => undefined);
  return next;
}

/**
 * Cheap metadata-only dirty check of the committed jam. A lane with no loop (EMPTY, or a take in
 * flight) gives only its revision; a lane mid-overdub reads as the loop it plays, which the save holds
 * until the layer commits. PCM is copied only inside persistCurrent().
 */
function inspectJam(): JamFingerprint {
  const master = source.masterFramesValue();
  let blank = master === 0;
  let committed = false;
  const parts: string[] = [];

  for (let i = 0; i < source.trackCount; i++) {
    const state = source.stateOf(i);
    const revision = source.revision(i);
    if (state !== 'EMPTY') blank = false;
    if (state === 'EMPTY' || state === 'RECORDING') {
      parts.push(`${i}:-:${revision}`);
      continue;
    }
    committed = true;
    parts.push(
      `${i}:${state === 'OVERDUBBING' ? 'PLAYING' : state}:${source.trackInfo(i).lengthFrames}:${revision}:` +
        `${source.trackVolume(i)}:${Number(source.trackMuted(i))}:${JSON.stringify(source.fxState(i))}`,
    );
  }

  return { value: `${committed ? master : '-'}|${parts.join('|')}`, blank, committed };
}

/** Nothing is committed: a player's clear deletes the running rate's jam; a replaced looper keeps it. */
async function forgetJam(): Promise<void> {
  if (source.playerCleared()) await deleteLatest(source.sampleRate());
  failedRestoreFingerprint = '';
}

async function persistCurrent(snapshot = inspectJam()): Promise<void> {
  if (failedRestoreFingerprint && snapshot.value === failedRestoreFingerprint) return;
  // "Nothing committed" is decided BEFORE the grid is validated. A first take in flight has no loop and
  // the master grid is still 0 frames: the whole-bar check below would reject that as a save failure and
  // the close guard would warn about losing loops that never existed.
  if (!snapshot.committed) return forgetJam();
  // The tempo is locked while loops exist, so reading it beside the snapshot is safe.
  const bpm = source.bpm();
  const committed = await source.exportSnapshot();
  if (committed.tracks.length === 0) return forgetJam();
  const masterFrames = committed.masterLengthFrames;
  const perBar = framesPerBar(bpm, committed.sampleRate);
  const bars = masterFrames / perBar;
  if (!Number.isInteger(bars) || bars < 1) {
    throw new Error(`Current loop grid is not whole bars (${masterFrames} frames at ${bpm} BPM)`);
  }
  // The snapshot holds copies of the PCM with each lane's state as it was read. The serialized save
  // queue keeps a slow encode/write from overtaking a newer save or CLEAR ALL.
  const bytes = await encodeRecovery({ snapshot: committed, meta: { bpm, bars }, base: exportBase() });
  await saveLatest(bytes, committed.sampleRate);
  failedRestoreFingerprint = '';
}

/** Restore the jam at the running rate into a blank looper: the latest one, or else the one kept aside
 * at this rate (the slots then swap). */
async function restoreLatestImpl(): Promise<Restore> {
  try {
    const rate = source.sampleRate();
    const latest = await readRecord(LATEST);
    latestRate = latest ? recordRate(latest) : null;
    const kept = elsewhere(latestRate, rate) || !latest ? await readRecord(KEPT) : null;
    const saved = latest && !elsewhere(latestRate, rate) ? latest : kept && recordRate(kept) === rate ? kept : null;
    if (!saved || !inspectJam().blank) {
      failedRestoreFingerprint = '';
      return { restored: false, keptAt: !saved && elsewhere(latestRate, rate) ? latestRate : null };
    }
    await importSession(saved.bytes, source);
    if (saved === kept) {
      await swapSlots(latest, kept);
      latestRate = rate;
    }
    failedRestoreFingerprint = '';
    console.info(`[autosave] restored local recovery from ${new Date(saved.savedAt).toISOString()}`);
    return { restored: true, keptAt: null };
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

const kHz = (rate: number) => `${rate / 1000} kHz`;

/** The latest jam is at another rate than the device's: it stays, and the player hears why. */
function reportKept(rate: number): void {
  const running = source.sampleRate();
  console.info(`[autosave] the recovery holds loops recorded at ${rate} Hz and the device runs at ${running} Hz: kept`);
  notifyInfo(
    'Loops kept in recovery',
    `They were recorded at ${kHz(rate)}, and this audio device runs at ${kHz(running)}. ` +
      `Start BleepLoop on a ${kHz(rate)} device to get them back.`,
  );
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
    .then(({ restored, keptAt }) => {
      // If a user-created/imported jam beat restore to the all-empty precondition, make that live
      // state the next autosave candidate instead of assuming the older IndexedDB record matches it.
      saveInitial = !restored && !inspectJam().blank;
      if (keptAt !== null) reportKept(keptAt);
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
        if (current.value === saved || current.value === failedFingerprint || performance.now() - quietSince < QUIET_MS) return;

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

/** Force the latest coherent committed state to disk, including deletion after a player's CLEAR ALL. A
 * no-op before `start`: an empty looper that never tried the restore must not delete the saved jam (engine
 * mode starts recovery only once its device runs). */
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
  restoreLatest: () => serialized(async () => (await restoreLatestImpl()).restored),
  /** Delete every saved jam, the kept one too. */
  clearSaved: () => serialized(async () => {
    const db = await openDatabase();
    const tx = db.transaction(STORE, 'readwrite');
    const done = transactionDone(tx);
    tx.objectStore(STORE).delete(LATEST);
    tx.objectStore(STORE).delete(KEPT);
    await done;
    latestRate = null;
    failedRestoreFingerprint = '';
  }),
  /** A latest jam is saved (whatever its rate). */
  hasSaved: async () => (await readRecord(LATEST)) !== null,
} as const;
