import { prepareStemArchive, type StemSnapshot } from './stem-archive';
import { makeZip } from './zip';

export interface RecoveryEncodeRequest {
  snapshot: StemSnapshot;
  meta: { bpm: number; bars: number };
  base: string;
}

export type RecoveryEncodeResult = { bytes: Uint8Array<ArrayBuffer> } | { error: string };

const worker = self as unknown as DedicatedWorkerGlobalScope;
worker.onmessage = ({ data }: MessageEvent<RecoveryEncodeRequest>) => {
  try {
    const { entries, session } = prepareStemArchive(data.snapshot, data.meta, data.base, 'float32');
    entries.push({
      name: `${data.base}-session.json`,
      data: new TextEncoder().encode(JSON.stringify(session, null, 2)),
    });
    const bytes = makeZip(entries);
    worker.postMessage({ bytes } satisfies RecoveryEncodeResult, [bytes.buffer]);
  } catch (error) {
    worker.postMessage({
      error: error instanceof Error ? error.message : String(error),
    } satisfies RecoveryEncodeResult);
  }
};
