//! The v0.1.0 session files Stage 5 must import (`tests/fixtures/v0.1.0`, written by
//! `verify/probes/export-refs.mjs --write`): the downloaded export zip and the recovery archive.
//! This proves the fixtures are what their manifest says and that the layout Stage 5 reads is readable
//! without a zip crate: a store-only zip (EOCD, central directory, local headers, CRC-32), float32 mono
//! stems, a PCM16 stereo master, and a session.json whose grid and track list agree with the stems.

use lf_engine::grid::frames_per_bar;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v0.1.0")
}

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn u16_at(b: &[u8], at: usize) -> usize {
    u16::from_le_bytes([b[at], b[at + 1]]) as usize
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// CRC-32 (IEEE, reflected 0xEDB88320), bitwise: zip.ts's table form gives the same value.
fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for &byte in data {
        c ^= byte as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

/// The entries of a store-only zip as zip.ts writes it (no archive comment, no extra fields), in
/// central-directory order, each checked against its local header and CRC.
fn entries(zip: &[u8]) -> Vec<(String, &[u8])> {
    let eocd = zip.len() - 22;
    assert_eq!(u32_at(zip, eocd), 0x0605_4b50, "EOCD signature");
    assert_eq!(u16_at(zip, eocd + 20), 0, "archive comment");
    let count = u16_at(zip, eocd + 10);
    let (size, start) = (u32_at(zip, eocd + 12) as usize, u32_at(zip, eocd + 16) as usize);
    assert_eq!(start + size, eocd, "central directory ends at the EOCD");
    let mut out = Vec::new();
    let mut p = start;
    for _ in 0..count {
        assert_eq!(u32_at(zip, p), 0x0201_4b50, "central header signature");
        assert_eq!(u16_at(zip, p + 10), 0, "store method");
        let (crc, len) = (u32_at(zip, p + 16), u32_at(zip, p + 20) as usize);
        assert_eq!(u32_at(zip, p + 24) as usize, len, "stored size");
        let name_len = u16_at(zip, p + 28);
        let name = String::from_utf8(zip[p + 46..p + 46 + name_len].to_vec()).unwrap();
        let local = u32_at(zip, p + 42) as usize;
        assert_eq!(u32_at(zip, local), 0x0403_4b50, "local header signature");
        assert_eq!(&zip[local + 30..local + 30 + name_len], name.as_bytes(), "local name");
        let data_at = local + 30 + u16_at(zip, local + 26) + u16_at(zip, local + 28);
        let data = &zip[data_at..data_at + len];
        assert_eq!(crc32(data), crc, "{name}: CRC-32");
        out.push((name, data));
        p += 46 + name_len + u16_at(zip, p + 30) + u16_at(zip, p + 32);
    }
    assert_eq!(p, eocd);
    out
}

/// (format tag, channels, sample rate, bits, data bytes) from the WAV's fmt and data chunks.
fn wav(bytes: &[u8]) -> (usize, usize, u32, usize, usize) {
    assert_eq!(&bytes[0..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    let (mut fmt, mut data) = (None, None);
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let size = u32_at(bytes, at + 4) as usize;
        match &bytes[at..at + 4] {
            b"fmt " => fmt = Some((u16_at(bytes, at + 8), u16_at(bytes, at + 10), u32_at(bytes, at + 12), u16_at(bytes, at + 22))),
            b"data" => data = Some(size),
            _ => {}
        }
        at += 8 + size + (size & 1);
    }
    let (tag, channels, rate, bits) = fmt.expect("fmt chunk");
    (tag, channels, rate, bits, data.expect("data chunk"))
}

#[test]
fn v0_1_0_archives_match_their_manifest_and_read_without_a_zip_crate() {
    let manifest: Value = serde_json::from_slice(&std::fs::read(dir().join("manifest.json")).unwrap()).unwrap();
    let mut stems = Vec::new();
    for file in manifest["files"].as_array().unwrap() {
        let name = file["file"].as_str().unwrap();
        let zip = std::fs::read(dir().join(name)).unwrap();
        assert_eq!(zip.len() as u64, file["bytes"].as_u64().unwrap(), "{name}: size");
        assert_eq!(hex(&zip), file["sha256"].as_str().unwrap(), "{name}: sha256");

        let listed = entries(&zip);
        let want = file["entries"].as_array().unwrap();
        assert_eq!(listed.len(), want.len(), "{name}: entry count");
        for ((entry, data), w) in listed.iter().zip(want) {
            assert_eq!(entry, w["name"].as_str().unwrap());
            assert_eq!(hex(data), w["sha256"].as_str().unwrap(), "{name}/{entry}: sha256");
        }

        let (_, json) = listed.iter().find(|(n, _)| n.ends_with("-session.json")).expect("session.json");
        let session: Value = serde_json::from_slice(json).unwrap();
        let rate = session["sampleRate"].as_u64().unwrap() as u32;
        let frames = session["masterLengthFrames"].as_i64().unwrap();
        let bars = session["bars"].as_i64().unwrap();
        assert_eq!(session["app"], "BleepLoop");
        assert_eq!(session["formatVersion"], 1);
        assert_eq!(frames, bars * frames_per_bar(session["bpm"].as_f64().unwrap(), rate), "{name}: grid");

        let tracks = session["tracks"].as_array().unwrap();
        for track in tracks {
            let stem = track["file"].as_str().unwrap();
            let (_, data) = listed.iter().find(|(n, _)| n == stem).expect("the stem a track names");
            assert_eq!(wav(data), (3, 1, rate, 32, frames as usize * 4), "{name}/{stem}: float32 mono");
            assert_eq!(track["frames"].as_i64().unwrap(), frames);
            assert_eq!(track["fx"].as_array().unwrap().len(), 5);
        }
        match &session["master"] {
            Value::Null => assert_eq!(listed.len(), tracks.len() + 1, "{name}: stems + session.json"),
            master => {
                assert_eq!(master["kind"], "wet-v1");
                let (_, data) = listed.iter().find(|(n, _)| n == master["file"].as_str().unwrap()).expect("master");
                assert_eq!(wav(data), (1, 2, rate, 16, frames as usize * 4), "{name}: PCM16 stereo master");
                assert_eq!(listed.len(), tracks.len() + 2, "{name}: stems + master + session.json");
            }
        }
        stems.push(listed.iter().filter(|(n, _)| n.contains("-track")).map(|(_, d)| hex(d)).collect::<Vec<_>>());
    }
    // The export and the recovery archive hold the same stems, byte for byte.
    assert_eq!(stems.len(), 2);
    assert_eq!(stems[0], stems[1]);
}
