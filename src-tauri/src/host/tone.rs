//! OWNS: tone recall's pure half (engine mode): the tone file, the store, the VST3 and VST2 state
//! containers and the save debounce. A TONE is a plugin's saved state: a CLAP plugin's `state` blob, a
//! VST3 plugin's component and edit-controller states in one container (`encode_vst3`), or a VST2
//! plugin's bank chunk or parameter values (`encode_vst2`). The tone belongs to the slot
//! AND the plugin: the store keeps one file per slot and plugin identity (format, path, id) in the
//! app-local data folder's `tones/`, named by the slot and a stable FNV-1a hash of that identity and
//! replaced atomically. So the same plugin in both slots keeps two tones, and a slot that swaps its
//! plugin and back gets its tone back. The same file is what a session export carries per slot.
//!
//! Nothing here touches a plugin. The engine-mode owners (`clap_engine`, `vst3_engine`, `vst2_engine`) restore a tone
//! only inside their load sequence, before the plugin activates, and save one only on their own thread,
//! through a [`ToneKeeper`]: debounced after the last change, when the editor closes, before an unload,
//! and when a session export or the app's exit asks. Every save hands the tone back; whether it also
//! lands in the store is the keeper's call:
//!
//! - a load whose restore failed (the file unreadable, or the plugin refused it, perhaps after an
//!   update) leaves the stored tone alone until the player changes something: a reinstalled version of
//!   the plugin may take it again, and the defaults it fell back to are nothing to keep;
//! - a session import ([`ToneStore::import`]) is written and counted under its plugin's write lock,
//!   which every write of that plugin's file takes, so no save interleaves with it. It is checked first
//!   against the plugin session.json names, and a failed write changes nothing. From then on every load
//!   of that plugin in that slot from before the import saves nothing more to the store; the slot the
//!   import reloads restores the imported bytes as they were handed over ([`ToneBinding::imported`], parked
//!   under a reload token in a [`ToneHandoff`]), not the file another load may have written since.
//!
//! An owner's turn reads the import count without waiting on any lock held across disk I/O
//! (`ToneKeeper::poll`), and another tone's write never holds up its save. A save of the same tone
//! takes that tone's write lock on the owner thread (`engine_slot::keep_tone`), so an import of it
//! stalled in the file system stalls that owner until the write returns.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{
    AtomicBool, AtomicU64,
    Ordering::{Acquire, Relaxed, Release},
};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use super::engine_slot::PluginFormat;

/// How long a plugin must go without a change before its tone is saved. A kill loses at most this.
pub(crate) const SAVE_QUIET: Duration = Duration::from_secs(2);

/// The largest state a tone holds. A session import sizes its archive limit from it
/// (`src/session/archive-limits.ts`, `MAX_TONE_BYTES`); change them together.
pub(crate) const MAX_STATE_BYTES: usize = 16 << 20;

/// The largest format, path, id or name a tone file carries.
const MAX_FIELD_BYTES: usize = 4096;

/// A tone file: `MAGIC`, the version (u16), four length-prefixed UTF-8 fields (format, path, id,
/// name; u32 lengths), the length-prefixed state, then an FNV-1a 64 of every byte before it. Little
/// endian throughout.
const MAGIC: [u8; 4] = *b"BLTN";
const VERSION: u16 = 1;

/// The VST3 container inside a tone's state: `VST3_MAGIC`, its version (u16), then the component's
/// and the edit controller's states, each length-prefixed (u32).
const VST3_MAGIC: [u8; 4] = *b"LFV3";
const VST3_VERSION: u16 = 1;

/// The VST2 container inside a tone's state: `VST2_MAGIC`, its version (u16), a kind byte, the
/// current program's index (i32), then the payload: the bank chunk, length-prefixed (u32), or the
/// parameter count (u32) and that many values (f32).
const VST2_MAGIC: [u8; 4] = *b"LFV2";
const VST2_VERSION: u16 = 1;
const VST2_KIND_CHUNK: u8 = 1;
const VST2_KIND_PARAMS: u8 = 2;

/// The largest chunk a VST2 tone holds: what the container leaves of `MAX_STATE_BYTES`.
pub(crate) const VST2_MAX_CHUNK: usize = MAX_STATE_BYTES - 15;

/// FNV-1a 64: stable across Rust versions and runs, unlike `DefaultHasher`.
fn fnv1a64(parts: &[&[u8]]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for &byte in *part {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

/// Which plugin a tone belongs to: the scan descriptor's format (`clap` | `vst3` | `vst2`), bundle path and
/// plugin id.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ToneIdentity {
    pub(crate) format: String,
    pub(crate) path: String,
    pub(crate) id: String,
}

impl ToneIdentity {
    /// The store's file for this plugin in `slot`. Each field is followed by a 0 byte, so no two
    /// identities hash the same concatenation.
    pub(crate) fn file_name(&self, slot: usize) -> String {
        let key = fnv1a64(&[self.format.as_bytes(), &[0], self.path.as_bytes(), &[0], self.id.as_bytes(), &[0]]);
        format!("slot{slot}-{key:016x}.tone")
    }
}

impl ToneIdentity {
    /// A chain unit's own file: `unit-<id>.tone`, one per unit, whatever its plugin
    /// ([`ToneIdentity::file_name`] stays the slot's).
    pub(crate) fn unit_file_name(unit: lf_engine::UnitId) -> String {
        format!("unit-{unit}.tone")
    }
}

/// Whose tone a load reads and a save writes: a legacy slot's (its file per slot and plugin), or a chain
/// unit's (its own file, the slot's file as the fallback a new unit starts from).
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ToneKey {
    Slot { slot: usize, identity: ToneIdentity },
    Unit { unit: lf_engine::UnitId, slot: usize, identity: ToneIdentity },
}

impl ToneKey {
    fn file_name(&self) -> String {
        match self {
            ToneKey::Slot { slot, identity } => identity.file_name(*slot),
            ToneKey::Unit { unit, .. } => ToneIdentity::unit_file_name(*unit),
        }
    }

    pub(crate) fn identity(&self) -> &ToneIdentity {
        match self {
            ToneKey::Slot { identity, .. } | ToneKey::Unit { identity, .. } => identity,
        }
    }
}

/// One tone: whose it is, the plugin's name (a session import names it in its toast) and the state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Tone {
    pub(crate) identity: ToneIdentity,
    pub(crate) name: String,
    pub(crate) state: Vec<u8>,
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// A tone file's bytes. Fields past their limits are the caller's bug: `ToneKeeper` never builds one.
pub(crate) fn encode(tone: &Tone) -> Vec<u8> {
    let fields = [&tone.identity.format, &tone.identity.path, &tone.identity.id, &tone.name];
    let mut out = Vec::with_capacity(32 + fields.iter().map(|f| f.len()).sum::<usize>() + tone.state.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    for field in fields {
        put_bytes(&mut out, field.as_bytes());
    }
    put_bytes(&mut out, &tone.state);
    let sum = fnv1a64(&[&out]);
    out.extend_from_slice(&sum.to_le_bytes());
    out
}

/// A bounds-checked reader over a tone file or a container.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize, what: &str) -> Result<&'a [u8], String> {
        let end = self.at.checked_add(n).filter(|&end| end <= self.bytes.len());
        let end = end.ok_or_else(|| format!("truncated: {what} needs {n} bytes at {}", self.at))?;
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }

    fn u16(&mut self, what: &str) -> Result<u16, String> {
        let b = self.take(2, what)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self, what: &str) -> Result<u32, String> {
        let b = self.take(4, what)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn sized(&mut self, max: usize, what: &str) -> Result<&'a [u8], String> {
        let len = self.u32(what)? as usize;
        if len > max {
            return Err(format!("{what} is {len} bytes, more than {max}"));
        }
        self.take(len, what)
    }

    fn text(&mut self, what: &str) -> Result<String, String> {
        let b = self.sized(MAX_FIELD_BYTES, what)?;
        String::from_utf8(b.to_vec()).map_err(|_| format!("{what} is not UTF-8"))
    }
}

