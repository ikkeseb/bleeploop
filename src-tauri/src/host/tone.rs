//! OWNS: tone recall's pure half (engine mode): the tone file, the store, the VST3 state container and
//! the save debounce. A TONE is a plugin's saved state: a CLAP plugin's `state` blob, or a VST3 plugin's
//! component and edit-controller states in one container (`encode_vst3`). The tone belongs to the slot
//! AND the plugin: the store keeps one file per slot and plugin identity (format, path, id) in the
//! app-local data folder's `tones/`, named by the slot and a stable FNV-1a hash of that identity and
//! replaced atomically. So the same plugin in both slots keeps two tones, and a slot that swaps its
//! plugin and back gets its tone back. The same file is what a session export carries per slot.
//!
//! Nothing here touches a plugin. The engine-mode owners (`clap_engine`, `vst3_engine`) restore a tone
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

/// How long a plugin must go without a change before its tone is saved. A kill loses at most this.
pub(crate) const SAVE_QUIET: Duration = Duration::from_secs(2);

/// The largest state a tone holds. A session import sizes its archive limit from it
/// (`src/session/import.ts`, `MAX_TONE_BYTES`); change them together.
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

/// Which plugin a tone belongs to: the scan descriptor's format (`clap` | `vst3`), bundle path and
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

    fn sized(&mut self, max: usize, what: &str) -> Result<&'a [u8], String> {
        let b = self.take(4, what)?;
        let len = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize;
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
    if format != "clap" && format != "vst3" {
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

    fn path(&self, slot: usize, identity: &ToneIdentity) -> PathBuf {
        self.dir.join(identity.file_name(slot))
    }

    fn entry(&self, slot: usize, identity: &ToneIdentity) -> Arc<Entry> {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.entry(identity.file_name(slot)).or_default().clone()
    }

    /// How many session imports have replaced `identity`'s tone in `slot` this run. Waits on no write.
    pub(crate) fn revision(&self, slot: usize, identity: &ToneIdentity) -> u64 {
        self.entry(slot, identity).revision.load(Acquire)
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
        let path = self.path(slot, identity);
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

    /// A session import: store a tone file's bytes (checked first) for `expected`, the plugin session.json
    /// names for `slot`, then count the import, so no load of that plugin in that slot from before it
    /// saves over it. A tone file of another plugin is refused before anything is written, and a failed
    /// write counts nothing.
    pub(crate) fn import(&self, slot: usize, bytes: &[u8], expected: &ToneIdentity) -> Result<Imported, String> {
        let tone = decode(bytes)?;
        if &tone.identity != expected {
            return Err(format!(
                "the session's tone file belongs to {} ({}), not to the plugin session.json names ({})",
                tone.name, tone.identity.path, expected.path
            ));
        }
        let entry = self.entry(slot, &tone.identity);
        let _write = entry.write();
        self.write(slot, &tone.identity, bytes)?;
        let revision = entry.revision.fetch_add(1, Release) + 1;
        Ok(Imported { revision, tone })
    }

    /// A load's save: store a tone file's bytes for `identity` in `slot` unless a session import replaced
    /// that tone after the load read it (`revision`, from `load_for_restore`). `Ok(false)`: not stored.
    fn save_from_load(&self, slot: usize, identity: &ToneIdentity, revision: u64, bytes: &[u8]) -> Result<bool, String> {
        let entry = self.entry(slot, identity);
        let _write = entry.write();
        if entry.revision.load(Acquire) != revision {
            return Ok(false);
        }
        self.write(slot, identity, bytes)?;
        Ok(true)
    }

    /// Under the file's write lock only (`Entry::write`).
    fn write(&self, slot: usize, identity: &ToneIdentity, bytes: &[u8]) -> Result<(), String> {
        #[cfg(test)]
        {
            let stall = self.stall.lock().unwrap_or_else(PoisonError::into_inner).take();
            if let Some((entered, go)) = stall {
                let _ = entered.send(());
                let _ = go.recv();
            }
        }
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("{}: {e}", self.dir.display()))?;
        write_atomic(&self.path(slot, identity), bytes)
    }

    /// Test-only: store a tone file's bytes for `slot` under the identity they carry, as a save from
    /// nowhere would.
    #[cfg(test)]
    pub(crate) fn save_encoded(&self, slot: usize, bytes: &[u8]) -> Result<Tone, String> {
        let tone = decode(bytes)?;
        let entry = self.entry(slot, &tone.identity);
        let _write = entry.write();
        self.write(slot, &tone.identity, bytes)?;
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

/// Where a loaded plugin's tone lives: the store, the slot and the plugin's identity.
#[derive(Clone, Debug)]
pub(crate) struct ToneBinding {
    pub(crate) store: ToneStore,
    pub(crate) slot: usize,
    pub(crate) identity: ToneIdentity,
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
        let (stored, revision) = b.store.load_for_restore(b.slot, &b.identity);
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

    /// An owner-thread change (a CLAP `mark_dirty` or params rescan, a VST3 re-list).
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
        self.binding.as_ref().is_some_and(|b| b.store.revision(b.slot, &b.identity) != self.revision)
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
            Some(b) => b.identity.clone(),
            None => ToneIdentity { format: String::new(), path: String::new(), id: String::new() },
        };
        let name: String = name.chars().take(256).collect();
        let bytes = encode(&Tone { identity, name, state });
        let not_stored = match &self.binding {
            None => None,
            Some(_) if self.keep_stored => Some("the stored tone this load could not restore is kept until something changes"),
            Some(b) => (!b.store.save_from_load(b.slot, &b.identity, self.revision, &bytes)?)
                .then_some("a session import replaced this slot's tone after this load"),
        };
        Ok(Saved { bytes, not_stored })
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
        ToneBinding { store: self.1.clone(), slot, identity, imported: None }
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
        let polled = turns.recv_timeout(Duration::from_secs(2));
        let other_saved = turns.recv_timeout(Duration::from_secs(2));
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
}
