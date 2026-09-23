// src/audio/export/unzip.ts
// Minimal store-only (no compression) ZIP container reader — the exact inverse of makeZip in ./zip.ts.
// PURE — no Web Audio / DOM / Tauri imports, so a Node verify guard can import it directly
// (verify/guards/unzip.mjs) and it builds/verifies on the Mac half. Exists as the round-trip mate to the
// writer: it lets a guard prove makeZip → parseZip is lossless, and gives any consumer a dependency-free way
// to read the export archives back.
//
// Deliberately does NOT support: any compression method other than 0/store (DEFLATE etc. throw), ZIP64
// (>4 GiB or >65535 entries), encryption, multi-disk archives, data descriptors (bit 3 of the general-purpose
// flags), or archive comments beyond what EOCD scanning tolerates. It reads the Central Directory as the
// authoritative index (walking 0x02014b50 records), follows each entry's local-header offset (0x04034b50),
// and validates hard: every field read is bounds-checked, signatures must match, method must be store, and
// each entry's CRC-32 is recomputed (via crc32) and compared. Names are decoded with TextDecoder (the writer
// encodes them with TextEncoder).

import { crc32, type ZipEntry } from './zip.ts';

const SIG_LOCAL = 0x04034b50;
const SIG_CENTRAL = 0x02014b50;
const SIG_EOCD = 0x06054b50;

/** Bounds-checked little-endian reads over a DataView; each throws on a truncated/short buffer. */
function u16(dv: DataView, off: number): number {
  if (off < 0 || off + 2 > dv.byteLength) throw new Error(`truncated archive: 16-bit read at ${off} exceeds ${dv.byteLength} bytes`);
  return dv.getUint16(off, true);
}
function u32(dv: DataView, off: number): number {
  if (off < 0 || off + 4 > dv.byteLength) throw new Error(`truncated archive: 32-bit read at ${off} exceeds ${dv.byteLength} bytes`);
  return dv.getUint32(off, true) >>> 0;
}

/**
 * Parse a store-only ZIP archive into its entries. Returns one ZipEntry per file, in central-directory order,
 * with the name decoded and the raw stored bytes exposed as views. Throws Error (never returns garbage or hangs) on
 * any malformed input: missing/bad signatures, a compression method other than store, a per-entry CRC-32
 * mismatch, inconsistent size fields, or a truncated buffer.
 */
export function parseZip(bytes: Uint8Array, maxEntries = 65535): ZipEntry[] {
  const dv = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const dec = new TextDecoder();

  // --- Locate the End Of Central Directory record by scanning BACKWARDS for its signature. Robust even with
  //     a trailing comment (our own writer emits none); we take the LAST match so a comment that happens to
  //     contain the signature bytes can't shadow the real record. EOCD is 22 bytes minimum. ---
  const minEocd = 22;
  if (bytes.byteLength < minEocd) throw new Error(`truncated archive: ${bytes.byteLength} bytes, need at least ${minEocd} for EOCD`);
  let eocd = -1;
  for (let i = bytes.byteLength - minEocd; i >= 0; i--) {
    if (dv.getUint32(i, true) === SIG_EOCD) {
      // Verify the recorded comment length lands exactly at end-of-buffer — rejects a coincidental signature.
      const commentLen = dv.getUint16(i + 20, true);
      if (i + minEocd + commentLen === bytes.byteLength) {
        eocd = i;
        break;
      }
    }
  }
  if (eocd < 0) throw new Error('bad archive: End Of Central Directory signature (0x06054b50) not found');

  const totalEntries = u16(dv, eocd + 10);
  // Bound work before following entries or computing CRCs: many directory records can point to
  // the same large payload, so archive byte length alone does not bound decoding CPU.
  if (totalEntries > maxEntries) {
    throw new Error(`archive has ${totalEntries} entries; maximum is ${maxEntries}`);
  }
  const centralSize = u32(dv, eocd + 12);
  const centralStart = u32(dv, eocd + 16);
  if (centralStart + centralSize > eocd) {
    throw new Error(`inconsistent archive: central directory [${centralStart}..${centralStart + centralSize}) overruns EOCD at ${eocd}`);
  }

  const entries: ZipEntry[] = [];
  let p = centralStart;
  for (let e = 0; e < totalEntries; e++) {
    if (u32(dv, p) !== SIG_CENTRAL) {
      throw new Error(`bad central directory: entry ${e} signature 0x${u32(dv, p).toString(16)} !== 0x02014b50 at offset ${p}`);
    }
    const method = u16(dv, p + 10);
    if (method !== 0) throw new Error(`unsupported compression method ${method} for entry ${e} (only store/0 is supported)`);
    const crcStored = u32(dv, p + 16);
    const compSize = u32(dv, p + 20);
    const uncompSize = u32(dv, p + 24);
    if (compSize !== uncompSize) {
      throw new Error(`inconsistent sizes for entry ${e}: compressed ${compSize} !== uncompressed ${uncompSize} (store method requires equality)`);
    }
    const nameLen = u16(dv, p + 28);
    const extraLen = u16(dv, p + 30);
    const commentLen = u16(dv, p + 32);
    const localOff = u32(dv, p + 42);
    const nameStart = p + 46;
    if (nameStart + nameLen > bytes.byteLength) throw new Error(`truncated archive: central name for entry ${e} exceeds buffer`);
    const name = dec.decode(bytes.subarray(nameStart, nameStart + nameLen));

    // --- Follow the local header. It is the authoritative source of the stored bytes. ---
    if (u32(dv, localOff) !== SIG_LOCAL) {
      throw new Error(`bad local header: entry ${e} ("${name}") signature 0x${u32(dv, localOff).toString(16)} !== 0x04034b50 at offset ${localOff}`);
    }
    const lMethod = u16(dv, localOff + 8);
    if (lMethod !== 0) throw new Error(`unsupported compression method ${lMethod} for entry ${e} ("${name}") in local header (only store/0 is supported)`);
    const lCrc = u32(dv, localOff + 14);
    const lCompSize = u32(dv, localOff + 18);
    const lUncompSize = u32(dv, localOff + 22);
    const lNameLen = u16(dv, localOff + 26);
    const lExtraLen = u16(dv, localOff + 28);
    if (lCompSize !== lUncompSize) {
      throw new Error(`inconsistent sizes for entry ${e} ("${name}") local header: ${lCompSize} !== ${lUncompSize}`);
    }
    if (lCompSize !== compSize) {
      throw new Error(`inconsistent archive: entry ${e} ("${name}") size ${lCompSize} in local header != ${compSize} in central directory`);
    }
    const dataStart = localOff + 30 + lNameLen + lExtraLen;
    if (dataStart + lCompSize > bytes.byteLength) {
      throw new Error(`truncated archive: entry ${e} ("${name}") data [${dataStart}..${dataStart + lCompSize}) exceeds ${bytes.byteLength} bytes`);
    }
    const data = bytes.subarray(dataStart, dataStart + lCompSize);

    const crcActual = crc32(data);
    if (crcActual !== lCrc || crcActual !== crcStored) {
      throw new Error(
        `CRC-32 mismatch for entry ${e} ("${name}"): computed 0x${crcActual.toString(16)}, local header 0x${lCrc.toString(16)}, central 0x${crcStored.toString(16)}`,
      );
    }

    entries.push({ name, data });
    p = nameStart + nameLen + extraLen + commentLen;
  }

  return entries;
}
