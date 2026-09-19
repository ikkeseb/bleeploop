import { createMemo, createSignal } from 'solid-js';
import { clock } from '../../audio/clock';
import { engine } from '../../audio/engine';
import { exportLoops } from '../../audio/export/export';
import { importSession, maxImportArchiveBytes } from '../../audio/export/import';
import { looper } from '../../audio/looper/looper';
import { notifyError } from '../../notify';
import { anyTrackIn, masterBars } from '../looper/Looper';

/**
 * EXPORT / IMPORT as two icon tools in the command bar's far-right tool cluster (app.tsx `.tools`).
 * They are file operations, not transport, and as labelled pills they pushed the bar past the viewport
 * at every width below 1600 (the Tauri default is 1280).
 *
 * Export every committed track (mono WAV) + the wet stereo master + session.json. Import a previous
 * export into an EMPTY looper — import never overwrites, so it is enabled only while no master loop
 * exists (the inverse of EXPORT).
 */
export function SessionTools() {
  const hasMaster = () => looper.masterLengthFrames() > 0;
  const loopBars = () => masterBars(looper.masterLengthFrames(), clock.bpm(), engine.ctx.sampleRate);
  const anyLive = createMemo(() => anyTrackIn('PLAYING', 'OVERDUBBING', 'RECORDING'));
  const anyCapturing = createMemo(() => anyTrackIn('RECORDING', 'OVERDUBBING'));
  // EXPORT needs committed audio and an idle capture path. A bare master length isn't enough —
  // clear-during-record can leave a length with zero committed tracks. Finish the current take/layer
  // before snapshotting the editable session; export.ts repeats this guard against races.
  const anyCommitted = createMemo(() => anyTrackIn('PLAYING', 'STOPPED', 'OVERDUBBING'));
  const [exporting, setExporting] = createSignal(false);

  // Async since v1 (the offline master render); render failures inside fall back to the dry mix — this
  // catch is the net for anything else (snapshot/zip/download), so a rejection never goes unhandled.
  const onExport = async () => {
    if (exporting() || !anyCommitted() || anyCapturing()) return;
    setExporting(true);
    try {
      await exportLoops({ bpm: clock.bpm(), bars: loopBars() });
    } catch (err) {
      console.error('[transport] export failed', err);
      notifyError('Export failed', err);
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
    if (!file) return;
    try {
      const limit = maxImportArchiveBytes(engine.ctx.sampleRate);
      if (file.size > limit) {
        throw new Error(`archive is too large (${file.size} bytes; maximum ${limit})`);
      }
      await importSession(await file.arrayBuffer());
      // No success toast — five lanes lighting up IS the feedback.
    } catch (err) {
      console.error('[transport] import failed', err);
      notifyError('Import failed: ' + (err instanceof Error ? err.message : String(err)));
    } finally {
      input.value = '';
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
        aria-label="Export loops as WAV files"
        title="Export each track + a master mix as WAV, plus session.json"
      >
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true">
          <path d="M12 4v11M7.5 10.5 12 15l4.5-4.5" stroke-linecap="round" stroke-linejoin="round" />
          <path d="M4 17v2.5h16V17" stroke-linecap="round" stroke-linejoin="round" />
        </svg>
      </button>
      <button
        type="button"
        class="tool tool--import"
        disabled={hasMaster() || anyLive()}
        onClick={() => importInputRef?.click()}
        aria-label="Import a session zip"
        title="Import a BleepLoop export zip (stems + session.json), only while the looper is empty"
      >
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true">
          <path d="M12 15V4M7.5 8.5 12 4l4.5 4.5" stroke-linecap="round" stroke-linejoin="round" />
          <path d="M4 17v2.5h16V17" stroke-linecap="round" stroke-linejoin="round" />
        </svg>
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