/// Read a tone file. Refuses a wrong magic, a newer version, a truncated or overlong field, trailing
/// bytes and a checksum that does not match, without panicking on any input.
pub(crate) fn decode(bytes: &[u8]) -> Result<Tone, String> {
    let mut r = Reader { bytes, at: 0 };
    if r.take(4, "magic")? != MAGIC {
        return Err("not a BleepLoop tone".to_string());
    }
    let version = r.u16("version")?;
    if version != VERSION {
        return Err(format!("tone version {version}; this build reads {VERSION}"));
    }
    let format = r.text("format")?;
    let path = r.text("path")?;
    let id = r.text("id")?;
    let name = r.text("name")?;
    let state = r.sized(MAX_STATE_BYTES, "state")?.to_vec();
    let body = r.at;
    let sum = r.take(8, "checksum")?;
    if r.at != bytes.len() {
        return Err(format!("{} bytes after the checksum", bytes.len() - r.at));
    }
    let want = fnv1a64(&[&bytes[..body]]);
    if sum != want.to_le_bytes() {
        return Err("checksum mismatch: the file is corrupt".to_string());
    }
    // The stored NAME says what the tone is for; its path is never asked.
    if PluginFormat::from_name(&format).is_none() {
        return Err(format!("unknown plugin format {format:?}"));
    }
    Ok(Tone { identity: ToneIdentity { format, path, id }, name, state })
}

/// A VST3 tone's state: the component's (`IComponent::getState`) and the edit controller's
/// (`IEditController::getState`, empty when there is none or it keeps nothing).
pub(crate) fn encode_vst3(component: &[u8], controller: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(14 + component.len() + controller.len());
    out.extend_from_slice(&VST3_MAGIC);
    out.extend_from_slice(&VST3_VERSION.to_le_bytes());
    put_bytes(&mut out, component);
    put_bytes(&mut out, controller);
    out
}

/// The component's and the controller's states from a VST3 tone's state.
pub(crate) fn decode_vst3(bytes: &[u8]) -> Result<(&[u8], &[u8]), String> {
    let mut r = Reader { bytes, at: 0 };
    if r.take(4, "VST3 magic")? != VST3_MAGIC {
        return Err("not a VST3 tone".to_string());
    }
    let version = r.u16("VST3 version")?;
    if version != VST3_VERSION {
        return Err(format!("VST3 tone version {version}; this build reads {VST3_VERSION}"));
    }
    let component = r.sized(MAX_STATE_BYTES, "component state")?;
    let controller = r.sized(MAX_STATE_BYTES, "controller state")?;
    if r.at != bytes.len() {
        return Err(format!("{} bytes after the controller state", bytes.len() - r.at));
    }
    Ok((component, controller))
}

/// A VST2 tone's state. A plugin that keeps its state as an opaque chunk (`effFlagsProgramChunks`)
/// is saved as its bank chunk, which restores its own program; any other as every parameter's value.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Vst2State {
    Chunk { program: i32, chunk: Vec<u8> },
    Params { program: i32, values: Vec<f32> },
}

