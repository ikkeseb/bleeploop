//! OWNS: the player's MIDI-learn bindings on disk, `midi-bindings.json` beside `plugin-folders.json`, and
//! the one-time import of the WebView's `localStorage` list (`lf.midiLearn`, `src/app/midi-actions.ts`):
//! the plan's decisions 8 and 9. Native MIDI's host keeps one (`super::Core`, dormant with it), which
//! decides which records the learn model runs: the port resolution is its, never this file's.
//!
//! This file is USER DATA, as the folder list is (`host/folders.rs`): only a MISSING file is an empty
//! store. One that cannot be read or parsed, or that a newer build wrote, is reported to the caller
//! ([`LoadResult`]) and leaves the store read-only for the session: what an import or an edit brings works
//! in memory, and no write ever replaces that file. A stored record this build cannot read is reported by
//! its index and kept in the document's `rejected` list as its JSON text, as is a legacy record the import
//! cannot read, so a later write never loses either. Kept as text, a record adds no nesting to the
//! document, so it can never push the file past the JSON parser's depth limit; and every document is read
//! back before it is written, so no write can leave a file the next load refuses.
//!
//! Writes happen off the router's lock. Under it the caller takes a [`Snapshot`] (two `Arc` clones and the
//! store's revision); after releasing it, [`Snapshot::write`]. Every snapshot of one store shares one write
//! slot that serializes the writes and remembers the newest snapshot written, so an older snapshot is
//! skipped: two writers racing never leave an older list over a newer one. A write is
//! `host::tone::write_atomic` (temp file, `sync_all`, rename), the folder list's.
//!
//! The app is not single-instance, and an older build may run beside a newer one, so a write first reads
//! the file again. One this build cannot read, or of another version, makes the store read-only (the
//! file is kept); one whose document `revision` is not the one this store last loaded or wrote is a
//! conflict: another instance wrote it, and the caller reloads rather than write over it. (Between that
//! read and the rename another process can still write: no lock spans the two.)
//!
//! # The legacy import
//!
//! A web record's port id (`input-<N>`) is a per-run ordinal (the plan's § Step 0 findings), so a legacy
//! record keeps it as its `port_id`, marked `ordinal`, and its port NAME is its identity: two records on
//! `input-1` with different names are two controllers. Whether a record activates is decided at
//! resolution by the name rule, elsewhere; resolution persists the port it chose with
//! [`Store::reanchor`], and the player's choice with [`Store::assign`], both of which give the record a
//! real port id. The import itself only refuses to guess where a renumbered run may have relearned a
//! control: records of one port name, from several legacy ids, that bind the same message are all
//! blocked until the player assigns them. A blocked record is listed, with why, but never reaches the
//! learn model. The import runs once: the document's `legacy.imported` says it ran, and
//! [`Store::legacy_durable`] says a write holding it has completed, the UI's cue to drop `lf.midiLearn`
//! (one release later). A file that says `imported` is itself that write, so the flag is not stored twice.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::bindings::{parse_binding, Binding, Kind, Rejected};
use crate::host::tone::write_atomic;

/// The store's file, in the folder the caller passes (the one holding `plugin-folders.json`).
pub const FILE_NAME: &str = "midi-bindings.json";

/// Bump when the shape changes; a file of any other version is refused, never rewritten.
const VERSION: u64 = 1;

/// Where a binding came from: learned natively, or imported from the web's list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    Native,
    Legacy,
}

/// One stored binding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    origin: Origin,
    /// The port id is a legacy run's ordinal, not a port's identity: the port name is. Set by the import;
    /// cleared when resolution or the player gives the record a real port id.
    ordinal: bool,
    /// Why it waits for the player's assignment, when it does.
    blocked: Option<String>,
    binding: Binding,
}

impl Record {
    /// This record and binding `b` (whose id is an ordinal or not) listen to the same message on the same
    /// port. An ordinal id names a port only together with the port name.
    fn on_control(&self, b: &Binding, ordinal: bool) -> bool {
        let named = !(self.ordinal || ordinal) || self.binding.port_name == b.port_name;
        named && control(&self.binding) == control(b)
    }

    fn active(&self) -> bool {
        self.blocked.is_none()
    }
}

/// A record in the document's `bindings` list, read with the binding's own checks (`parse_binding`).
fn parse_record(value: Value) -> Result<Record, String> {
    #[derive(Deserialize)]
    struct Raw {
        origin: Origin,
        ordinal: bool,
        blocked: Option<String>,
        binding: Value,
    }
    let raw: Raw = serde_json::from_value(value).map_err(|e| e.to_string())?;
    Ok(Record { origin: raw.origin, ordinal: raw.ordinal, blocked: raw.blocked, binding: parse_binding(raw.binding)? })
}

/// The message a binding listens to on its port id.
fn control(b: &Binding) -> (&str, u8, Kind, u8) {
    (&b.port_id, b.channel, b.kind, b.number)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Legacy {
    imported: bool,
}

/// The file, as written.
#[derive(Serialize)]
struct Document<'a> {
    version: u64,
    /// Raised by every write: a write refuses a file whose revision is not the one it expects.
    revision: u64,
    bindings: &'a [Record],
    /// Records this build could not read, each as `{"from": "store" | "legacy", "record": "<JSON text>"}`
    /// when this build set it aside; entries already in the file are kept as they are.
    rejected: &'a [Value],
    legacy: Legacy,
}

/// The file, as read once its version is known to be this build's.
#[derive(Deserialize)]
struct OnDisk {
    revision: u64,
    bindings: Vec<Value>,
    rejected: Vec<Value>,
    legacy: Legacy,
}

/// A record set aside, as its JSON text, tagged with the list it came from.
fn set_aside(from: &str, record: &Value) -> Value {
    let text = match record {
        Value::String(text) if from == "legacy" => text.clone(),
        other => other.to_string(),
    };
    serde_json::json!({ "from": from, "record": text })
}

/// What [`load`] found. Only `Missing` and `Loaded` leave the store writable; the other two are the
/// caller's to report (release log and UI), and the file stays as it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadResult {
    /// No file: an empty store, written on its first change.
    Missing,
    /// The file read; `rejected` names each stored record that did not (kept in the file).
    Loaded { rejected: Vec<Rejected> },
    /// The file cannot be read, is not JSON, or does not have the document's shape.
    Invalid(String),
    /// A build that reads another version wrote it.
    UnsupportedVersion(u64),
}

