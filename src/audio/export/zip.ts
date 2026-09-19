// src/audio/export/zip.ts
// Minimal store-only (no compression) ZIP container writer. PURE — no Web Audio / DOM / Tauri imports,
// so a Node verify guard can import it directly (verify/fs-zip-verify.mjs) and it builds/verifies on the
// Mac half. Exists to bundle the WAV-export deliverable into ONE file: browsers (and WebView2) gate more
// than one programmatic download per user gesture, so firing N+2 separate <a download> clicks silently
// drops every file after the first. Packing the stems + master + session.json into a single .zip is one
// download, one gesture — the whole export always reaches the user. Store method (0) avoids a DEFLATE
// dependency; WAV/JSON are already effectively incompressible-enough for a v0 export.

/** Precomputed CRC-32 (IEEE 802.3, polynomial 0xEDB88320) lookup table. */
const CRC_TABLE = (() => {
  const t = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c >>> 0;
  }
  return t;
})();

/** CRC-32 of a byte array (unsigned 32-bit). */
export function crc32(bytes: Uint8Array): number {
  let c = 0xffffffff;
  for (let i = 0; i < bytes.length; i++) c = CRC_TABLE[(c ^ bytes[i]) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

export interface ZipEntry {
  /** File name inside the archive (forward slashes for any folders; ASCII names only for v0). */
  name: string;
  data: Uint8Array;
}

/**
 * Build a valid store-only (uncompressed) ZIP archive from `entries`. `date` stamps the DOS
 * modified-time field (defaults to now). Returns the complete archive bytes: a Local File Header +
 * name + data per entry, then the Central Directory, then the End Of Central Directory record.
 */
export function makeZip(entries: ZipEntry[], date = new Date()): Uint8Array<ArrayBuffer> {
  const enc = new TextEncoder();
  // DOS time/date: 2-second resolution; year is offset from 1980.
  const dosTime =
    ((date.getHours() & 0x1f) << 11) | ((date.getMinutes() & 0x3f) << 5) | ((date.getSeconds() >> 1) & 0x1f);
  const yr = date.getFullYear() - 1980;
  const dosDate = (((yr < 0 ? 0 : yr) & 0x7f) << 9) | (((date.getMonth() + 1) & 0xf) << 5) | (date.getDate() & 0x1f);

  const parts: Uint8Array[] = [];
  const central: Uint8Array[] = [];
  let offset = 0; // running offset of each local header from the start of the archive

  for (const e of entries) {
    const nameBytes = enc.encode(e.name);
    const crc = crc32(e.data);
    const size = e.data.length;

    const lh = new DataView(new ArrayBuffer(30));
    lh.setUint32(0, 0x04034b50, true); // local file header signature
    lh.setUint16(4, 20, true); // version needed to extract (2.0)
    lh.setUint16(6, 0, true); // general purpose flags
    lh.setUint16(8, 0, true); // compression method: 0 = store
    lh.setUint16(10, dosTime, true);
    lh.setUint16(12, dosDate, true);
    lh.setUint32(14, crc, true);
    lh.setUint32(18, size, true); // compressed size (== uncompressed for store)
    lh.setUint32(22, size, true); // uncompressed size
    lh.setUint16(26, nameBytes.length, true);
    lh.setUint16(28, 0, true); // extra field length
    const lhBytes = new Uint8Array(lh.buffer);
    parts.push(lhBytes, nameBytes, e.data);

    const ch = new DataView(new ArrayBuffer(46));
    ch.setUint32(0, 0x02014b50, true); // central directory header signature
    ch.setUint16(4, 20, true); // version made by
    ch.setUint16(6, 20, true); // version needed
    ch.setUint16(8, 0, true); // flags
    ch.setUint16(10, 0, true); // method: store
    ch.setUint16(12, dosTime, true);
    ch.setUint16(14, dosDate, true);
    ch.setUint32(16, crc, true);
    ch.setUint32(20, size, true);
    ch.setUint32(24, size, true);
    ch.setUint16(28, nameBytes.length, true);
    ch.setUint16(30, 0, true); // extra length
    ch.setUint16(32, 0, true); // comment length
    ch.setUint16(34, 0, true); // disk number start
    ch.setUint16(36, 0, true); // internal attrs
    ch.setUint32(38, 0, true); // external attrs
    ch.setUint32(42, offset, true); // relative offset of local header
    central.push(new Uint8Array(ch.buffer), nameBytes);

    offset += lhBytes.length + nameBytes.length + size;
  }

  const centralStart = offset;
  let centralSize = 0;
  for (const c of central) centralSize += c.length;

  const eocd = new DataView(new ArrayBuffer(22));
  eocd.setUint32(0, 0x06054b50, true); // end of central directory signature
  eocd.setUint16(4, 0, true); // disk number
  eocd.setUint16(6, 0, true); // disk with central dir
  eocd.setUint16(8, entries.length, true); // entries on this disk
  eocd.setUint16(10, entries.length, true); // total entries
  eocd.setUint32(12, centralSize, true);
  eocd.setUint32(16, centralStart, true);
  eocd.setUint16(20, 0, true); // comment length

  const all = [...parts, ...central, new Uint8Array(eocd.buffer)];
  let total = 0;
  for (const p of all) total += p.length;
  const out = new Uint8Array(total);
  let pos = 0;
  for (const p of all) {
    out.set(p, pos);
    pos += p.length;
  }
  return out;
}