/// A VST2 tone's state as bytes. A chunk past `VST2_MAX_CHUNK` is the caller's bug: an owner checks
/// the plugin's length before it copies anything.
pub(crate) fn encode_vst2(state: &Vst2State) -> Vec<u8> {
    let (kind, program, payload) = match state {
        Vst2State::Chunk { program, chunk } => (VST2_KIND_CHUNK, *program, chunk.len()),
        Vst2State::Params { program, values } => (VST2_KIND_PARAMS, *program, values.len() * 4),
    };
    let mut out = Vec::with_capacity(15 + payload);
    out.extend_from_slice(&VST2_MAGIC);
    out.extend_from_slice(&VST2_VERSION.to_le_bytes());
    out.push(kind);
    out.extend_from_slice(&program.to_le_bytes());
    match state {
        Vst2State::Chunk { chunk, .. } => put_bytes(&mut out, chunk),
        Vst2State::Params { values, .. } => {
            out.extend_from_slice(&(values.len() as u32).to_le_bytes());
            for value in values {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
    }
    out
}

/// Read a VST2 tone's state. Refuses a wrong magic, another version, an unknown kind, a truncated
/// payload and trailing bytes; nothing is sized from a length the bytes do not hold.
pub(crate) fn decode_vst2(bytes: &[u8]) -> Result<Vst2State, String> {
    let mut r = Reader { bytes, at: 0 };
    if r.take(4, "VST2 magic")? != VST2_MAGIC {
        return Err("not a VST2 tone".to_string());
    }
    let version = r.u16("VST2 version")?;
    if version != VST2_VERSION {
        return Err(format!("VST2 tone version {version}; this build reads {VST2_VERSION}"));
    }
    let kind = r.take(1, "VST2 kind")?[0];
    let program = r.u32("program index")? as i32;
    let state = match kind {
        VST2_KIND_CHUNK => Vst2State::Chunk { program, chunk: r.sized(VST2_MAX_CHUNK, "chunk")?.to_vec() },
        VST2_KIND_PARAMS => {
            let count = r.u32("parameter count")? as usize;
            let len = count.checked_mul(4).ok_or_else(|| format!("{count} parameters"))?;
            let values = r.take(len, "parameter values")?;
            Vst2State::Params {
                program,
                values: values.chunks_exact(4).map(|v| f32::from_le_bytes([v[0], v[1], v[2], v[3]])).collect(),
            }
        }
        other => return Err(format!("unknown VST2 tone kind {other}")),
    };
    if r.at != bytes.len() {
        return Err(format!("{} bytes after the VST2 state", bytes.len() - r.at));
    }
    Ok(state)
}

/// The tone store: `tones/` in the app-local data folder (a probe's run has a profile of its own), and
/// per slot and plugin how many session imports have replaced its tone this run. Clones share both.
#[derive(Clone, Debug)]
pub(crate) struct ToneStore {
    dir: PathBuf,
    /// Per slot and plugin (its file name). The map's lock is held only to find an entry, never across I/O.
    entries: Arc<Mutex<HashMap<String, Arc<Entry>>>>,
    /// Test-only: the next write stops mid-way until the test lets it go (`stall_next_write`).
    #[cfg(test)]
    stall: Arc<Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>>,
}

/// One slot's tone of one plugin: its place in the store.
#[derive(Debug, Default)]
struct Entry {
    /// The session imports of this run. It advances only under `write` and only after the import's
    /// file landed, and an owner's turn reads it without a lock.
    revision: AtomicU64,
    /// Held across every write of this file (and a load's read of it with its revision), so an import
    /// and a load's save never interleave. Another file's writes never wait on it.
    write: Mutex<()>,
}

impl Entry {
    /// Nothing under the lock panics mid-write, so a poisoned one is taken as it is.
    fn write(&self) -> MutexGuard<'_, ()> {
        self.write.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A session import, stored: the tone, and the store's import count for its slot and plugin after it.
/// The reload that restores these bytes is a load from after the import.
#[derive(Clone, Debug)]
pub(crate) struct Imported {
    pub(crate) tone: Tone,
    pub(crate) revision: u64,
}

impl ToneStore {
    pub(crate) fn new(dir: PathBuf) -> ToneStore {
        ToneStore {
            dir,
            entries: Arc::default(),
            #[cfg(test)]
            stall: Arc::default(),
        }
    }

    fn path_for(&self, file_name: &str) -> PathBuf {
        self.dir.join(file_name)
    }

    fn entry_for(&self, file_name: &str) -> Arc<Entry> {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = entries.get(file_name) {
            return entry.clone();
        }
        let entry = Arc::new(Entry::default());
        entries.insert(file_name.to_string(), entry.clone());
        entry
    }

    fn read_file(&self, file_name: &str, identity: &ToneIdentity) -> Result<Option<Tone>, String> {
        let path = self.path_for(file_name);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let tone = decode(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        if &tone.identity != identity {
            return Err(format!("{} holds the tone of {} ({})", path.display(), tone.name, tone.identity.path));
        }
        Ok(Some(tone))
    }

    /// Under the file's write lock only (`Entry::write`).
    fn write_file(&self, file_name: &str, bytes: &[u8]) -> Result<(), String> {
        #[cfg(test)]
        {
            let stall = self.stall.lock().unwrap_or_else(PoisonError::into_inner).take();
            if let Some((entered, go)) = stall {
                let _ = entered.send(());
                let _ = go.recv();
            }
        }
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("{}: {e}", self.dir.display()))?;
        write_atomic(&self.path_for(file_name), bytes)
    }

    fn entry(&self, slot: usize, identity: &ToneIdentity) -> Arc<Entry> {
        self.entry_for(&identity.file_name(slot))
    }

    /// How many session imports have replaced `identity`'s tone in `slot` this run. Waits on no write.
    #[allow(dead_code)]
    pub(crate) fn revision(&self, slot: usize, identity: &ToneIdentity) -> u64 {
        self.entry(slot, identity).revision.load(Acquire)
    }

    fn revision_key(&self, key: &ToneKey) -> u64 {
        self.entry_for(&key.file_name()).revision.load(Acquire)
    }

    /// A load's read: the stored tone (as `load`) and the import count it belongs to, taken together so
    /// an import lands wholly before or wholly after it.
    pub(crate) fn load_for_restore(&self, slot: usize, identity: &ToneIdentity) -> (Result<Option<Tone>, String>, u64) {
        let entry = self.entry(slot, identity);
        let _write = entry.write();
        (self.load(slot, identity), entry.revision.load(Acquire))
    }

    /// The tone stored for `identity` in `slot`: `Ok(None)` when there is none, `Err` when the file
    /// cannot be read, is corrupt or belongs to another plugin.
    pub(crate) fn load(&self, slot: usize, identity: &ToneIdentity) -> Result<Option<Tone>, String> {
        self.read_file(&identity.file_name(slot), identity)
    }

    /// A session import: store a tone file's bytes (checked first) for `expected`, the plugin session.json
    /// names for `slot`, then count the import, so no load of that plugin in that slot from before it
    /// saves over it. A tone file of another plugin is refused before anything is written, and a failed
    /// write counts nothing.
    pub(crate) fn import(&self, slot: usize, bytes: &[u8], expected: &ToneIdentity) -> Result<Imported, String> {
        let tone = Self::check(bytes, expected)?;
        let file_name = tone.identity.file_name(slot);
        let entry = self.entry_for(&file_name);
        let _write = entry.write();
        self.write_file(&file_name, bytes)?;
        let revision = entry.revision.fetch_add(1, Release) + 1;
        Ok(Imported { revision, tone })
    }

    /// A load's save: store a tone file's bytes for `key` unless a session import/redeem replaced
    /// that tone after the load read it (`revision`, from `load_for_restore_key`). `Ok(false)`: not stored.
    fn save_from_load(&self, key: &ToneKey, revision: u64, bytes: &[u8]) -> Result<bool, String> {
        let file_name = key.file_name();
        let entry = self.entry_for(&file_name);
        let _write = entry.write();
        if entry.revision.load(Acquire) != revision {
            return Ok(false);
        }
        self.write_file(&file_name, bytes)?;
        Ok(true)
    }

    /// Test-only: store a tone file's bytes for `slot` under the identity they carry, as a save from
    /// nowhere would.
    #[cfg(test)]
    pub(crate) fn save_encoded(&self, slot: usize, bytes: &[u8]) -> Result<Tone, String> {
        let tone = decode(bytes)?;
        let file_name = tone.identity.file_name(slot);
        let entry = self.entry_for(&file_name);
        let _write = entry.write();
        self.write_file(&file_name, bytes)?;
        Ok(tone)
    }

    /// Test-only: the next write, holding its file's write lock, signals `entered` and waits for `go`
    /// (a file system that stalls).
    #[cfg(test)]
    pub(crate) fn stall_next_write(&self) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (go, go_rx) = std::sync::mpsc::channel();
        *self.stall.lock().unwrap_or_else(PoisonError::into_inner) = Some((entered_tx, go_rx));
        (entered, go)
    }
}

#[allow(dead_code)]
impl ToneStore {
    /// A load's read by key ([`ToneStore::load_for_restore`] by slot stays): for a unit, its own file,
    /// else the slot's file, read only; and the import count it belongs to.
    pub(crate) fn load_for_restore_key(&self, key: &ToneKey) -> (Result<Option<Tone>, String>, u64) {
        match key {
            ToneKey::Slot { slot, identity } => self.load_for_restore(*slot, identity),
            ToneKey::Unit { unit, slot, identity } => {
                let unit_file = ToneIdentity::unit_file_name(*unit);
                let unit_entry = self.entry_for(&unit_file);
                let (own, revision) = {
                    let _write = unit_entry.write();
                    (self.read_file(&unit_file, identity), unit_entry.revision.load(Acquire))
                };
                match own {
                    Ok(None) => {
                        let slot_entry = self.entry(*slot, identity);
                        let _slot_write = slot_entry.write();
                        (self.load(*slot, identity), revision)
                    }
                    other => (other, revision),
                }
            }
        }
    }

    /// A tone file's bytes checked against `expected`, the plugin session.json names: nothing written.
    pub(crate) fn check(bytes: &[u8], expected: &ToneIdentity) -> Result<Tone, String> {
        let tone = decode(bytes)?;
        if &tone.identity != expected {
            return Err(format!(
                "the session's tone file belongs to {} ({}), not to the plugin session.json names ({})",
                tone.name, tone.identity.path, expected.path
            ));
        }
        Ok(tone)
    }

    /// A parked tone redeemed by the unit `key` names: written to the unit's file under its write lock
    /// and counted, so the outgoing load's save is dropped from here on.
    pub(crate) fn redeem(&self, key: &ToneKey, tone: Tone) -> Result<Imported, String> {
        let expected = key.identity();
        if &tone.identity != expected {
            return Err(format!(
                "the session's tone file belongs to {} ({}), not to the plugin session.json names ({})",
                tone.name, tone.identity.path, expected.path
            ));
        }
        let bytes = encode(&tone);
        let file_name = key.file_name();
        let entry = self.entry_for(&file_name);
        let _write = entry.write();
        self.write_file(&file_name, &bytes)?;
        let revision = entry.revision.fetch_add(1, Release) + 1;
        Ok(Imported { revision, tone })
    }

    /// The unit `key` names left for good (its owner got `Back::Removed`): its file becomes the slot's,
    /// counted, so the next unit of that plugin in the slot starts from it. Only while the unit file's
    /// import count is still `revision`, the one its owner loaded at, checked under the file's lock: a
    /// redeem that landed since belongs to a newer load and is left as it is (`Ok(())`, nothing touched).
    pub(crate) fn retire(&self, key: &ToneKey, revision: u64) -> Result<(), String> {
        let ToneKey::Unit { unit, slot, identity } = key else { return Ok(()) };
        let unit_file = ToneIdentity::unit_file_name(*unit);
        let unit_entry = self.entry_for(&unit_file);
        let _unit_write = unit_entry.write();
        if unit_entry.revision.load(Acquire) != revision {
            return Ok(());
        }
        let unit_path = self.path_for(&unit_file);
        let bytes = match std::fs::read(&unit_path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(format!("{}: {e}", unit_path.display())),
        };
        let tone = decode(&bytes).map_err(|e| format!("{}: {e}", unit_path.display()))?;
        if &tone.identity != identity {
            return Err(format!("{} holds the tone of {} ({})", unit_path.display(), tone.name, tone.identity.path));
        }
        let slot_file = identity.file_name(*slot);
        let slot_entry = self.entry_for(&slot_file);
        let _slot_write = slot_entry.write();
        self.write_file(&slot_file, &bytes)?;
        slot_entry.revision.fetch_add(1, Release);
        let _ = std::fs::remove_file(&unit_path);
        Ok(())
    }
}

/// Write `bytes` to a temporary file beside `path`, flush it to disk, then rename it over `path`: a
/// reader sees the old file or the new one, never half of either. The temporary name is unique per
/// write, and a failed write removes it. The plugin folder list (`folders.rs`) is written this way too.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_extension(format!("tmp-{}-{}", std::process::id(), NEXT.fetch_add(1, Relaxed)));
    let written = std::fs::File::create(&tmp).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()
    });
    let renamed = written.and_then(|()| std::fs::rename(&tmp, path));
    renamed.map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

/// When the next debounced save is due: every change restarts the quiet period.
#[derive(Default, Debug)]
pub(crate) struct Debounce {
    since: Option<Instant>,
}

impl Debounce {
    pub(crate) fn touch(&mut self, now: Instant) {
        self.since = Some(now);
    }

    /// A change is waiting to be saved.
    pub(crate) fn dirty(&self) -> bool {
        self.since.is_some()
    }

    /// The last change is `SAVE_QUIET` old.
    pub(crate) fn due(&self, now: Instant) -> bool {
        self.since.is_some_and(|since| now.saturating_duration_since(since) >= SAVE_QUIET)
    }

    pub(crate) fn clear(&mut self) {
        self.since = None;
    }
}

/// Where a loaded plugin's tone lives: the store and its key (a legacy slot or a chain unit).
#[derive(Clone, Debug)]
pub(crate) struct ToneBinding {
    pub(crate) store: ToneStore,
    pub(crate) key: ToneKey,
    /// The reload a session import asked for: the imported tone, which this load restores as it was
    /// handed over instead of reading the store.
    pub(crate) imported: Option<Imported>,
}

/// The imported tones parked for the reloads a session import asked for, one per slot, each under a
/// reload token the import answers. Only the load that passes that token back takes it; any other load
/// of the slot (a source change) drops it, and so does a reload the frontend skipped (`forget`).
#[derive(Debug)]
pub(crate) struct ToneHandoff<const SLOTS: usize> {
    last: u32,
    parked: [Option<(u32, Imported)>; SLOTS],
}

impl<const SLOTS: usize> Default for ToneHandoff<SLOTS> {
    fn default() -> Self {
        ToneHandoff { last: 0, parked: std::array::from_fn(|_| None) }
    }
}

