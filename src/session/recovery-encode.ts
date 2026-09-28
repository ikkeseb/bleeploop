import RecoveryWorker from './recovery-worker?worker';
import type { RecoveryEncodeRequest, RecoveryEncodeResult } from './recovery-worker';

const ENCODE_TIMEOUT_MS = 30_000;

/** Owns the copied snapshot buffers. Success and failure both release the worker and its memory. */
export function encodeRecovery(request: RecoveryEncodeRequest): Promise<Uint8Array<ArrayBuffer>> {
  return new Promise((resolve, reject) => {
    const worker = new RecoveryWorker();
    let settled = false;
    const finish = (error?: Error, bytes?: Uint8Array<ArrayBuffer>) => {
      if (settled) return;
      settled = true;
      clearTimeout(timeout);
      worker.terminate();
      if (error) reject(error);
      else resolve(bytes!);
    };
    const timeout = setTimeout(() => finish(new Error('Recovery encoding timed out')), ENCODE_TIMEOUT_MS);
    worker.onmessage = ({ data }: MessageEvent<RecoveryEncodeResult>) => {
      if (!data || typeof data !== 'object') finish(new Error('Recovery worker returned invalid data'));
      else if ('error' in data) finish(new Error(String(data.error)));
      else if (data.bytes instanceof Uint8Array && data.bytes.buffer instanceof ArrayBuffer) finish(undefined, data.bytes);
      else finish(new Error('Recovery worker returned invalid data'));
    };
    worker.onerror = (event) => {
      event.preventDefault();
      finish(new Error(event.message || 'Recovery worker failed'));
    };
    worker.onmessageerror = () => finish(new Error('Recovery worker response could not be read'));
    try {
      // exportSnapshot() already copied committed PCM synchronously. Transfer those copies only;
      // live track buffers stay attached, and later edits cannot change this archive mid-encode.
      worker.postMessage(request, request.snapshot.tracks.map((t) => t.pcm.buffer as ArrayBuffer));
    } catch (error) {
      finish(error instanceof Error ? error : new Error(String(error)));
    }
  });
}