/// A binding as the UI lists it: every stored one, blocked or not.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Listed {
    pub binding: Binding,
    pub origin: Origin,
    /// Its port id is a legacy run's ordinal: the port name is its identity until resolution or the
    /// player gives it a real one.
    pub ordinal: bool,
    /// Why it waits for the player's assignment ([`Store::assign`]), when it does.
    pub blocked: Option<String>,
    /// The port as the player knows it: the stored port name (the port id when the name is empty).
    pub display_name: String,
}

/// A legacy record the import blocked, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Blocked {
    /// Its position in the legacy list.
    pub index: usize,
    pub why: String,
}

/// What [`Store::import_legacy`] did, by each record's position in the legacy list.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    /// The import had already run: nothing changed.
    pub already: bool,
    /// The legacy document was not a JSON array: it is kept as one rejected entry.
    pub unreadable: Option<String>,
    /// Imported and not blocked.
    pub imported: Vec<usize>,
    /// Imported, waiting for the player's assignment.
    pub blocked: Vec<Blocked>,
    /// Not readable; kept as text.
    pub rejected: Vec<Rejected>,
    /// The store already held a binding on that control (same port, channel, kind, number).
    pub skipped: Vec<usize>,
}

/// What [`Store::reanchor`] did, by listed index.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reanchored {
    /// Records now on the new port.
    pub moved: Vec<usize>,
    /// Records the new port already has a binding for: left where they were, blocked.
    pub blocked: Vec<usize>,
}

/// What one [`Snapshot::write`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Written {
    Wrote,
    /// A newer snapshot, or this one, is already written: nothing was.
    Skipped,
}

/// Why [`Snapshot::write`] wrote nothing. The file is as it was in every case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteError {
    /// The store is read-only for the session: the load, or this write's reread, found a file this build
    /// cannot read or of another version.
    ReadOnly(String),
    /// Another instance wrote the file since this store loaded or wrote it: reload before editing again.
    Conflict(String),
    /// The write itself failed (or its document would not read back); a later write may succeed.
    Failed(String),
}

/// What every snapshot of one store shares.
#[derive(Default)]
struct Slot {
    /// Serializes the writes. Holds the document revision this store last loaded or wrote (0: no file).
    disk: Mutex<u64>,
    /// The newest snapshot revision written (0: the store as loaded).
    written: AtomicU64,
    /// Why no write may replace the file, once that is known; never cleared in a session.
    read_only: OnceLock<String>,
}

impl Slot {
    fn refuse(&self, why: String) -> WriteError {
        let _ = self.read_only.set(why);
        WriteError::ReadOnly(self.read_only.get().cloned().unwrap_or_default())
    }
}

/// The bindings, in the order the UI lists them, and what a write needs. Kept under the caller's lock;
/// never touches the disk itself.
pub struct Store {
    records: Arc<Vec<Record>>,
    rejected: Arc<Vec<Value>>,
    imported: bool,
    /// The revision whose write makes the import durable; `None` when the loaded file already held it.
    import_revision: Option<u64>,
    /// Raised by every change; 0 is the file as loaded.
    revision: u64,
    slot: Arc<Slot>,
}

/// A document as this build reads it.
struct Decoded {
    revision: u64,
    records: Vec<Record>,
    /// The file's `rejected` list, then each stored record that did not read, set aside.
    kept: Vec<Value>,
    rejected: Vec<Rejected>,
    imported: bool,
}

/// Read a document; `Err` is the [`LoadResult`] that refuses it.
fn decode(file: &Path, bytes: &[u8]) -> Result<Decoded, LoadResult> {
    let invalid = |e: serde_json::Error| LoadResult::Invalid(format!("{}: {e}", file.display()));
    let value: Value = serde_json::from_slice(bytes).map_err(invalid)?;
    #[derive(Deserialize)]
    struct Head {
        version: u64,
    }
    let Head { version } = serde_json::from_value(value.clone()).map_err(invalid)?;
    if version != VERSION {
        return Err(LoadResult::UnsupportedVersion(version));
    }
    let doc: OnDisk = serde_json::from_value(value).map_err(invalid)?;
    let mut decoded =
        Decoded { revision: doc.revision, records: Vec::new(), kept: doc.rejected, rejected: Vec::new(), imported: doc.legacy.imported };
    for (index, value) in doc.bindings.into_iter().enumerate() {
        match parse_record(value.clone()) {
            Ok(record) => decoded.records.push(record),
            Err(reason) => {
                decoded.rejected.push(Rejected { index, reason });
                decoded.kept.push(set_aside("store", &value));
            }
        }
    }
    Ok(decoded)
}

/// Why a refused document leaves the store read-only.
fn refusal(file: &Path, result: LoadResult) -> String {
    match result {
        LoadResult::Invalid(why) => why,
        LoadResult::UnsupportedVersion(n) => format!("{}: version {n} (this build reads version {VERSION})", file.display()),
        LoadResult::Missing | LoadResult::Loaded { .. } => unreachable!("decode refuses only as Invalid or UnsupportedVersion"),
    }
}

/// An empty store with no file behind it (no data folder): it works in memory, and nothing writes it.
pub fn empty() -> Store {
    Store { records: Arc::default(), rejected: Arc::default(), imported: false, import_revision: None, revision: 0, slot: Arc::default() }
}

/// Read the store in `dir`. The store is always usable; [`LoadResult`] says whether it can be written.
pub fn load(dir: &Path) -> (Store, LoadResult) {
    let file = dir.join(FILE_NAME);
    let mut store = empty();
    let result = match std::fs::read(&file) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => LoadResult::Missing,
        Err(e) => LoadResult::Invalid(format!("{}: {e}", file.display())),
        Ok(bytes) => match decode(&file, &bytes) {
            Ok(doc) => {
                store.records = Arc::new(doc.records);
                store.rejected = Arc::new(doc.kept);
                store.imported = doc.imported;
                *store.slot.disk.lock().unwrap_or_else(PoisonError::into_inner) = doc.revision;
                LoadResult::Loaded { rejected: doc.rejected }
            }
            Err(refused) => refused,
        },
    };
    if matches!(result, LoadResult::Invalid(_) | LoadResult::UnsupportedVersion(_)) {
        let _ = store.slot.read_only.set(refusal(&file, result.clone()));
    }
    (store, result)
}

impl Store {
    fn bump(&mut self) {
        self.revision += 1;
    }

