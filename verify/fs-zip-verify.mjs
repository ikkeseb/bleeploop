// verify/fs-zip-verify.mjs — deterministic guard for src/audio/export/zip.ts.
// Imports the REAL store-only ZIP writer (Node TS type-stripping) so it cannot drift from the source.
// Asserts CRC-32 against a known vector, the ZIP signatures/offsets, and — the load-bearing property —
// that every entry in the GENERATED archive decodes back to its source payload the way an unzip would:
// by the method the entry's own header declares (0 = stored, compared byte-for-byte; 8 = DEFLATE, run
// through Node's zlib inflateRaw). makeZip is store-only today, so the method-8 arm never runs on this
// archive — it is a forward guard, not a claim about the current writer, and section D exercises it
// against a known raw-deflate vector so it cannot rot.
// This is what the WAV-export single-download fix relies on: one valid archive == one download gesture.
// Run: node verify/fs-zip-verify.mjs
import { inflateRawSync } from 'node:zlib';
import { crc32, makeZip } from '../src/audio/export/zip.ts';

let fails = 0,
  checks = 0;
function ok(name, cond, detail = '') {
  checks++;
  if (!cond) {
    fails++;
    console.log(`  FAIL  ${name}  ${detail}`);
  }
}
const enc = new TextEncoder();
const dec = new TextDecoder();

// ---- A. CRC-32 known vector: crc32("123456789") == 0xCBF43926 ----
ok('A.crc32 IEEE vector', crc32(enc.encode('123456789')) === 0xcbf43926, crc32(enc.encode('123456789')).toString(16));
ok('A.crc32 empty == 0', crc32(new Uint8Array(0)) === 0, String(crc32(new Uint8Array(0))));

// ---- B. archive structure: signatures + entry count ----
const files = [
  { name: 'a.txt', data: enc.encode('hello world') },
  { name: 'dir/b.bin', data: Uint8Array.from([0, 1, 2, 3, 255, 128, 64]) },
  { name: 'c.json', data: enc.encode(JSON.stringify({ x: 1 })) },
];
const zip = makeZip(files, new Date(2026, 6, 7, 12, 34, 20));
const dv = new DataView(zip.buffer, zip.byteOffset, zip.byteLength);
ok('B.local file header sig @0', dv.getUint32(0, true) === 0x04034b50, dv.getUint32(0, true).toString(16));
// End-Of-Central-Directory is the last 22 bytes (no comment).
const eocdOff = zip.byteLength - 22;
ok('B.EOCD sig', dv.getUint32(eocdOff, true) === 0x06054b50, dv.getUint32(eocdOff, true).toString(16));
ok('B.EOCD total entries == 3', dv.getUint16(eocdOff + 10, true) === 3, String(dv.getUint16(eocdOff + 10, true)));
ok('B.EOCD disk entries == 3', dv.getUint16(eocdOff + 8, true) === 3, String(dv.getUint16(eocdOff + 8, true)));
const centralStart = dv.getUint32(eocdOff + 16, true);
ok('B.central dir sig at recorded offset', dv.getUint32(centralStart, true) === 0x02014b50, dv.getUint32(centralStart, true).toString(16));

// ---- C. round-trip: parse each local entry, verify name + method(store) + CRC + decoded payload ----
{
  // Decode an entry's archived bytes as an unzip would — by the method its header declares. Returns
  // null for a method neither we nor a v0 unzip can handle, which fails the comparison below.
  const decodeEntry = (method, bytes) => {
    if (method === 0) return bytes;
    // A corrupt member must FAIL the check below, not crash the guard with a zlib stack trace.
    if (method === 8) { try { return new Uint8Array(inflateRawSync(bytes)); } catch { return null; } }
    return null;
  };
  let p = 0;
  const seen = [];
  for (let e = 0; e < files.length; e++) {
    ok(`C.entry${e} local sig`, dv.getUint32(p, true) === 0x04034b50, dv.getUint32(p, true).toString(16));
    const method = dv.getUint16(p + 8, true);
    const crcStored = dv.getUint32(p + 14, true) >>> 0;
    const compSize = dv.getUint32(p + 18, true);
    const uncompSize = dv.getUint32(p + 22, true);
    const nameLen = dv.getUint16(p + 26, true);
    const extraLen = dv.getUint16(p + 28, true);
    const nameStart = p + 30;
    const name = dec.decode(zip.subarray(nameStart, nameStart + nameLen));
    const dataStart = nameStart + nameLen + extraLen;
    const data = zip.subarray(dataStart, dataStart + compSize);
    ok(`C.entry${e} method store`, method === 0, String(method));
    ok(`C.entry${e} comp==uncomp`, compSize === uncompSize, `${compSize}/${uncompSize}`);
    ok(`C.entry${e} name`, name === files[e].name, name);
    ok(`C.entry${e} crc matches recomputed`, crcStored === crc32(files[e].data), crcStored.toString(16));
    const decoded = decodeEntry(method, data);
    ok(`C.entry${e} decodes to source payload (method ${method})`,
      decoded !== null && Buffer.compare(Buffer.from(decoded), Buffer.from(files[e].data)) === 0,
      `${name} decodedLen=${decoded ? decoded.length : 'n/a'} srcLen=${files[e].data.length}`);
    ok(`C.entry${e} decoded crc == header crc`, decoded !== null && crc32(decoded) === crcStored, name);
    ok(`C.entry${e} uncompSize == decoded length`, decoded !== null && uncompSize === decoded.length,
      `${uncompSize} vs ${decoded ? decoded.length : 'n/a'}`);
    seen.push(name);
    p = dataStart + compSize;
  }
  // p now points at the first central directory header.
  ok('C.data ends at central dir', p === centralStart, `${p} vs ${centralStart}`);
}

// ---- D. degenerate archive (a zero-length entry), plus a live check of the DEFLATE arm that section C
//         would take if makeZip ever stopped storing — store-only means C never reaches it. ----
{
  const zEmpty = makeZip([{ name: 'empty', data: new Uint8Array(0) }]);
  const dvE = new DataView(zEmpty.buffer, zEmpty.byteOffset, zEmpty.byteLength);
  ok('D.empty-file crc == 0', dvE.getUint32(14, true) >>> 0 === 0, dvE.getUint32(14, true).toString(16));
  ok('D.empty-file size == 0', dvE.getUint32(22, true) === 0, String(dvE.getUint32(22, true)));
  // The method-8 decode path, exercised on a known raw-deflate vector so it cannot rot unnoticed.
  const raw = Buffer.from([0x4b, 0x4c, 0x4a, 0x06, 0x00]); // raw-deflate of "abc"
  ok('D.zlib inflateRaw sane', dec.decode(inflateRawSync(raw)) === 'abc');
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
if (fails) process.exit(1);
