// verify/fs-unzip-verify.mjs — deterministic guard for src/audio/export/unzip.ts.
// Imports the REAL store-only ZIP reader AND writer (Node TS type-stripping) so neither can drift from source.
// Proves the load-bearing property: makeZip → parseZip is lossless (identical names + byte-identical data),
// and that every hard-validation path (bad signature, corrupt data via CRC, truncation, non-store method)
// THROWS rather than returning garbage or hanging. Pure Node — no browser, AudioContext, or hardware.
// Run: node verify/fs-unzip-verify.mjs
import { parseZip } from '../src/audio/export/unzip.ts';
import { makeZip, crc32 } from '../src/audio/export/zip.ts';

let fails = 0,
  checks = 0;
function ok(name, cond, detail = '') {
  checks++;
  if (!cond) {
    fails++;
    console.log(`  FAIL  ${name}  ${detail}`);
  }
}
/** Assert that `fn` throws, optionally that the message contains `substr`. */
function throws(name, fn, substr = '') {
  checks++;
  let threw = false,
    msg = '';
  try {
    fn();
  } catch (e) {
    threw = true;
    msg = e instanceof Error ? e.message : String(e);
  }
  if (!threw) {
    fails++;
    console.log(`  FAIL  ${name}  did not throw`);
  } else if (substr && !msg.includes(substr)) {
    fails++;
    console.log(`  FAIL  ${name}  threw but message lacks "${substr}": ${msg}`);
  }
}

const enc = new TextEncoder();

// Seeded LCG (Numerical Recipes constants) → deterministic pseudo-random bytes. NO Math.random.
function lcgBytes(n, seed = 0x1234abcd) {
  const out = new Uint8Array(n);
  let s = seed >>> 0;
  for (let i = 0; i < n; i++) {
    s = (Math.imul(s, 1664525) + 1013904223) >>> 0;
    out[i] = (s >>> 24) & 0xff;
  }
  return out;
}

// ---- A. round-trip: makeZip → parseZip returns identical names + byte-identical data ----
const files = [
  { name: 'empty.bin', data: new Uint8Array(0) },
  { name: 'one.bin', data: Uint8Array.from([0x42]) },
  { name: 'dir/rand.bin', data: lcgBytes(4096) },
  { name: 'session.json', data: enc.encode(JSON.stringify({ bpm: 120, tracks: 5 })) },
];
const zip = makeZip(files, new Date(2026, 6, 10, 9, 30, 0));
const parsed = parseZip(zip);
ok('A.entry count', parsed.length === files.length, `${parsed.length} vs ${files.length}`);
for (let i = 0; i < files.length; i++) {
  ok(`A.entry${i} name`, parsed[i]?.name === files[i].name, parsed[i]?.name);
  const a = parsed[i]?.data,
    b = files[i].data;
  ok(`A.entry${i} length`, a?.length === b.length, `${a?.length} vs ${b.length}`);
  ok(`A.entry${i} bytes identical`, a && Buffer.compare(Buffer.from(a), Buffer.from(b)) === 0, files[i].name);
}
// The reader must recompute CRC and accept a clean archive without complaint (implicit in no-throw above).
ok('A.crc32 IEEE vector (import sanity)', crc32(enc.encode('123456789')) === 0xcbf43926);

// ---- B. corruption / truncation must THROW (never hang, never return garbage) ----
const dvZip = new DataView(zip.buffer, zip.byteOffset, zip.byteLength);
const eocdOff = zip.byteLength - 22; // no comment
const centralStart = dvZip.getUint32(eocdOff + 16, true);

// B1. corrupt one data byte → CRC-32 catches it. centralStart-1 is the last byte of the last entry's data.
{
  const bad = zip.slice();
  bad[centralStart - 1] ^= 0xff;
  throws('B.corrupt data byte → CRC throw', () => parseZip(bad), 'CRC-32');
}
// B2. corrupt the EOCD signature → not found.
{
  const bad = zip.slice();
  bad[eocdOff] ^= 0xff;
  throws('B.corrupt EOCD sig → throw', () => parseZip(bad), 'End Of Central Directory');
}
// B3. corrupt a central-directory signature → throw.
{
  const bad = zip.slice();
  bad[centralStart] ^= 0xff;
  throws('B.corrupt central sig → throw', () => parseZip(bad));
}
// B4. corrupt the first local-header signature → throw.
{
  const bad = zip.slice();
  bad[0] ^= 0xff;
  throws('B.corrupt local sig → throw', () => parseZip(bad), 'local header');
}
// B5. truncate the buffer at several offsets → throw, never hang.
for (const cut of [0, 4, 15, 30, Math.floor(zip.byteLength / 2), zip.byteLength - 22, zip.byteLength - 10, zip.byteLength - 1]) {
  throws(`B.truncate@${cut} → throw`, () => parseZip(zip.slice(0, cut)));
}

// ---- C. non-store compression method must be rejected ----
// The central-directory method field of entry 0 sits at centralStart+10 (u16). Patch it to 8 (DEFLATE).
{
  const bad = zip.slice();
  const dv = new DataView(bad.buffer, bad.byteOffset, bad.byteLength);
  dv.setUint16(centralStart + 10, 8, true);
  throws('C.central method=8 → throw', () => parseZip(bad), 'unsupported compression');
}
// Also patch the LOCAL header method (central store) to exercise the local-side check. Local header of the
// first entry is at offset 0; method field is at +8.
{
  const bad = zip.slice();
  const dv = new DataView(bad.buffer, bad.byteOffset, bad.byteLength);
  dv.setUint16(8, 8, true); // local method → DEFLATE, central stays store
  throws('C.local method=8 → throw', () => parseZip(bad), 'unsupported compression');
}

console.log(`\n=== RESULT: ${checks - fails}/${checks} checks passed, ${fails} failed ===`);
if (fails) process.exit(1);