impl<const SLOTS: usize> ToneHandoff<SLOTS> {
    /// Park `imported` for `slot`'s reload, replacing an earlier one there; the token its load passes.
    pub(crate) fn park(&mut self, slot: usize, imported: Imported) -> u32 {
        self.last = self.last.checked_add(1).unwrap_or(1);
        self.parked[slot] = Some((self.last, imported));
        self.last
    }

    /// A load of `identity` into `slot`, passing `token` (`None`: not a reload): the parked tone when
    /// the token and the plugin are its own. Whatever was parked for the slot is gone after any load.
    pub(crate) fn take(&mut self, slot: usize, token: Option<u32>, identity: &ToneIdentity) -> Option<Imported> {
        let (parked, imported) = self.parked[slot].take()?;
        (token == Some(parked) && &imported.tone.identity == identity).then_some(imported)
    }

    /// The reload that `token` was for did not happen: drop its tone, if it is still parked.
    pub(crate) fn forget(&mut self, slot: usize, token: u32) {
        if self.parked[slot].as_ref().is_some_and(|(parked, _)| *parked == token) {
            self.parked[slot] = None;
        }
    }
}

/// The chain's tone tokens: a session import's tone parked for the unit reload that redeems it. A token
/// binds to the plugin identity alone; at most [`MAX_PARKED_TONES`] are parked. (Legacy keeps
/// [`ToneHandoff`].)
#[allow(dead_code)]
#[derive(Debug, Default)]
pub(crate) struct ToneTokens {
    last: u32,
    parked: Vec<(u32, Tone)>,
}

/// [`ToneTokens`]' bound: one per unit the chains can seat.
#[allow(dead_code)]
pub(crate) const MAX_PARKED_TONES: usize = lf_engine::SLOT_COUNT * lf_engine::MAX_UNITS;

#[allow(dead_code)]
impl ToneTokens {
    /// Park `tone`; the token a reload passes. At [`MAX_PARKED_TONES`], drops the oldest parked tone
    /// (FIFO) so abandoned tokens never wedge future imports.
    pub(crate) fn park(&mut self, tone: Tone) -> u32 {
        if self.parked.len() >= MAX_PARKED_TONES {
            self.parked.remove(0);
        }
        self.last = self.last.checked_add(1).unwrap_or(1);
        self.parked.push((self.last, tone));
        self.last
    }

    /// The tone parked under `token`, when it belongs to `identity`.
    pub(crate) fn take(&mut self, token: u32, identity: &ToneIdentity) -> Option<Tone> {
        let pos = self.parked.iter().position(|(t, _)| *t == token)?;
        if &self.parked[pos].1.identity != identity {
            return None;
        }
        Some(self.parked.remove(pos).1)
    }

    /// The reload `token` was for did not happen: drop its tone, if it is still parked.
    pub(crate) fn forget(&mut self, token: u32) {
        if let Some(pos) = self.parked.iter().position(|(t, _)| *t == token) {
            self.parked.remove(pos);
        }
    }
}

/// What a save did: the tone file's bytes (empty for a plugin that keeps no state), and why the store
/// did not get them (`None`: it did, or there is no store).
pub(crate) struct Saved {
    pub(crate) bytes: Vec<u8>,
    pub(crate) not_stored: Option<&'static str>,
}

/// An engine-mode owner's tone bookkeeping. `edited` is raised wherever a change is seen off the owner
/// thread (a host parameter set, a CLAP unit's output events on the audio thread, a VST3 `performEdit`;
/// an atomic store only); the owner polls it once per turn. Without a binding (a test, no data folder)
/// the keeper keeps nothing and a save only hands the tone back.
pub(crate) struct ToneKeeper {
    binding: Option<ToneBinding>,
    edited: Arc<AtomicBool>,
    debounce: Debounce,
    /// The store's import count for this slot's tone when this load read it: once an import has
    /// counted past it, this load saves nothing more to the store.
    revision: u64,
    /// This load's restore failed and nothing has changed since: the stored tone stays as it is.
    keep_stored: bool,
}

impl ToneKeeper {
    pub(crate) fn new(binding: Option<ToneBinding>, edited: Arc<AtomicBool>) -> ToneKeeper {
        ToneKeeper { binding, edited, debounce: Debounce::default(), revision: 0, keep_stored: false }
    }

    /// The flag a change raises (the unit and the component handler hold it too).
    pub(crate) fn edited(&self) -> Arc<AtomicBool> {
        self.edited.clone()
    }

    /// The state to restore at load (`Ok(None)`: nothing is stored): the imported tone a reload was
    /// handed, else the store's. Notes which import it belongs to.
    pub(crate) fn stored(&mut self) -> Result<Option<Vec<u8>>, String> {
        let Some(b) = &mut self.binding else { return Ok(None) };
        if let Some(imported) = b.imported.take() {
            self.revision = imported.revision;
            return Ok(Some(imported.tone.state));
        }
        let (stored, revision) = b.store.load_for_restore_key(&b.key);
        self.revision = revision;
        Ok(stored?.map(|tone| tone.state))
    }

    /// The restore failed: no save replaces the stored tone until something changes (`keep_stored`).
    pub(crate) fn keep_stored(&mut self) {
        self.keep_stored = true;
    }

    /// After a restore: what the plugin reported while it took the state describes the state just
    /// loaded, not a change.
    pub(crate) fn settle(&mut self) {
        self.edited.store(false, Relaxed);
        self.debounce.clear();
    }

    fn changed(&mut self, now: Instant) {
        self.debounce.touch(now);
        self.keep_stored = false;
    }

    /// An owner-thread change (a CLAP `mark_dirty` or params rescan, a VST3 re-list, what a VST2
    /// plugin latched for its owner).
    pub(crate) fn note_change(&mut self, now: Instant) {
        self.changed(now);
    }

    /// Owner turn: fold in what `edited` saw; true when a debounced save is due now and may still land.
    pub(crate) fn poll(&mut self, now: Instant) -> bool {
        if self.edited.swap(false, Acquire) {
            self.changed(now);
        }
        if !self.debounce.due(now) {
            return false;
        }
        if self.superseded() {
            self.debounce.clear();
            return false;
        }
        true
    }

    /// Whether a change is waiting to be saved (an unload saves it first).
    pub(crate) fn dirty(&mut self) -> bool {
        if self.edited.swap(false, Acquire) {
            self.changed(Instant::now());
        }
        self.debounce.dirty() && !self.superseded()
    }

    /// A session import replaced this slot's tone after this load read it. A lock-free read: an
    /// import still writing is not counted yet, and the save that follows checks again under the lock.
    fn superseded(&self) -> bool {
        self.binding.as_ref().is_some_and(|b| b.store.revision_key(&b.key) != self.revision)
    }

    /// Save now: `take` asks the plugin named `name` for its state (`Ok(None)`: it keeps none). The tone
    /// file's bytes come back either way; the store gets them unless this load keeps the stored tone or
    /// an import superseded it. The pending change is settled either way, so a failing save waits for
    /// the next change rather than retrying every turn.
    pub(crate) fn save(
        &mut self,
        name: &str,
        take: impl FnOnce() -> Result<Option<Vec<u8>>, String>,
    ) -> Result<Saved, String> {
        self.debounce.clear();
        let Some(state) = take()? else { return Ok(Saved { bytes: Vec::new(), not_stored: None }) };
        if state.len() > MAX_STATE_BYTES {
            return Err(format!("the plugin's state is {} bytes, more than {MAX_STATE_BYTES}", state.len()));
        }
        let identity = match &self.binding {
            Some(b) => b.key.identity().clone(),
            None => ToneIdentity { format: String::new(), path: String::new(), id: String::new() },
        };
        let name: String = name.chars().take(256).collect();
        let bytes = encode(&Tone { identity, name, state });
        let not_stored = match &self.binding {
            None => None,
            Some(_) if self.keep_stored => Some("the stored tone this load could not restore is kept until something changes"),
            Some(b) => (!b.store.save_from_load(&b.key, self.revision, &bytes)?)
                .then_some("a session import replaced this slot's tone after this load"),
        };
        Ok(Saved { bytes, not_stored })
    }
}

impl ToneKeeper {
    /// The unit left for good: its file becomes the slot's ([`ToneStore::retire`]), unless a redeem
    /// replaced it after this load read it (checked again under the file's lock there).
    pub(crate) fn retire(&mut self) {
        if self.superseded() {
            return;
        }
        let Some(b) = &self.binding else { return };
        if let Err(e) = b.store.retire(&b.key, self.revision) {
            log::warn!("tone retire failed: {e}");
        }
    }
}