    /// Raised by every change (0: the file as loaded); a [`Snapshot`] carries the one it was taken at.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// False once the file proved unreadable or of another version: every write is refused this session.
    pub fn writable(&self) -> bool {
        self.slot.read_only.get().is_none()
    }

    /// The legacy import ran and a write holding it has completed (or the loaded file already held it):
    /// `lf.midiLearn` may go.
    pub fn legacy_durable(&self) -> bool {
        self.imported && self.import_revision.is_none_or(|r| self.slot.written.load(Ordering::Acquire) >= r)
    }

    /// Every binding not blocked, in list order. Which of them the learn model runs is the port
    /// resolution's (`super::Core`): never an ordinal one, nor one no present port answers to.
    #[cfg(test)]
    pub fn bindings(&self) -> Vec<Binding> {
        self.records.iter().filter(|r| r.active()).map(|r| r.binding.clone()).collect()
    }

    /// Every binding, for the UI's list. [`Store::assign`] takes an index into this list.
    pub fn listed(&self) -> Vec<Listed> {
        self.records
            .iter()
            .map(|r| Listed {
                binding: r.binding.clone(),
                origin: r.origin,
                ordinal: r.ordinal,
                blocked: r.blocked.clone(),
                display_name: if r.binding.port_name.is_empty() { &r.binding.port_id } else { &r.binding.port_name }
                    .clone(),
            })
            .collect()
    }

    /// The store as it is now, to write once the caller's lock is released.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            revision: self.revision,
            records: Arc::clone(&self.records),
            rejected: Arc::clone(&self.rejected),
            legacy: Legacy { imported: self.imported },
            slot: Arc::clone(&self.slot),
        }
    }

    /// The learn model's list after an edit of its own (a learn, a pedal read as momentary). `live` names
    /// the records the learn model was handed ([`Store::listed`] indices); `list` is its list now. A
    /// binding on the control of one of them takes that record's place, keeping its origin; one on
    /// another control is added at the end, native. A live record whose control `list` no longer binds is
    /// gone. Every other record (blocked, ordinal, on a port no present port answers to) stays as it is,
    /// but for an active one on a real port id whose control `list` now binds: one action per message.
    pub fn replace(&mut self, live: &[usize], list: Vec<Binding>) {
        let list: Vec<Binding> = list.into_iter().map(Binding::normalized).collect();
        let mut placed = vec![false; list.len()];
        let mut next: Vec<Record> = Vec::with_capacity(self.records.len() + list.len());
        for (i, r) in self.records.iter().enumerate() {
            if live.contains(&i) {
                let on = (0..list.len()).find(|&k| !placed[k] && control(&list[k]) == control(&r.binding));
                if let Some(k) = on {
                    placed[k] = true;
                    next.push(Record { binding: list[k].clone(), ..r.clone() });
                }
                continue;
            }
            if !(r.active() && !r.ordinal && list.iter().any(|b| control(b) == control(&r.binding))) {
                next.push(r.clone());
            }
        }
        let added = list.into_iter().zip(placed).filter(|(_, placed)| !placed);
        next.extend(added.map(|(binding, _)| Record { origin: Origin::Native, ordinal: false, blocked: None, binding }));
        if next != *self.records {
            self.records = Arc::new(next);
            self.bump();
        }
    }

    /// The player drops listed binding `index`.
    pub fn forget(&mut self, index: usize) -> Result<(), String> {
        if index >= self.records.len() {
            return Err(format!("no binding {index}"));
        }
        Arc::make_mut(&mut self.records).remove(index);
        self.bump();
        Ok(())
    }

    /// The player edits listed binding `index` (its kind, its HOLD): `binding`, on the same control,
    /// takes its place with its invariants kept; its origin, ordinal and blocked state stay.
    pub fn edit(&mut self, index: usize, binding: Binding) -> Result<(), String> {
        let record = self.records.get(index).ok_or_else(|| format!("no binding {index}"))?;
        if control(&record.binding) != control(&binding) || record.binding.port_name != binding.port_name {
            return Err(format!("binding {index} listens to another message"));
        }
        let binding = binding.normalized();
        if record.binding != binding {
            Arc::make_mut(&mut self.records)[index].binding = binding;
            self.bump();
        }
        Ok(())
    }

    /// The player assigns listed binding `index` to a present port: its id and name become that port's,
    /// it is no longer blocked, its origin stays. Refused when another active binding already has that
    /// control (one action per message), or there is no such binding.
    pub fn assign(&mut self, index: usize, port_id: &str, port_name: &str) -> Result<(), String> {
        let record = self.records.get(index).ok_or_else(|| format!("no binding {index}"))?;
        let binding = Binding { port_id: port_id.to_owned(), port_name: port_name.to_owned(), ..record.binding.clone() };
        if self.clash(index, &binding) {
            return Err(format!("another binding on {port_name} already uses this message"));
        }
        let record = &mut Arc::make_mut(&mut self.records)[index];
        record.binding = binding;
        record.ordinal = false;
        record.blocked = None;
        self.bump();
        Ok(())
    }

    /// Another active record than `index` listens to `b`, a binding on a real port id.
    fn clash(&self, index: usize, b: &Binding) -> bool {
        self.records.iter().enumerate().any(|(i, r)| i != index && r.active() && r.on_control(b, false))
    }

    /// Resolution moved the port `old_id` named `old_name` to a present port: persist the move of its
    /// active records. Records on the same id under another name are another port's (an ordinal id) and
    /// stay. A record whose message the new port already has a binding for would shadow it or be shadowed:
    /// it stays where it was, blocked, for the player to assign.
    pub fn reanchor(&mut self, old_id: &str, old_name: &str, new_id: &str, new_name: &str) -> Reanchored {
        let mut out = Reanchored::default();
        let on_old = |r: &Record| r.active() && r.binding.port_id == old_id && r.binding.port_name == old_name;
        let from: Vec<usize> = (0..self.records.len()).filter(|&i| on_old(&self.records[i])).collect();
        if from.is_empty() || (old_id == new_id && old_name == new_name) {
            return out;
        }
        for i in from {
            let moved = Binding { port_id: new_id.to_owned(), port_name: new_name.to_owned(), ..self.records[i].binding.clone() };
            let clash = self.clash(i, &moved);
            let record = &mut Arc::make_mut(&mut self.records)[i];
            if clash {
                record.blocked = Some(format!("{new_name} already has a binding on this message"));
                out.blocked.push(i);
            } else {
                record.binding = moved;
                record.ordinal = false;
                out.moved.push(i);
            }
        }
        self.bump();
        out
    }

    /// Import the web's list (`lf.midiLearn` verbatim; `"[]"` when the key is absent), once. Each record
    /// keeps its legacy port id, marked ordinal; the collision rule blocks what a renumbered run may have
    /// relearned. A second call changes nothing and reports `already`, whether the first has been written
    /// yet or not.
    pub fn import_legacy(&mut self, json: &str) -> ImportReport {
        let mut report = ImportReport::default();
        if self.imported {
            report.already = true;
            return report;
        }
        // `parse_bindings`, record by record: the report and `rejected` need each record's index and raw
        // value, which its result drops.
        match serde_json::from_str::<Vec<Value>>(json) {
            Err(e) => {
                report.unreadable = Some(e.to_string());
                Arc::make_mut(&mut self.rejected).push(set_aside("legacy", &Value::String(json.to_owned())));
            }
            Ok(values) => self.import_records(values, &mut report),
        }
        self.imported = true;
        self.bump();
        self.import_revision = Some(self.revision);
        report
    }

    fn import_records(&mut self, values: Vec<Value>, report: &mut ImportReport) {
        let mut read = Vec::new();
        for (index, value) in values.into_iter().enumerate() {
            match parse_binding(value.clone()) {
                Ok(b) => read.push((index, b)),
                Err(reason) => {
                    report.rejected.push(Rejected { index, reason });
                    Arc::make_mut(&mut self.rejected).push(set_aside("legacy", &value));
                }
            }
        }
        // Decision 9: the legacy ids each (port name, message) was learned under.
        let mut ids: HashMap<(&str, u8, Kind, u8), BTreeSet<&str>> = HashMap::new();
        for (_, b) in &read {
            ids.entry((&b.port_name, b.channel, b.kind, b.number)).or_default().insert(&b.port_id);
        }
        let mut added: Vec<Record> = Vec::new();
        for (index, b) in &read {
            if self.records.iter().chain(&added).any(|r| r.on_control(b, true)) {
                report.skipped.push(*index);
                continue;
            }
            let under = &ids[&(b.port_name.as_str(), b.channel, b.kind, b.number)];
            let blocked = (under.len() > 1).then(|| {
                let list = under.iter().copied().collect::<Vec<_>>().join(", ");
                format!("{:?} bound this message under several port ids ({list}); a renumbered run may have relearned it", b.port_name)
            });
            match &blocked {
                Some(why) => report.blocked.push(Blocked { index: *index, why: why.clone() }),
                None => report.imported.push(*index),
            }
            added.push(Record { origin: Origin::Legacy, ordinal: true, blocked, binding: b.clone() });
        }
        Arc::make_mut(&mut self.records).extend(added);
    }
}

