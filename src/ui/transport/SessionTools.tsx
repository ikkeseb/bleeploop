import { createMemo, createSignal } from 'solid-js';
import { exportLoops } from '../../session/export';
import { importSession, maxImportArchiveBytes } from '../../session/import';
import { restoreSessionTones } from '../state/slot-tones';
import { notifyError, notifyInfo } from '../../notify';
import { looper, sampleRate, session } from '../state/audio';
import { anyTrackIn } from '../looper/shared';
import { ExportIcon, ImportIcon } from './session-icons';

/**
 * EXPORT / IMPORT as two icon tools in the command bar's far-right tool cluster (app.tsx `.tools`).
 * They are file operations, not transport, and as labelled pills they pushed the bar past the viewport
 * at every width below 1600 (the Tauri default is 1280).
 *
 * Export every committed track (mono WAV) + the wet stereo master + session.json, and each
 * loaded plugin slot's tone. Import a previous export into an EMPTY looper — import never overwrites, so
 * it is enabled only while no master loop exists (the inverse of EXPORT) — then store the tones it
 * carries (`slot-tones.ts`). Both go through `session` (`src/ui/state/audio.ts`).
 */
export function SessionTools() {
  const hasMaster = () => looper.masterLengthFrames() > 0;
  const anyLive = createMemo(() => anyTrackIn('PLAYING', 'OVERDUBBING', 'RECORDING'));
  const anyCapturing = createMemo(() => anyTrackIn('RECORDING', 'OVERDUBBING'));
  // EXPORT needs committed audio and an idle capture path. A bare master length isn't enough —
  // clear-during-record can leave a length with zero committed tracks. Finish the current take/layer
  // before snapshotting the editable session; export.ts repeats this guard against races.
  const anyCommitted = createMemo(() => anyTrackIn('PLAYING', 'STOPPED', 'OVERDUBBING'));
  const [exporting, setExporting] = createSignal(false);
  // Busy from the file read until the tones are restored: until the feed shows the loaded lanes the
  // looper still reads empty, so without it a second pick could start a second import, whose refusal
  // toasts an import failure over the first one's success.
  const [importing, setImporting] = createSignal(false);

  // Async since v1 (the offline master render); render failures inside fall back to the dry mix — this
  // catch is the net for anything else (snapshot/zip/download), so a rejection never goes unhandled.
  const onExport = async () => {
    if (exporting() || !anyCommitted() || anyCapturing()) return;
    setExporting(true);
    try {
      const filename = await exportLoops(session);
      if (filename) notifyInfo(`Exported ${filename}`, 'One .zip with every track + the master as WAV. Look in your Downloads folder.');
    } catch (err) {
      console.error('[transport] export failed', err);
      notifyError('Could not export the loops. Export again to retry.', err);
    } finally {
      setExporting(false);
    }
  };

  // The hidden file input stays in the DOM (display:none, not unmounted) so Playwright can drive it.
  // `input.value` resets in finally so re-picking the same file re-fires `change`.
  let importInputRef: HTMLInputElement | undefined;
  const onImportPick = async (e: Event) => {
    const input = e.target as HTMLInputElement;
    const file = input.files?.[0];
    if (!file || importing()) {
      input.value = '';
      return;
    }
    setImporting(true);
    try {
      const limit = maxImportArchiveBytes(sampleRate());
      if (file.size > limit) {
        throw new Error(`archive is too large (${file.size} bytes; maximum ${limit})`);
      }
      const tones = await importSession(await file.arrayBuffer(), session);
      // No success toast — five lanes lighting up IS the feedback. The tones come after the loops: a
      // tone the host refuses toasts on its own and never takes the loops back out.
      await restoreSessionTones(tones);
    } catch (err) {
      console.error('[transport] import failed', err);
      // The reason line names the remedy where there is one (the rate to open it at, a newer build).
      notifyError('Could not import the session.', err);
    } finally {
      input.value = '';
      setImporting(false);
    }
  };

  return (
    <>
      <button
        type="button"
        class="tool tool--export"
        classList={{ 'tool--on': exporting() }}
        disabled={!anyCommitted() || anyCapturing() || exporting()}
        onClick={() => void onExport()}
        aria-busy={exporting()}
        aria-label="Export loops as a zip of WAV files"
        title="Export one .zip: each track + a master mix as WAV, plus session.json"
      >
        <ExportIcon />
      </button>
      <button
        type="button"
        class="tool tool--import"
        classList={{ 'tool--on': importing() }}
        disabled={hasMaster() || anyLive() || importing()}
        onClick={() => importInputRef?.click()}
        aria-busy={importing()}
        aria-label="Import a session zip"
        title="Import a BleepLoop export zip (stems + session.json), only while the looper is empty"
      >
        <ImportIcon />
      </button>
      <input
        ref={importInputRef}
        style={{ display: 'none' }}
        type="file"
        accept=".zip"
        aria-hidden="true"
        tabindex="-1"
        onChange={onImportPick}
      />
    </>
  );
}