/// Test-only: a fresh folder under the system temp dir with one store over it, removed on drop (the
/// store tests and the engine-mode fixtures keep their tones there).
#[cfg(test)]
pub(crate) struct TempDir(pub(crate) PathBuf, ToneStore);

#[cfg(test)]
impl TempDir {
    pub(crate) fn new(tag: &str) -> TempDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!("lf-tone-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Relaxed)));
        let _ = std::fs::remove_dir_all(&dir);
        TempDir(dir.clone(), ToneStore::new(dir))
    }

    /// The folder's store (every binding shares it, as the app's loads share one).
    pub(crate) fn store(&self) -> &ToneStore {
        &self.1
    }

    /// A binding to the folder's store, for `identity` in `slot`.
    pub(crate) fn binding(&self, slot: usize, identity: ToneIdentity) -> ToneBinding {
        ToneBinding { store: self.1.clone(), key: ToneKey::Slot { slot, identity }, imported: None }
    }

    /// A binding to the folder's store, for chain `unit` of `identity` in `slot`.
    pub(crate) fn unit_binding(&self, unit: lf_engine::UnitId, slot: usize, identity: ToneIdentity) -> ToneBinding {
        ToneBinding { store: self.1.clone(), key: ToneKey::Unit { unit, slot, identity }, imported: None }
    }
}