/// The store at one revision, to write off the caller's lock.
pub struct Snapshot {
    revision: u64,
    records: Arc<Vec<Record>>,
    rejected: Arc<Vec<Value>>,
    legacy: Legacy,
    slot: Arc<Slot>,
}

impl Snapshot {
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Write this revision to `dir`, unless a snapshot as new is already written. The file is read again
    /// first: refused (the store becomes read-only) when this build cannot read it or it is of another
    /// version, a conflict when another instance wrote it since. The slot's lock is held through the
    /// write, so writes of one store never interleave; the caller's own lock need not be.
    pub fn write(&self, dir: &Path) -> Result<Written, WriteError> {
        let file = dir.join(FILE_NAME);
        if let Some(why) = self.slot.read_only.get() {
            return Err(WriteError::ReadOnly(why.clone()));
        }
        let mut disk = self.slot.disk.lock().unwrap_or_else(PoisonError::into_inner);
        if self.revision <= self.slot.written.load(Ordering::Acquire) {
            return Ok(Written::Skipped);
        }
        match std::fs::read(&file) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(self.slot.refuse(format!("{}: {e}", file.display()))),
            Ok(bytes) => match decode(&file, &bytes) {
                Err(refused) => return Err(self.slot.refuse(refusal(&file, refused))),
                Ok(found) if found.revision != *disk => {
                    return Err(WriteError::Conflict(format!(
                        "{}: revision {} on disk, this store last saw {}",
                        file.display(),
                        found.revision,
                        *disk
                    )))
                }
                Ok(_) => {}
            },
        }
        let doc = Document {
            version: VERSION,
            revision: *disk + 1,
            bindings: &self.records,
            rejected: &self.rejected,
            legacy: self.legacy,
        };
        let json = serde_json::to_vec_pretty(&doc).map_err(|e| WriteError::Failed(format!("serialize: {e}")))?;
        // The next load must read what this write leaves, every record included.
        match decode(&file, &json) {
            Ok(back) if back.rejected.is_empty() && back.records.len() == self.records.len() => {}
            Ok(_) => return Err(WriteError::Failed("the document would not read back whole".into())),
            Err(refused) => return Err(WriteError::Failed(format!("the document would not read back: {refused:?}"))),
        }
        std::fs::create_dir_all(dir).map_err(|e| WriteError::Failed(format!("{}: {e}", dir.display())))?;
        write_atomic(&file, &json).map_err(WriteError::Failed)?;
        *disk = doc.revision;
        self.slot.written.store(self.revision, Ordering::Release);
        Ok(Written::Wrote)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering::Relaxed;

    /// A scratch dir for one `midi-bindings.json`, removed on drop (`folders.rs`'s).
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "lf-midi-store-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn file(&self) -> PathBuf {
            self.0.join(FILE_NAME)
        }
        fn json(&self) -> Value {
            serde_json::from_slice(&std::fs::read(self.file()).unwrap()).unwrap()
        }
        /// Every file in the dir (a temp file a write left behind shows up here).
        fn files(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(&self.0)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn binding(port_id: &str, port_name: &str, number: u8) -> Binding {
        parse_binding(serde_json::json!({
            "portId": port_id, "portName": port_name, "channel": 0, "kind": "cc", "number": number,
            "action": "undo", "pressHigh": true, "momentary": true,
        }))
        .unwrap()
    }

    /// A web record (`port`, as `midi-actions.ts` saves it).
    fn legacy(port: &str, port_name: &str, number: u8) -> Value {
        serde_json::json!({
            "port": port, "portName": port_name, "channel": 0, "kind": "cc", "number": number,
            "action": "recDub", "pressHigh": true, "momentary": true, "target": null, "hold": false,
        })
    }

    fn legacy_list(records: &[Value]) -> String {
        serde_json::to_string(records).unwrap()
    }

    impl Store {
        /// `list` as the learn model's whole list: every active record was handed to it.
        fn set(&mut self, list: Vec<Binding>) {
            let live: Vec<usize> = (0..self.records.len()).filter(|&i| self.records[i].active()).collect();
            self.replace(&live, list);
        }
    }

    fn blocked(store: &Store) -> Vec<bool> {
        store.listed().iter().map(|l| l.blocked.is_some()).collect()
    }

    fn ports(store: &Store) -> Vec<(String, String)> {
        store.listed().into_iter().map(|l| (l.binding.port_id, l.binding.port_name)).collect()
    }

    /// `depth` arrays, one inside the other.
    fn nested(depth: usize) -> Value {
        (0..depth).fold(Value::Null, |inner, _| Value::Array(vec![inner]))
    }

    #[test]
    fn a_missing_file_is_an_empty_writable_store_and_its_first_write_creates_the_document() {
        let scratch = Scratch::new("missing");
        let (mut store, result) = load(&scratch.0);
        assert_eq!(result, LoadResult::Missing);
        assert!(store.writable());
        assert_eq!(store.bindings(), []);
        assert_eq!(store.snapshot().write(&scratch.0), Ok(Written::Skipped), "nothing changed yet");
        assert_eq!(scratch.files(), Vec::<String>::new());

        let pedal = binding("native-1", "Pedal", 20);
        store.set(vec![pedal.clone()]);
        assert_eq!(store.snapshot().write(&scratch.0), Ok(Written::Wrote));
        assert_eq!(
            scratch.json(),
            serde_json::json!({
                "version": 1,
                "revision": 1,
                "bindings": [{
                    "origin": "native", "ordinal": false, "blocked": null,
                    "binding": serde_json::to_value(&pedal).unwrap(),
                }],
                "rejected": [],
                "legacy": { "imported": false },
            })
        );
        assert_eq!(scratch.files(), vec![FILE_NAME], "no temp file is left");
        let (again, result) = load(&scratch.0);
        assert_eq!(result, LoadResult::Loaded { rejected: vec![] });
        assert_eq!(again.bindings(), [pedal]);
    }

    #[test]
    fn an_invalid_or_newer_file_is_reported_and_never_overwritten() {
        let scratch = Scratch::new("refused");
        let newer = serde_json::to_vec(&serde_json::json!({ "version": 2, "bindings": [], "later": true })).unwrap();
        let cases: [(&[u8], fn(&LoadResult) -> bool); 5] = [
            (b"{ not json", |r| matches!(r, LoadResult::Invalid(_))),
            (b"[]", |r| matches!(r, LoadResult::Invalid(_))),
            (br#"{"version":"1","revision":1,"bindings":[],"rejected":[],"legacy":{"imported":false}}"#, |r| {
                matches!(r, LoadResult::Invalid(_))
            }),
            (br#"{"version":1,"revision":1,"bindings":{},"rejected":[],"legacy":{"imported":false}}"#, |r| {
                matches!(r, LoadResult::Invalid(_))
            }),
            (&newer, |r| *r == LoadResult::UnsupportedVersion(2)),
        ];
        for (bytes, expected) in cases {
            std::fs::write(scratch.file(), bytes).unwrap();
            let (mut store, result) = load(&scratch.0);
            assert!(expected(&result), "{result:?}");
            assert!(!store.writable());
            // The session still works in memory: an import and an edit.
            store.import_legacy(&legacy_list(&[legacy("input-1", "Pedal", 20)]));
            store.set(vec![binding("native-1", "Pedal", 21)]);
            assert_eq!(store.bindings().len(), 1);
            let refused = store.snapshot().write(&scratch.0);
            assert!(matches!(refused, Err(WriteError::ReadOnly(_))), "{refused:?}");
            assert!(!store.legacy_durable(), "a refused write never makes the import durable");
            assert_eq!(std::fs::read(scratch.file()).unwrap(), bytes, "the file is left as it was");
            assert_eq!(scratch.files(), vec![FILE_NAME]);
        }
    }

    #[test]
    fn a_record_this_build_cannot_read_is_reported_and_kept_as_text_across_a_write() {
        let scratch = Scratch::new("malformed");
        let good = serde_json::json!({
            "origin": "native", "ordinal": false, "blocked": null,
            "binding": serde_json::to_value(binding("native-1", "Pedal", 20)).unwrap(),
        });
        let mut bad = good.clone();
        bad["binding"]["action"] = "selfDestruct".into();
        let older = serde_json::json!({ "from": "store", "record": "{\"anything\":[1,2]}" });
        let doc = serde_json::json!({
            "version": 1, "revision": 4, "bindings": [good, bad.clone()], "rejected": [older.clone()],
            "legacy": { "imported": true },
        });
        std::fs::write(scratch.file(), serde_json::to_vec(&doc).unwrap()).unwrap();
        let (mut store, result) = load(&scratch.0);
        let LoadResult::Loaded { rejected } = result else { panic!("{result:?}") };
        assert_eq!(rejected.iter().map(|r| r.index).collect::<Vec<_>>(), [1]);
        assert!(rejected[0].reason.contains("selfDestruct"), "{}", rejected[0].reason);
        assert_eq!(store.bindings().len(), 1);

        store.set(vec![binding("native-2", "Keys", 64)]);
        assert_eq!(store.snapshot().write(&scratch.0), Ok(Written::Wrote));
        let written = scratch.json();
        assert_eq!(written["revision"], 5);
        assert_eq!(written["rejected"], serde_json::json!([older, { "from": "store", "record": bad.to_string() }]));
        let text = written["rejected"][1]["record"].as_str().unwrap();
        assert_eq!(serde_json::from_str::<Value>(text).unwrap(), bad, "the text is the record");
        assert_eq!(written["bindings"].as_array().unwrap().len(), 1);
        assert_eq!(written["bindings"][0]["binding"]["portName"], "Keys");
        // Read again, the set-aside record stays set aside: nothing is lost on a second round either.
        let (mut store, _) = load(&scratch.0);
        store.set(vec![]);
        store.snapshot().write(&scratch.0).unwrap();
        assert_eq!(scratch.json()["rejected"], written["rejected"]);
    }

    // A record nested as deep as the parser allows, set aside: kept as text, the file still reads.
    #[test]
    fn a_deeply_nested_rejected_record_never_makes_the_file_unreadable() {
        let scratch = Scratch::new("deep");
        let (mut store, _) = load(&scratch.0);
        let mut deep = legacy("input-1", "Pedal", 20);
        deep["action"] = nested(125);
        let json = legacy_list(&[deep.clone(), legacy("input-1", "Pedal", 21)]);
        let report = store.import_legacy(&json);
        assert_eq!((report.rejected.len(), report.imported.len()), (1, 1));
        assert_eq!(store.snapshot().write(&scratch.0), Ok(Written::Wrote));
        let (store, result) = load(&scratch.0);
        assert_eq!(result, LoadResult::Loaded { rejected: vec![] });
        assert!(store.writable());
        assert_eq!(*store.rejected, [serde_json::json!({ "from": "legacy", "record": deep.to_string() })]);

        // A document that would not read back is never written: the file stays as it was.
        let (mut store, _) = load(&scratch.0);
        let before = std::fs::read(scratch.file()).unwrap();
        Arc::make_mut(&mut store.rejected).push(nested(130));
        store.set(vec![]);
        let refused = store.snapshot().write(&scratch.0);
        assert!(matches!(refused, Err(WriteError::Failed(_))), "{refused:?}");
        assert_eq!(std::fs::read(scratch.file()).unwrap(), before);
        assert_eq!(scratch.files(), vec![FILE_NAME]);
    }

    #[test]
    fn the_import_runs_once_keeps_legacy_ids_and_sets_aside_what_it_cannot_read() {
        let scratch = Scratch::new("import");
        let (mut store, _) = load(&scratch.0);
        let mut unknown = legacy("input-2", "Keys", 1);
        unknown["action"] = "selfDestruct".into();
        let json = legacy_list(&[legacy("input-1", "Pedal", 20), unknown.clone(), legacy("input-1", "Pedal", 20)]);
        let report = store.import_legacy(&json);
        assert_eq!(report.imported, [0]);
        assert_eq!(report.rejected.iter().map(|r| r.index).collect::<Vec<_>>(), [1]);
        assert_eq!(report.skipped, [2], "a second record on the same control is not duplicated");
        assert_eq!((report.already, report.blocked.len()), (false, 0));
        let listed = store.listed();
        assert_eq!(listed.len(), 1);
        assert_eq!((listed[0].binding.port_id.as_str(), listed[0].origin), ("input-1", Origin::Legacy));
        assert_eq!(listed[0].display_name, "Pedal");

        let revision = store.snapshot().revision();
        let again = store.import_legacy(&json);
        assert_eq!(again, ImportReport { already: true, ..ImportReport::default() });
        assert_eq!(store.snapshot().revision(), revision, "a second call changes nothing");

        store.snapshot().write(&scratch.0).unwrap();
        assert_eq!(scratch.json()["rejected"], serde_json::json!([{ "from": "legacy", "record": unknown.to_string() }]));
        let (mut reloaded, _) = load(&scratch.0);
        assert!(reloaded.legacy_durable(), "a file that says imported is the durable write");
        assert!(reloaded.import_legacy(&json).already);
        assert_eq!(reloaded.listed(), listed);

        // A legacy document that is not a list is kept whole, and the import is done.
        let (mut fresh, _) = load(&Scratch::new("import-garbage").0);
        let report = fresh.import_legacy("{oops");
        assert!(report.unreadable.is_some());
        assert_eq!(fresh.bindings(), []);
        assert_eq!(*fresh.rejected, [serde_json::json!({ "from": "legacy", "record": "{oops" })]);
        assert!(fresh.import_legacy("[]").already);
    }

    // An ordinal id is no port's identity: one `input-1` under two names is two controllers.
    #[test]
    fn the_import_tells_two_ports_with_one_ordinal_id_apart_by_name() {
        let (mut store, _) = load(&Scratch::new("ordinal").0);
        let report = store.import_legacy(&legacy_list(&[
            legacy("input-1", "Pedal", 20),
            legacy("input-1", "Keys", 20),
            legacy("input-1", "Keys", 20),
        ]));
        assert_eq!((report.imported, report.skipped), (vec![0, 1], vec![2]));
        assert_eq!(ports(&store), [("input-1".into(), "Pedal".into()), ("input-1".into(), "Keys".into())]);
    }

    // Decision 9: one name, several legacy ids, one message: a renumbered run may have relearned it.
    #[test]
    fn the_collision_rule_blocks_one_name_from_several_ids_on_one_message_only() {
        let import = |records: &[Value]| {
            let (mut store, _) = load(&Scratch::new("collision").0);
            let report = store.import_legacy(&legacy_list(records));
            (store, report)
        };
        let (store, report) = import(&[legacy("input-1", "Pedal", 20), legacy("input-3", "Pedal", 20)]);
        assert_eq!(blocked(&store), [true, true]);
        assert_eq!(report.blocked.iter().map(|b| b.index).collect::<Vec<_>>(), [0, 1]);
        assert!(report.blocked[0].why.contains("input-1, input-3"), "{}", report.blocked[0].why);
        assert_eq!(store.listed()[0].blocked.as_ref(), Some(&report.blocked[0].why), "the list says why");
        assert_eq!(store.bindings(), [], "a blocked record never reaches the learn model");

        let (store, report) = import(&[legacy("input-1", "Pedal", 20), legacy("input-3", "Pedal", 21)]);
        assert_eq!((blocked(&store), report.imported), (vec![false, false], vec![0, 1]), "different messages");

        let (store, _) = import(&[legacy("input-1", "Pedal", 20), legacy("input-3", "Keys", 20)]);
        assert_eq!(blocked(&store), [false, false], "different names");
    }

    #[test]
    fn the_import_is_durable_only_once_a_write_holding_it_has_completed() {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 1;
        let scratch = Scratch::new("durable");
        let (mut store, _) = load(&scratch.0);
        store.set(vec![binding("native-1", "Pedal", 1)]);
        let before_import = store.snapshot();
        store.import_legacy(&legacy_list(&[legacy("input-1", "Pedal", 20)]));
        assert!(!store.legacy_durable());
        before_import.write(&scratch.0).unwrap();
        assert!(!store.legacy_durable(), "that write did not hold the import");

        // The file held open without delete sharing: the rename over it fails, a write's last step.
        let held = std::fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(scratch.file()).unwrap();
        assert!(matches!(store.snapshot().write(&scratch.0), Err(WriteError::Failed(_))));
        assert!(!store.legacy_durable());
        assert_eq!(scratch.files(), vec![FILE_NAME], "the temp file is removed");
        drop(held);
        assert_eq!(store.snapshot().write(&scratch.0), Ok(Written::Wrote));
        assert!(store.legacy_durable());
        assert_eq!(scratch.json()["legacy"], serde_json::json!({ "imported": true }));
    }

    #[test]
    fn snapshots_written_out_of_order_leave_the_newer_on_disk() {
        let scratch = Scratch::new("order");
        let (mut store, _) = load(&scratch.0);
        store.set(vec![binding("native-1", "Old", 1)]);
        let older = store.snapshot();
        store.set(vec![binding("native-1", "New", 2)]);
        let newer = store.snapshot();
        assert!(older.revision() < newer.revision());
        assert_eq!(newer.write(&scratch.0), Ok(Written::Wrote));
        assert_eq!(older.write(&scratch.0), Ok(Written::Skipped));
        assert_eq!(newer.write(&scratch.0), Ok(Written::Skipped), "already on disk");
        assert_eq!(scratch.json()["bindings"][0]["binding"]["portName"], "New");

        // Two threads, each with its own snapshot, in either order: the newer always wins.
        for flip in [false, true] {
            store.set(vec![binding("native-1", "A", 3 + u8::from(flip))]);
            let a = store.snapshot();
            store.set(vec![binding("native-1", "B", 5 + u8::from(flip))]);
            let b = store.snapshot();
            let (first, second) = if flip { (a, b) } else { (b, a) };
            let dir = scratch.0.clone();
            std::thread::spawn(move || first.write(&dir).unwrap()).join().unwrap();
            second.write(&scratch.0).unwrap();
            assert_eq!(scratch.json()["bindings"][0]["binding"]["portName"], "B");
            assert_eq!(scratch.files(), vec![FILE_NAME]);
        }
    }

    // Two instances of the app, or an older build beside a newer one: a write never replaces a file it
    // did not load or write last.
    #[test]
    fn a_write_refuses_a_file_another_instance_or_build_wrote_since() {
        let scratch = Scratch::new("instances");
        let (mut a, _) = load(&scratch.0);
        let (mut b, _) = load(&scratch.0);
        a.set(vec![binding("native-1", "A", 1)]);
        assert_eq!(a.snapshot().write(&scratch.0), Ok(Written::Wrote));
        b.set(vec![binding("native-1", "B", 1)]);
        assert!(matches!(b.snapshot().write(&scratch.0), Err(WriteError::Conflict(_))), "B never saw A's file");
        let (mut b, _) = load(&scratch.0);
        a.set(vec![binding("native-1", "A", 2)]);
        assert_eq!(a.snapshot().write(&scratch.0), Ok(Written::Wrote));
        b.set(vec![binding("native-1", "B", 2)]);
        assert!(matches!(b.snapshot().write(&scratch.0), Err(WriteError::Conflict(_))), "B loaded an older revision");
        assert_eq!(scratch.json()["bindings"][0]["binding"]["portName"], "A");
        assert!(b.writable(), "a conflict is no reason to stop writing after a reload");

        // A newer build, or garbage, written over the file since: refused, and read-only from then on.
        let newer = serde_json::to_vec(&serde_json::json!({ "version": 2, "later": true })).unwrap();
        for bytes in [newer, b"{ torn".to_vec()] {
            let (mut c, _) = load(&scratch.0);
            std::fs::write(scratch.file(), &bytes).unwrap();
            c.set(vec![binding("native-1", "C", 3)]);
            assert!(matches!(c.snapshot().write(&scratch.0), Err(WriteError::ReadOnly(_))));
            assert!(!c.writable());
            assert_eq!(std::fs::read(scratch.file()).unwrap(), bytes, "the file is left as it was");
            // Put back a file this build reads, for the next case.
            std::fs::remove_file(scratch.file()).unwrap();
            let (mut d, _) = load(&scratch.0);
            d.set(vec![binding("native-1", "D", 4)]);
            d.snapshot().write(&scratch.0).unwrap();
        }
        assert_eq!(scratch.files(), vec![FILE_NAME]);
    }

    #[test]
    fn replace_keeps_each_controls_origin_and_place_and_the_blocked_records() {
        let (mut store, _) = load(&Scratch::new("replace").0);
        store.import_legacy(&legacy_list(&[
            legacy("input-1", "Pedal", 20),
            legacy("input-2", "Pedal", 30),
            legacy("input-4", "Pedal", 30),
        ]));
        let mut list = store.bindings();
        assert_eq!(list.len(), 1);
        list[0].momentary = false;
        list.push(binding("native-1", "Keys", 64));
        store.set(list);
        let listed: Vec<(String, Origin, bool)> =
            store.listed().into_iter().map(|l| (l.binding.port_id, l.origin, l.blocked.is_some())).collect();
        assert_eq!(
            listed,
            [
                ("input-1".into(), Origin::Legacy, false),
                ("input-2".into(), Origin::Legacy, true),
                ("input-4".into(), Origin::Legacy, true),
                ("native-1".into(), Origin::Native, false),
            ],
            "an edited record keeps its place; a new one goes last"
        );
        assert!(store.records[0].ordinal, "the edited legacy record is still on its ordinal id");
        let revision = store.snapshot().revision();
        store.set(store.bindings());
        assert_eq!(store.snapshot().revision(), revision, "an unchanged list is no change");
    }

    #[test]
    fn assign_unblocks_onto_a_present_port() {
        let (mut store, _) = load(&Scratch::new("assign").0);
        store.import_legacy(&legacy_list(&[
            legacy("input-1", "Pedal", 20),
            legacy("input-3", "Pedal", 20),
            legacy("input-1", "Pedal", 21),
        ]));
        assert_eq!(blocked(&store), [true, true, false]);

        store.assign(0, "dev-A#0", "Pedal (USB)").unwrap();
        let first = &store.listed()[0];
        assert_eq!((first.blocked.is_some(), first.origin), (false, Origin::Legacy), "the origin stays");
        assert_eq!((first.binding.port_id.as_str(), first.display_name.as_str()), ("dev-A#0", "Pedal (USB)"));
        assert!(!store.records[0].ordinal);
        assert_eq!(store.bindings().len(), 2);
        assert!(store.assign(1, "dev-A#0", "Pedal (USB)").is_err(), "that control is taken");
        assert_eq!(blocked(&store), [false, true, false], "a refused assignment changes nothing");
        assert!(store.assign(9, "dev-A#0", "Pedal (USB)").is_err());
    }

    // Resolution moves one port's records: by id AND name, since an ordinal id is shared across ports.
    #[test]
    fn reanchor_moves_only_the_named_ports_active_records() {
        let (mut store, _) = load(&Scratch::new("reanchor").0);
        store.import_legacy(&legacy_list(&[
            legacy("input-1", "Pedal", 20),
            legacy("input-1", "Keys", 21),
            legacy("input-3", "Pedal", 22),
            legacy("input-5", "Pedal", 22),
        ]));
        assert_eq!(blocked(&store), [false, false, true, true]);
        let revision = store.snapshot().revision();
        assert_eq!(store.reanchor("input-1", "Pedal", "dev-B#0", "Pedal").moved, [0]);
        assert_eq!(store.snapshot().revision(), revision + 1);
        assert_eq!(store.reanchor("input-3", "Pedal", "dev-B#0", "Pedal"), Reanchored::default(), "blocked records stay");
        assert_eq!(store.reanchor("gone", "Pedal", "dev-B#0", "Pedal"), Reanchored::default());
        assert_eq!(store.snapshot().revision(), revision + 1, "only a move is a change");
        assert_eq!(
            ports(&store),
            [
                ("dev-B#0".into(), "Pedal".into()),
                ("input-1".into(), "Keys".into()),
                ("input-3".into(), "Pedal".into()),
                ("input-5".into(), "Pedal".into()),
            ]
        );
        assert!(!store.records[0].ordinal && store.records[1].ordinal);
    }

    // A move onto a port that already binds the message would leave one of the two silently shadowed.
    #[test]
    fn reanchor_blocks_a_record_whose_message_the_new_port_already_binds() {
        let (mut store, _) = load(&Scratch::new("reanchor-clash").0);
        store.set(vec![binding("dev-A#0", "Pedal", 20)]);
        store.import_legacy(&legacy_list(&[legacy("input-1", "Pedal", 20), legacy("input-1", "Pedal", 21)]));
        let out = store.reanchor("input-1", "Pedal", "dev-A#0", "Pedal");
        assert_eq!((out.moved, out.blocked), (vec![2], vec![1]));
        assert_eq!(ports(&store)[1], ("input-1".into(), "Pedal".into()), "the blocked record stays where it was");
        assert!(store.listed()[1].blocked.as_ref().unwrap().contains("already has a binding"));
        let on_20 = store.bindings().iter().filter(|b| b.port_id == "dev-A#0" && b.number == 20).count();
        assert_eq!(on_20, 1, "one active binding per control");
    }

    // The learn model runs only the records the port resolution hands it: an edit of its own leaves the
    // others as they are (an ordinal record, one on a port no present port answers to), but for an
    // active one on a real id that its list now binds the message of (one action per message).
    #[test]
    fn replace_leaves_the_records_the_learn_model_does_not_run() {
        let (mut store, _) = load(&Scratch::new("replace-live").0);
        store.import_legacy(&legacy_list(&[legacy("input-1", "Pedal", 20)]));
        store.set(vec![binding("input-1", "Pedal", 20), binding("dev-A#0", "Keys", 1), binding("dev-B#0", "Gone", 2)]);
        assert_eq!(store.records.len(), 3);
        // Handed to the learn model: Keys alone (Pedal is ordinal, Gone's port is absent).
        let mut list = vec![Binding { momentary: false, ..binding("dev-A#0", "Keys", 1) }];
        list.push(binding("dev-B#0", "Pedal", 9));
        store.replace(&[1], list.clone());
        let got: Vec<(String, u8, bool)> = store.listed().into_iter().map(|l| (l.binding.port_id, l.binding.number, l.binding.momentary)).collect();
        assert_eq!(
            got,
            [("input-1".into(), 20, true), ("dev-A#0".into(), 1, false), ("dev-B#0".into(), 2, true), ("dev-B#0".into(), 9, true)],
            "the others stay; Keys is edited in place; the learn is added"
        );
        assert!(store.listed()[0].ordinal);
        // A learn on the message an unrun record on a real id binds takes it over; an ordinal one stays.
        store.replace(&[1, 3], [list, vec![binding("dev-B#0", "Gone", 2), binding("input-1", "Pedal", 20)]].concat());
        let got: Vec<(String, u8)> = store.listed().into_iter().map(|l| (l.binding.port_id, l.binding.number)).collect();
        assert_eq!(
            got,
            [("input-1".into(), 20), ("dev-A#0".into(), 1), ("dev-B#0".into(), 9), ("dev-B#0".into(), 2), ("input-1".into(), 20)]
        );
        // A live record its list dropped is forgotten.
        let revision = store.snapshot().revision();
        store.replace(&[1, 2, 3, 4], vec![binding("dev-B#0", "Gone", 2)]);
        assert_eq!(store.listed().iter().map(|l| l.binding.number).collect::<Vec<_>>(), [20, 2]);
        assert!(store.snapshot().revision() > revision);
    }

    #[test]
    fn forget_and_edit_act_on_any_listed_record() {
        let (mut store, _) = load(&Scratch::new("edit").0);
        store.import_legacy(&legacy_list(&[legacy("input-1", "Pedal", 20), legacy("input-3", "Pedal", 20), legacy("input-1", "Pedal", 21)]));
        assert_eq!(blocked(&store), [true, true, false]);
        let b = store.listed()[1].binding.clone();
        store.edit(1, Binding { momentary: false, hold: true, ..b.clone() }).unwrap();
        let edited = &store.listed()[1];
        assert_eq!((edited.binding.momentary, edited.binding.hold, edited.blocked.is_some(), edited.ordinal), (false, false, true, true), "a latching pedal has no HOLD; it stays blocked");
        assert!(store.edit(1, Binding { number: 30, ..b.clone() }).is_err(), "another message");
        assert!(store.edit(1, Binding { port_name: "Keys".into(), ..b }).is_err(), "another port");
        let revision = store.snapshot().revision();
        store.forget(0).unwrap();
        assert_eq!(blocked(&store), [true, false]);
        assert!(store.snapshot().revision() > revision);
        assert!(store.forget(5).is_err());
        assert!(empty().writable());
    }
}