#[cfg(test)]
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> ToneIdentity {
        ToneIdentity {
            format: "vst3".to_string(),
            path: r"C:\Program Files\Common Files\VST3\FabFilter Pro-Q 3.vst3".to_string(),
            id: "72c4db717a4d459aa97e3e9e3e5c5a8b".to_string(),
        }
    }

    fn tone(state: &[u8]) -> Tone {
        Tone { identity: identity(), name: "Pro-Q 3".to_string(), state: state.to_vec() }
    }

    #[test]
    fn an_identity_hashes_to_the_same_file_name_on_every_run() {
        // Fixed inputs, fixed names: a change here orphans every tone players have saved.
        assert_eq!(fnv1a64(&[b""]), 0xcbf2_9ce4_8422_2325, "FNV-1a 64 offset basis");
        assert_eq!(fnv1a64(&[b"a"]), 0xaf63_dc4c_8601_ec8c, "the published FNV-1a 64 of \"a\"");
        assert_eq!(identity().file_name(0), "slot0-e20f937c13804113.tone");
        assert_eq!(identity().file_name(1), "slot1-e20f937c13804113.tone");
        let clap = ToneIdentity { format: "clap".into(), path: r"C:\x.clap".into(), id: "org.x".into() };
        assert_eq!(clap.file_name(0), "slot0-a77f40f9ad871724.tone");
        // The separators keep a moved boundary from hashing the same.
        let shifted = ToneIdentity { format: "clap".into(), path: r"C:\x.clapo".into(), id: "rg.x".into() };
        assert_ne!(shifted.file_name(0), clap.file_name(0));
    }

    #[test]
    fn a_tone_file_round_trips() {
        let t = tone(&[0, 1, 2, 250, 255]);
        let bytes = encode(&t);
        assert_eq!(&bytes[..4], b"BLTN");
        assert_eq!(decode(&bytes).unwrap(), t);
        let empty = tone(&[]);
        assert_eq!(decode(&encode(&empty)).unwrap(), empty, "a plugin may keep an empty state");
    }

    #[test]
    fn a_corrupt_truncated_or_foreign_file_is_refused_without_panic() {
        let bytes = encode(&tone(b"some plugin state"));
        for cut in 0..bytes.len() {
            assert!(decode(&bytes[..cut]).is_err(), "truncated at {cut}");
        }
        for at in 0..bytes.len() {
            let mut flipped = bytes.clone();
            flipped[at] ^= 0x40;
            assert!(decode(&flipped).is_err(), "a flipped byte at {at}");
        }
        let mut long = bytes.clone();
        long.push(0);
        assert!(decode(&long).is_err(), "trailing bytes");
        // A length prefix claiming more than the limit is refused before anything is sized from it.
        let mut huge = MAGIC.to_vec();
        huge.extend_from_slice(&VERSION.to_le_bytes());
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode(&huge).unwrap_err().contains("more than"));
        assert!(decode(b"RIFF....WAVE").is_err(), "not a tone at all");
    }

    #[test]
    fn a_vst3_container_round_trips_and_refuses_garbage() {
        let bytes = encode_vst3(b"component", b"controller");
        assert_eq!(decode_vst3(&bytes).unwrap(), (&b"component"[..], &b"controller"[..]));
        let none = encode_vst3(b"only the component", b"");
        assert_eq!(decode_vst3(&none).unwrap(), (&b"only the component"[..], &b""[..]));
        for cut in 0..bytes.len() {
            assert!(decode_vst3(&bytes[..cut]).is_err(), "truncated at {cut}");
        }
        assert!(decode_vst3(b"a CLAP plugin's state").is_err());
        let mut long = bytes.clone();
        long.push(7);
        assert!(decode_vst3(&long).is_err(), "trailing bytes");
    }

    #[test]
    fn a_vst2_container_round_trips_both_kinds_and_refuses_garbage() {
        let chunk = Vst2State::Chunk { program: 3, chunk: b"a bank chunk".to_vec() };
        let bytes = encode_vst2(&chunk);
        assert_eq!(&bytes[..4], b"LFV2");
        assert_eq!(bytes.len(), 15 + 12, "the overhead `VST2_MAX_CHUNK` leaves room for");
        assert_eq!(decode_vst2(&bytes).unwrap(), chunk);
        let params = Vst2State::Params { program: -1, values: vec![0.0, 0.25, 1.0, f32::MIN_POSITIVE] };
        let param_bytes = encode_vst2(&params);
        assert_eq!(decode_vst2(&param_bytes).unwrap(), params);
        let none = Vst2State::Params { program: 0, values: Vec::new() };
        assert_eq!(decode_vst2(&encode_vst2(&none)).unwrap(), none, "a plugin without parameters");
        for whole in [&bytes, &param_bytes] {
            for cut in 0..whole.len() {
                assert!(decode_vst2(&whole[..cut]).is_err(), "truncated at {cut}");
            }
            let mut long = whole.clone();
            long.push(7);
            assert!(decode_vst2(&long).is_err(), "trailing bytes");
        }
        assert!(decode_vst2(&encode_vst3(b"component", b"")).is_err(), "another format's container");
        let mut kind = bytes.clone();
        kind[6] = 9;
        assert!(decode_vst2(&kind).unwrap_err().contains("kind 9"));
        // A count the bytes do not hold is refused before anything is sized from it.
        let mut huge = param_bytes.clone();
        huge[11..15].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode_vst2(&huge).is_err());
        let mut long_chunk = bytes.clone();
        long_chunk[11..15].copy_from_slice(&(VST2_MAX_CHUNK as u32 + 1).to_le_bytes());
        assert!(decode_vst2(&long_chunk).unwrap_err().contains("more than"));
    }

    #[test]
    fn the_store_round_trips_replaces_atomically_and_refuses_a_bad_file() {
        let dir = TempDir::new("store");
        let store = ToneStore::new(dir.0.clone());
        assert_eq!(store.load(0, &identity()).unwrap(), None, "nothing stored yet");

        let first = tone(b"first");
        store.save_encoded(0, &encode(&first)).unwrap();
        assert_eq!(store.load(0, &identity()).unwrap(), Some(first));
        let second = tone(b"second, longer than the first");
        store.save_encoded(0, &encode(&second)).unwrap();
        assert_eq!(store.load(0, &identity()).unwrap(), Some(second), "replaced");
        let names: Vec<_> = std::fs::read_dir(&dir.0).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, [identity().file_name(0).as_str()], "one file, no temporary left behind");

        assert!(store.save_encoded(0, b"garbage").is_err(), "a file that does not decode is never stored");
        let path = dir.0.join(identity().file_name(0));
        let good = std::fs::read(&path).unwrap();
        std::fs::write(&path, &good[..good.len() / 2]).unwrap();
        assert!(store.load(0, &identity()).is_err(), "a truncated file is an error, not a tone");

        // A file under this identity's name that carries another plugin's identity.
        let mut other = tone(b"x");
        other.identity.id = "another".to_string();
        std::fs::write(&path, encode(&other)).unwrap();
        assert!(store.load(0, &identity()).unwrap_err().contains("holds the tone of"));
    }

    #[test]
    fn the_debounce_waits_for_quiet() {
        let t0 = Instant::now();
        let mut d = Debounce::default();
        assert!(!d.dirty() && !d.due(t0 + SAVE_QUIET * 10), "nothing changed");
        d.touch(t0);
        assert!(d.dirty());
        assert!(!d.due(t0 + SAVE_QUIET - Duration::from_millis(1)));
        assert!(d.due(t0 + SAVE_QUIET));
        d.touch(t0 + Duration::from_secs(1));
        assert!(!d.due(t0 + SAVE_QUIET), "a later change restarts the wait");
        assert!(d.due(t0 + Duration::from_secs(1) + SAVE_QUIET));
        d.clear();
        assert!(!d.dirty() && !d.due(t0 + SAVE_QUIET * 10));
    }

    #[test]
    fn a_keeper_saves_after_quiet() {
        let dir = TempDir::new("keeper");
        let edited = Arc::new(AtomicBool::new(false));
        let mut keeper = ToneKeeper::new(Some(dir.binding(0, identity())), edited.clone());
        let t0 = Instant::now();
        assert!(!keeper.poll(t0), "nothing changed");
        assert_eq!(keeper.stored().unwrap(), None);

        edited.store(true, Relaxed); // a host parameter set, from a command thread
        assert!(!keeper.poll(t0));
        assert!(keeper.dirty());
        assert!(keeper.poll(t0 + SAVE_QUIET), "due once the change is quiet");
        let saved = keeper.save("Pro-Q 3", || Ok(Some(b"state v".to_vec()))).unwrap();
        assert_eq!((decode(&saved.bytes).unwrap(), saved.not_stored), (tone(b"state v"), None));
        assert!(!keeper.dirty() && !keeper.poll(t0 + SAVE_QUIET * 3), "saved: nothing pending");
        assert_eq!(keeper.stored().unwrap().as_deref(), Some(&b"state v"[..]), "the next load finds it");

        assert!(keeper.save("x", || Ok(Some(vec![0; MAX_STATE_BYTES + 1]))).is_err(), "an oversized state is refused");
        assert_eq!(keeper.save("x", || Ok(None)).unwrap().bytes, Vec::<u8>::new(), "a plugin that keeps no state");
        edited.store(true, Relaxed);
        let mut fresh = ToneKeeper::new(Some(dir.binding(0, identity())), edited.clone());
        assert!(fresh.dirty());
        assert!(fresh.save("x", || Err("the plugin refused".to_string())).is_err());
        assert!(!fresh.dirty(), "a failed save waits for the next change");
    }

    #[test]
    fn a_load_that_could_not_restore_keeps_the_stored_tone_until_a_change() {
        let dir = TempDir::new("kept");
        dir.store().save_encoded(0, &encode(&tone(b"refused"))).unwrap();
        let edited = Arc::new(AtomicBool::new(false));
        let mut keeper = ToneKeeper::new(Some(dir.binding(0, identity())), edited.clone());
        assert_eq!(keeper.stored().unwrap().as_deref(), Some(&b"refused"[..]));
        edited.store(true, Relaxed); // what the plugin raised while it refused the state
        keeper.settle();
        keeper.keep_stored();
        assert!(!keeper.dirty(), "the restore itself is no change");
        let saved = keeper.save("Pro-Q 3", || Ok(Some(b"defaults".to_vec()))).unwrap();
        assert_eq!(decode(&saved.bytes).unwrap().state, b"defaults", "a save still hands back what plays");
        assert!(saved.not_stored.is_some());
        assert_eq!(keeper.stored().unwrap().as_deref(), Some(&b"refused"[..]), "the store keeps the tone");

        edited.store(true, Relaxed); // the player turns a knob
        assert!(keeper.dirty());
        let saved = keeper.save("Pro-Q 3", || Ok(Some(b"edited".to_vec()))).unwrap();
        assert_eq!(saved.not_stored, None);
        assert_eq!(keeper.stored().unwrap().as_deref(), Some(&b"edited"[..]), "a change replaces it");
    }

    #[test]
    fn each_slot_keeps_its_own_tone_of_the_same_plugin() {
        let dir = TempDir::new("slots");
        let mut a = ToneKeeper::new(Some(dir.binding(0, identity())), Arc::new(AtomicBool::new(false)));
        let mut b = ToneKeeper::new(Some(dir.binding(1, identity())), Arc::new(AtomicBool::new(false)));
        assert_eq!((a.stored().unwrap(), b.stored().unwrap()), (None, None));
        assert_eq!(a.save("Pro-Q 3", || Ok(Some(b"clean".to_vec()))).unwrap().not_stored, None);
        assert_eq!(b.save("Pro-Q 3", || Ok(Some(b"crunch".to_vec()))).unwrap().not_stored, None);
        assert_eq!(a.save("Pro-Q 3", || Ok(Some(b"clean, later".to_vec()))).unwrap().not_stored, None);

        // The next loads (a restart) find each slot's own tone, whichever saved last.
        let mut a = ToneKeeper::new(Some(dir.binding(0, identity())), Arc::new(AtomicBool::new(false)));
        let mut b = ToneKeeper::new(Some(dir.binding(1, identity())), Arc::new(AtomicBool::new(false)));
        assert_eq!(a.stored().unwrap().as_deref(), Some(&b"clean, later"[..]));
        assert_eq!(b.stored().unwrap().as_deref(), Some(&b"crunch"[..]));
    }

    #[test]
    fn an_import_supersedes_every_earlier_load_of_its_plugin_in_its_slot() {
        let dir = TempDir::new("import");
        let t0 = Instant::now();
        // The plugin in both slots, loaded before an import for slot A.
        let (a_edited, b_edited) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let mut a = ToneKeeper::new(Some(dir.binding(0, identity())), a_edited.clone());
        let mut b = ToneKeeper::new(Some(dir.binding(1, identity())), b_edited.clone());
        assert_eq!((a.stored().unwrap(), b.stored().unwrap()), (None, None));

        let imported = dir.store().import(0, &encode(&tone(b"imported")), &identity()).unwrap();
        assert_eq!((imported.tone.state.as_slice(), imported.revision), (&b"imported"[..], 1));
        a_edited.store(true, Relaxed);
        b_edited.store(true, Relaxed);
        assert!(!a.poll(t0 + SAVE_QUIET * 9), "slot A's load schedules no save any more");
        assert!(b.dirty(), "slot B's tone is its own: it still saves");
        let saved = a.save("Pro-Q 3", || Ok(Some(b"slot a".to_vec()))).unwrap();
        assert_eq!(decode(&saved.bytes).unwrap().state, b"slot a", "an export still gets this load's state");
        assert!(saved.not_stored.is_some());
        assert_eq!(b.save("Pro-Q 3", || Ok(Some(b"slot b".to_vec()))).unwrap().not_stored, None);
        assert_eq!(dir.store().load(0, &identity()).unwrap().unwrap().state, b"imported", "the store keeps the import");
        assert_eq!(dir.store().load(1, &identity()).unwrap().unwrap().state, b"slot b", "and slot B's own tone");

        // A load from after the import saves as usual.
        let mut later = ToneKeeper::new(Some(dir.binding(0, identity())), Arc::new(AtomicBool::new(false)));
        assert_eq!(later.stored().unwrap().as_deref(), Some(&b"imported"[..]));
        assert_eq!(later.save("Pro-Q 3", || Ok(Some(b"later".to_vec()))).unwrap().not_stored, None);

        // The reload the import asked for restores the imported bytes it was handed, not the file.
        let mut binding = dir.binding(0, identity());
        binding.imported = Some(imported);
        let mut reload = ToneKeeper::new(Some(binding), Arc::new(AtomicBool::new(false)));
        assert_eq!(reload.stored().unwrap().as_deref(), Some(&b"imported"[..]));
        assert_eq!(reload.save("Pro-Q 3", || Ok(Some(b"reloaded".to_vec()))).unwrap().not_stored, None, "and saves");
        assert_eq!(dir.store().load(0, &identity()).unwrap().unwrap().state, b"reloaded");
    }

    #[test]
    fn a_failed_or_foreign_import_changes_nothing() {
        let dir = TempDir::new("import-failed");
        dir.store().save_encoded(0, &encode(&tone(b"before"))).unwrap();
        let edited = Arc::new(AtomicBool::new(false));
        let mut keeper = ToneKeeper::new(Some(dir.binding(0, identity())), edited.clone());
        assert_eq!(keeper.stored().unwrap().as_deref(), Some(&b"before"[..]));

        // A tone file of another plugin than the one session.json names.
        let mut other = tone(b"other");
        other.identity.id = "another".to_string();
        let refused = dir.store().import(0, &encode(&other), &identity()).unwrap_err();
        assert!(refused.contains("not to the plugin session.json names"), "{refused}");
        assert!(dir.store().load(0, &other.identity).unwrap().is_none(), "nothing written under either plugin");

        // A write that fails: a folder stands where the file goes.
        let path = dir.0.join(identity().file_name(0));
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(dir.store().import(0, &encode(&tone(b"imported")), &identity()).is_err());
        std::fs::remove_dir(&path).unwrap();
        assert_eq!(dir.store().revision(0, &identity()), 0, "neither counts as an import");

        edited.store(true, Relaxed);
        assert!(keeper.dirty(), "the loaded plugin still saves");
        assert_eq!(keeper.save("Pro-Q 3", || Ok(Some(b"after".to_vec()))).unwrap().not_stored, None);
        assert_eq!(dir.store().load(0, &identity()).unwrap().unwrap().state, b"after");
    }

    #[test]
    fn a_write_stalled_under_the_store_lock_holds_up_no_owner_turn() {
        let dir = TempDir::new("stall");
        let other = ToneIdentity { format: "clap".into(), path: r"C:\other.clap".into(), id: "org.other".into() };
        // Two owners from before the import: the imported plugin, dirty, and another plugin.
        let edited = Arc::new(AtomicBool::new(false));
        let mut owner = ToneKeeper::new(Some(dir.binding(0, identity())), edited.clone());
        let mut elsewhere = ToneKeeper::new(Some(dir.binding(0, other.clone())), Arc::new(AtomicBool::new(false)));
        assert_eq!((owner.stored().unwrap(), elsewhere.stored().unwrap()), (None, None));
        edited.store(true, Relaxed);

        // The import stalls in the file system, holding its plugin's write lock.
        let (entered, go) = dir.store().stall_next_write();
        let store = dir.store().clone();
        let import = std::thread::spawn(move || store.import(0, &encode(&tone(b"imported")), &identity()));
        entered.recv_timeout(Duration::from_secs(5)).expect("the import reached its write");

        // Each owner's turn on a thread of its own, so a turn that blocks fails the test instead of hanging it.
        let (tx, turns) = std::sync::mpsc::channel();
        let turn = std::thread::spawn(move || {
            let dirty = owner.dirty();
            let due = owner.poll(Instant::now() + SAVE_QUIET * 2);
            let _ = tx.send((dirty, due));
            let saved = elsewhere.save("Other", || Ok(Some(b"other".to_vec())));
            let _ = tx.send((saved.is_ok_and(|s| s.not_stored.is_none()), true));
            owner
        });
        let polled = turns.recv_timeout(Duration::from_secs(30));
        let other_saved = turns.recv_timeout(Duration::from_secs(30));
        go.send(()).unwrap();
        assert_eq!(polled.expect("the owner's turn waited on another operation's write"), (true, true), "not counted yet");
        assert_eq!(other_saved.expect("another plugin's save waited on this plugin's write"), (true, true));

        // The import lands and counts; the save the turn scheduled then stores nothing over it.
        let mut owner = turn.join().unwrap();
        assert_eq!(import.join().unwrap().unwrap().revision, 1);
        let saved = owner.save("Pro-Q 3", || Ok(Some(b"slot a".to_vec()))).unwrap();
        assert!(saved.not_stored.is_some(), "the save checks again under the lock");
        assert!(!owner.dirty(), "and the owner schedules nothing more");
        assert_eq!(dir.store().load(0, &identity()).unwrap().unwrap().state, b"imported");
        assert_eq!(dir.store().load(0, &other).unwrap().unwrap().state, b"other");
    }

    #[test]
    fn a_parked_tone_goes_only_to_the_load_that_passes_its_token() {
        let dir = TempDir::new("handoff");
        let imported = dir.store().import(0, &encode(&tone(b"imported")), &identity()).unwrap();
        let mut handoff = ToneHandoff::<2>::default();

        // A load without the token (the player's own pick of the plugin) takes nothing and drops it.
        let token = handoff.park(0, imported.clone());
        assert!(handoff.take(0, None, &identity()).is_none());
        assert!(handoff.take(0, Some(token), &identity()).is_none(), "gone after that load");

        // Another token, the other slot, another plugin: none takes it, and each drops it.
        let token = handoff.park(0, imported.clone());
        assert!(handoff.take(0, Some(token + 1), &identity()).is_none());
        assert!(handoff.take(0, Some(token), &identity()).is_none());
        let token = handoff.park(0, imported.clone());
        assert!(handoff.take(1, Some(token), &identity()).is_none(), "slot B's load");
        let mut other = identity();
        other.id = "another".into();
        assert!(handoff.take(0, Some(token), &other).is_none(), "another plugin in slot A");

        // A skipped reload drops it; a stale token does not drop a newer import's tone.
        let skipped = handoff.park(0, imported.clone());
        handoff.forget(0, skipped);
        assert!(handoff.take(0, Some(skipped), &identity()).is_none());
        let newer = handoff.park(0, imported.clone());
        handoff.forget(0, skipped);
        assert_ne!(newer, skipped);
        let taken = handoff.take(0, Some(newer), &identity()).expect("the reload's own load takes it");
        assert_eq!((taken.tone.state.as_slice(), taken.revision), (&b"imported"[..], imported.revision));
        assert!(handoff.take(0, Some(newer), &identity()).is_none(), "once");
    }

    #[test]
    fn a_unit_tone_round_trips_through_its_own_file() {
        assert_eq!(ToneIdentity::unit_file_name(7), "unit-7.tone");
        let dir = TempDir::new("unit-roundtrip");
        let mut keeper = ToneKeeper::new(Some(dir.unit_binding(7, 0, identity())), Arc::new(AtomicBool::new(false)));
        assert_eq!(keeper.stored().unwrap(), None);
        let saved = keeper.save("Pro-Q 3", || Ok(Some(b"unit state".to_vec()))).unwrap();
        assert_eq!(saved.not_stored, None);
        assert!(dir.0.join("unit-7.tone").exists(), "unit has its own file");
        assert!(!dir.0.join(identity().file_name(0)).exists(), "slot file is untouched");

        let mut reloaded = ToneKeeper::new(Some(dir.unit_binding(7, 0, identity())), Arc::new(AtomicBool::new(false)));
        assert_eq!(reloaded.stored().unwrap().as_deref(), Some(&b"unit state"[..]));
    }

    #[test]
    fn retire_promotes_the_unit_file_to_the_slot_and_the_next_unit_starts_from_it() {
        let dir = TempDir::new("unit-retire");
        let mut first = ToneKeeper::new(Some(dir.unit_binding(1, 0, identity())), Arc::new(AtomicBool::new(false)));
        assert_eq!(first.stored().unwrap(), None);
        assert_eq!(first.save("Pro-Q 3", || Ok(Some(b"dialed in".to_vec()))).unwrap().not_stored, None);
        assert_eq!(dir.store().revision(0, &identity()), 0);

        first.retire();
        assert!(!dir.0.join("unit-1.tone").exists(), "unit file moved into the slot");
        assert_eq!(dir.store().revision(0, &identity()), 1, "promotion is counted on the slot");
        assert_eq!(dir.store().load(0, &identity()).unwrap().unwrap().state, b"dialed in");

        // The next unit of the same plugin in the slot starts from the slot's file, read only.
        let mut next = ToneKeeper::new(Some(dir.unit_binding(2, 0, identity())), Arc::new(AtomicBool::new(false)));
        assert_eq!(next.stored().unwrap().as_deref(), Some(&b"dialed in"[..]));
        assert!(!dir.0.join("unit-2.tone").exists(), "fallback read writes nothing");

        // Retiring a unit that never saved its own file leaves the slot's file as it was.
        next.retire();
        assert_eq!(dir.store().load(0, &identity()).unwrap().unwrap().state, b"dialed in");
        assert_eq!(dir.store().revision(0, &identity()), 1);
    }

    // Unit contract § 6: a v0.8.9 tone carries over. The bytes are v0.8.9's `encode` of one tone (run from
    // `git show v0.8.9:src-tauri/src/host/tone.rs`, its `encode`, `put_bytes` and `fnv1a64` alone), stored
    // as v0.8.9 named it (`slot0-<hash>.tone`); today's `encode` writes the same bytes. A chain unit of that
    // plugin in that slot with a fresh id starts from it, read only: no unit file, the slot file untouched.
    #[test]
    fn a_v089_slot_tone_is_where_a_fresh_unit_of_its_plugin_starts() {
        const V089: [u8; 64] = [
            0x42, 0x4c, 0x54, 0x4e, 0x01, 0x00, 0x04, 0x00, 0x00, 0x00, 0x63, 0x6c, 0x61, 0x70, 0x09, 0x00, 0x00, 0x00, 0x43, 0x3a,
            0x5c, 0x78, 0x2e, 0x63, 0x6c, 0x61, 0x70, 0x05, 0x00, 0x00, 0x00, 0x6f, 0x72, 0x67, 0x2e, 0x78, 0x01, 0x00, 0x00, 0x00,
            0x58, 0x0b, 0x00, 0x00, 0x00, 0x76, 0x30, 0x2e, 0x38, 0x2e, 0x39, 0x20, 0x74, 0x6f, 0x6e, 0x65, 0x3a, 0xef, 0x98, 0xdc,
            0xd1, 0x6b, 0x65, 0x8a,
        ];
        let clap = ToneIdentity { format: "clap".into(), path: r"C:\x.clap".into(), id: "org.x".into() };
        let v089 = Tone { identity: clap.clone(), name: "X".into(), state: b"v0.8.9 tone".to_vec() };
        assert_eq!(encode(&v089), V089, "today's encode writes v0.8.9's bytes");
        let dir = TempDir::new("v089-slot");
        std::fs::create_dir_all(&dir.0).unwrap();
        let slot_file = dir.0.join("slot0-a77f40f9ad871724.tone");
        assert_eq!(clap.file_name(0), "slot0-a77f40f9ad871724.tone");
        std::fs::write(&slot_file, V089).unwrap();

        let key = ToneKey::Unit { unit: 4242, slot: 0, identity: clap.clone() };
        let (read, revision) = dir.store().load_for_restore_key(&key);
        assert_eq!((read.unwrap(), revision), (Some(v089), 0));
        let mut unit = ToneKeeper::new(Some(dir.unit_binding(4242, 0, clap.clone())), Arc::new(AtomicBool::new(false)));
        assert_eq!(unit.stored().unwrap().as_deref(), Some(&b"v0.8.9 tone"[..]));
        assert!(!dir.0.join("unit-4242.tone").exists(), "the fallback read writes no unit file");
        assert_eq!(std::fs::read(&slot_file).unwrap(), V089, "the slot file is untouched");
        assert_eq!(dir.store().revision(0, &clap), 0);
        // The other slot has no tone of it.
        let other = ToneKeeper::new(Some(dir.unit_binding(4243, 1, clap)), Arc::new(AtomicBool::new(false))).stored().unwrap();
        assert_eq!(other, None);
    }

    #[test]
    fn two_units_of_one_plugin_in_one_slot_keep_two_files() {
        let dir = TempDir::new("two-units");
        let mut u1 = ToneKeeper::new(Some(dir.unit_binding(10, 0, identity())), Arc::new(AtomicBool::new(false)));
        let mut u2 = ToneKeeper::new(Some(dir.unit_binding(11, 0, identity())), Arc::new(AtomicBool::new(false)));
        assert_eq!((u1.stored().unwrap(), u2.stored().unwrap()), (None, None));
        assert_eq!(u1.save("Pro-Q 3", || Ok(Some(b"eq pre".to_vec()))).unwrap().not_stored, None);
        assert_eq!(u2.save("Pro-Q 3", || Ok(Some(b"eq post".to_vec()))).unwrap().not_stored, None);

        let mut names: Vec<_> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["unit-10.tone", "unit-11.tone"]);

        let mut u1_again = ToneKeeper::new(Some(dir.unit_binding(10, 0, identity())), Arc::new(AtomicBool::new(false)));
        let mut u2_again = ToneKeeper::new(Some(dir.unit_binding(11, 0, identity())), Arc::new(AtomicBool::new(false)));
        assert_eq!(u1_again.stored().unwrap().as_deref(), Some(&b"eq pre"[..]));
        assert_eq!(u2_again.stored().unwrap().as_deref(), Some(&b"eq post"[..]));
    }

    #[test]
    fn check_and_tokens_refuse_another_plugins_identity() {
        let mut other = identity();
        other.id = "another".into();
        let checked = ToneStore::check(&encode(&tone(b"ok")), &identity()).unwrap();
        assert_eq!(checked.state, b"ok");
        assert!(ToneStore::check(&encode(&tone(b"ok")), &other).is_err());

        let mut tokens = ToneTokens::default();
        let token = tokens.park(checked);
        assert!(tokens.take(token, &other).is_none(), "another plugin's identity is refused");
        assert_eq!(tokens.take(token, &identity()).unwrap().state, b"ok", "its own plugin takes it");
        assert!(tokens.take(token, &identity()).is_none(), "once");

        let dropped = tokens.park(tone(b"skipped"));
        tokens.forget(dropped);
        assert!(tokens.take(dropped, &identity()).is_none());
    }

    #[test]
    fn redeem_supersedes_an_earlier_unit_load() {
        let dir = TempDir::new("unit-redeem");
        let t0 = Instant::now();
        let key = ToneKey::Unit { unit: 4, slot: 0, identity: identity() };
        let edited = Arc::new(AtomicBool::new(false));
        let mut old = ToneKeeper::new(Some(dir.unit_binding(4, 0, identity())), edited.clone());
        assert_eq!(old.stored().unwrap(), None);

        // A tone belonging to another plugin is refused without writing or counting.
        let mut foreign = tone(b"foreign");
        foreign.identity.id = "another".into();
        assert!(dir.store().redeem(&key, foreign).is_err());
        assert!(!dir.0.join("unit-4.tone").exists());

        let imported = dir.store().redeem(&key, tone(b"redeemed")).unwrap();
        assert_eq!((imported.tone.state.as_slice(), imported.revision), (&b"redeemed"[..], 1));

        // The outgoing load drops its saves from here on.
        edited.store(true, Relaxed);
        assert!(!old.poll(t0 + SAVE_QUIET * 4));
        assert!(!old.dirty());
        let saved = old.save("Pro-Q 3", || Ok(Some(b"stale".to_vec()))).unwrap();
        assert!(saved.not_stored.is_some());
        let (stored, rev) = dir.store().load_for_restore_key(&key);
        assert_eq!((stored.unwrap().unwrap().state.as_slice(), rev), (&b"redeemed"[..], 1));

        // The reloaded unit restores the imported bytes and can save again.
        let mut binding = dir.unit_binding(4, 0, identity());
        binding.imported = Some(imported);
        let mut fresh = ToneKeeper::new(Some(binding), Arc::new(AtomicBool::new(false)));
        assert_eq!(fresh.stored().unwrap().as_deref(), Some(&b"redeemed"[..]));
        assert_eq!(fresh.save("Pro-Q 3", || Ok(Some(b"edited".to_vec()))).unwrap().not_stored, None);
        let (stored, _) = dir.store().load_for_restore_key(&key);
        assert_eq!(stored.unwrap().unwrap().state, b"edited");
    }

    #[test]
    fn a_redeem_between_a_keepers_load_and_its_retire_is_kept() {
        let dir = TempDir::new("unit-retire-race");
        let key = ToneKey::Unit { unit: 5, slot: 0, identity: identity() };
        let mut keeper = ToneKeeper::new(Some(dir.unit_binding(5, 0, identity())), Arc::new(AtomicBool::new(false)));
        assert_eq!(keeper.stored().unwrap(), None);
        assert_eq!(keeper.save("Pro-Q 3", || Ok(Some(b"stale".to_vec()))).unwrap().not_stored, None);

        // The redeem stalls mid-write under the unit file's lock, not counted yet: the keeper's
        // lock-free check passes, and its retire waits on the lock until the redeem has landed.
        let (entered, go) = dir.store().stall_next_write();
        let store = dir.store().clone();
        let redeem = std::thread::spawn(move || store.redeem(&key, tone(b"redeemed")));
        entered.recv_timeout(Duration::from_secs(5)).expect("the redeem reached its write");
        let retire = std::thread::spawn(move || keeper.retire());
        std::thread::sleep(Duration::from_millis(200));
        go.send(()).unwrap();
        assert_eq!(redeem.join().unwrap().unwrap().revision, 1);
        retire.join().unwrap();

        let (stored, _) = dir.store().load_for_restore_key(&ToneKey::Unit { unit: 5, slot: 0, identity: identity() });
        assert_eq!(stored.unwrap().unwrap().state, b"redeemed", "the unit file keeps the redeemed bytes");
        assert!(!dir.0.join(identity().file_name(0)).exists(), "the slot file is untouched");
        assert_eq!(dir.store().revision(0, &identity()), 0);
    }

    #[test]
    fn tone_tokens_evicts_oldest_past_max_parked_tones() {
        let mut tokens = ToneTokens::default();
        let mut issued = Vec::with_capacity(MAX_PARKED_TONES);
        for i in 0..MAX_PARKED_TONES {
            issued.push(tokens.park(tone(&[i as u8])));
        }
        let extra = tokens.park(tone(b"extra"));
        assert!(tokens.take(issued[0], &identity()).is_none(), "oldest parked tone was evicted");
        assert_eq!(tokens.take(issued[1], &identity()).unwrap().state, &[1]);
        assert_eq!(tokens.take(extra, &identity()).unwrap().state, b"extra");
    }
}
