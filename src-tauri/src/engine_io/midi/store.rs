//! OWNS: the player's MIDI-learn bindings on disk, `midi-bindings.json` beside `plugin-folders.json`, and
//! the one-time import of the WebView's `localStorage` list (`lf.midiLearn`, `src/app/midi-actions.ts`):
//! the plan's decisions 8 and 9. Dormant: nothing builds a [`Store`] yet.
//!
//! This file is USER DATA, as the folder list is (`host/folders.rs`): only a MISSING file is an empty
//! store. One that cannot be read or parsed, or that a newer build wrote, is reported to the caller
//! ([`LoadResult`]) and leaves the store read-only for the session: what an import or an edit brings works
//! in memory, and no write ever replaces that file. A stored record this build cannot read is reported by
//! its index and kept verbatim in the document's `rejected` list, as is a legacy record the import cannot
//! read, so a later write never loses either.
//!
//! Writes happen off the router's lock. Under it the caller takes a [`Snapshot`] (two `Arc` clones and the
//! store's revision); after releasing it, [`Snapshot::write`]. Every snapshot of one store shares one write
//! slot that serializes the writes and remembers the revision on disk, so a snapshot older than the one
//! written is skipped: two writers racing never leave an older list over a newer one. A write is
//! `host::tone::write_atomic` (temp file, `sync_all`, rename), the folder list's.
//!
//! # The legacy import
//!
//! A web record's port id (`input-<N>`) is a per-run ordinal (the plan's § Step 0 findings), so a legacy
//! record keeps it as its `port_id` but its port NAME is its identity: whether it activates is decided at
//! resolution by the name rule, elsewhere. The import itself only refuses to guess where a renumbered run
//! may have relearned a control: records of one port name, from several legacy ids, that bind the same
//! message are all `blocked` until the player assigns them ([`Store::assign`]). A blocked record is listed
//! but never reaches the learn model. The import runs once: the document's `legacy.imported` says it ran,
//! and [`Store::legacy_durable`] says a write holding it has completed, the UI's cue to drop
//! `lf.midiLearn` (one release later). A file that says `imported` is itself that write, so the flag is
//! not stored twice.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

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

/// One stored binding. `blocked`: it waits for the player to assign it a port (the collision rule).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    origin: Origin,
    blocked: bool,
    binding: Binding,
}

/// A record in the document's `bindings` list, read with the binding's own checks (`parse_binding`).
fn parse_record(value: Value) -> Result<Record, String> {
    #[derive(Deserialize)]
    struct Raw {
        origin: Origin,
        blocked: bool,
        binding: Value,
    }
    let raw: Raw = serde_json::from_value(value).map_err(|e| e.to_string())?;
    Ok(Record { origin: raw.origin, blocked: raw.blocked, binding: parse_binding(raw.binding)? })
}

/// The message a binding listens to on its port: one binding per control, as the learn model keeps.
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
    bindings: &'a [Record],
    /// Records this build could not read, verbatim, each as `{"from": "store" | "legacy", "record": …}`
    /// when this build set it aside; entries already in the file are kept as they are.
    rejected: &'a [Value],
    legacy: Legacy,
}

/// The file, as read once its version is known to be this build's.
#[derive(Deserialize)]
struct OnDisk {
    bindings: Vec<Value>,
    rejected: Vec<Value>,
    legacy: Legacy,
}

/// A record set aside, tagged with the list it came from.
fn set_aside(from: &str, record: Value) -> Value {
    serde_json::json!({ "from": from, "record": record })
}

/// What [`load`] found. Only `Missing` and `Loaded` leave the store writable; the other two are the
/// caller's to report (release log and UI), and the file stays as it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadResult {
    /// No file: an empty store, written on its first change.
    Missing,
    /// The file read; `rejected` names each stored record that did not (kept verbatim in the file).
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
    pub blocked: bool,
    /// The port as the player knows it: the stored port name (the port id when the name is empty).
    pub display_name: String,
}

/// A legacy record the import blocked, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blocked {
    /// Its position in the legacy list.
    pub index: usize,
    pub why: String,
}

/// What [`Store::import_legacy`] did, by each record's position in the legacy list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportReport {
    /// The import had already run: nothing changed.
    pub already: bool,
    /// The legacy document was not a JSON array: it is kept verbatim as one rejected entry.
    pub unreadable: Option<String>,
    /// Imported and not blocked.
    pub imported: Vec<usize>,
    /// Imported, waiting for the player's assignment.
    pub blocked: Vec<Blocked>,
    /// Not readable; kept verbatim.
    pub rejected: Vec<Rejected>,
    /// The store already held a binding on that control (same port id, channel, kind, number).
    pub skipped: Vec<usize>,
}

/// What one [`Snapshot::write`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Written {
    Wrote,
    /// The file already holds this revision or a newer one: nothing was written.
    Skipped,
}

/// What every snapshot of one store shares: the lock that serializes writes, and the newest revision on
/// disk (0: the file as loaded, or none).
#[derive(Default)]
struct Slot {
    write: Mutex<()>,
    written: AtomicU64,
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
    /// Why no write may replace the file, when one may not.
    read_only: Option<Arc<str>>,
    slot: Arc<Slot>,
}

/// Read the store in `dir`. The store is always usable; [`LoadResult`] says whether it can be written.
pub fn load(dir: &Path) -> (Store, LoadResult) {
    let file = dir.join(FILE_NAME);
    let mut store = Store {
        records: Arc::default(),
        rejected: Arc::default(),
        imported: false,
        import_revision: None,
        revision: 0,
        read_only: None,
        slot: Arc::default(),
    };
    let result = match std::fs::read(&file) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => LoadResult::Missing,
        Err(e) => LoadResult::Invalid(format!("{}: {e}", file.display())),
        Ok(bytes) => read_document(&mut store, &file, &bytes),
    };
    match &result {
        LoadResult::Invalid(why) => store.read_only = Some(why.as_str().into()),
        LoadResult::UnsupportedVersion(n) => {
            store.read_only = Some(format!("{}: version {n} (this build reads version {VERSION})", file.display()).into())
        }
        LoadResult::Missing | LoadResult::Loaded { .. } => {}
    }
    (store, result)
}

fn read_document(store: &mut Store, file: &Path, bytes: &[u8]) -> LoadResult {
    let invalid = |e: serde_json::Error| LoadResult::Invalid(format!("{}: {e}", file.display()));
    let value: Value = match serde_json::from_slice(bytes) {
        Ok(value) => value,
        Err(e) => return invalid(e),
    };
    #[derive(Deserialize)]
    struct Head {
        version: u64,
    }
    match serde_json::from_value::<Head>(value.clone()) {
        Err(e) => return invalid(e),
        Ok(Head { version }) if version != VERSION => return LoadResult::UnsupportedVersion(version),
        Ok(_) => {}
    }
    let doc: OnDisk = match serde_json::from_value(value) {
        Ok(doc) => doc,
        Err(e) => return invalid(e),
    };
    let mut records = Vec::new();
    let mut kept = doc.rejected;
    let mut rejected = Vec::new();
    for (index, value) in doc.bindings.into_iter().enumerate() {
        match parse_record(value.clone()) {
            Ok(record) => records.push(record),
            Err(reason) => {
                rejected.push(Rejected { index, reason });
                kept.push(set_aside("store", value));
            }
        }
    }
    store.records = Arc::new(records);
    store.rejected = Arc::new(kept);
    store.imported = doc.legacy.imported;
    LoadResult::Loaded { rejected }
}

impl Store {
    fn bump(&mut self) {
        self.revision += 1;
    }

    /// False when the file was unreadable or newer: every write is refused this session.
    pub fn writable(&self) -> bool {
        self.read_only.is_none()
    }

    /// The legacy import ran and a write holding it has completed (or the loaded file already held it):
    /// `lf.midiLearn` may go.
    pub fn legacy_durable(&self) -> bool {
        self.imported && self.import_revision.is_none_or(|r| self.slot.written.load(Ordering::Acquire) >= r)
    }

    /// The bindings the learn model runs, in list order: every one not blocked.
    pub fn bindings(&self) -> Vec<Binding> {
        self.records.iter().filter(|r| !r.blocked).map(|r| r.binding.clone()).collect()
    }

    /// Every binding, for the UI's list. [`Store::assign`] takes an index into this list.
    pub fn listed(&self) -> Vec<Listed> {
        self.records
            .iter()
            .map(|r| Listed {
                binding: r.binding.clone(),
                origin: r.origin,
                blocked: r.blocked,
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
            read_only: self.read_only.clone(),
            slot: Arc::clone(&self.slot),
        }
    }

    /// The learn model's list after an edit ([`Store::bindings`] changed). A binding keeps the origin of
    /// the record on its control, else it is native; blocked records stay, after the list.
    pub fn replace(&mut self, list: Vec<Binding>) {
        let origin = |b: &Binding| {
            self.records
                .iter()
                .find(|r| !r.blocked && control(&r.binding) == control(b))
                .map_or(Origin::Native, |r| r.origin)
        };
        let mut next: Vec<Record> = list
            .into_iter()
            .map(Binding::normalized)
            .map(|binding| Record { origin: origin(&binding), blocked: false, binding })
            .collect();
        next.extend(self.records.iter().filter(|r| r.blocked).cloned());
        if next != *self.records {
            self.records = Arc::new(next);
            self.bump();
        }
    }

    /// The player assigns listed binding `index` to a present port: its id and name become that port's,
    /// it is no longer blocked, its origin stays. Refused when another active binding already has that
    /// control (one action per message), or there is no such binding.
    pub fn assign(&mut self, index: usize, port_id: &str, port_name: &str) -> Result<(), String> {
        let record = self.records.get(index).ok_or_else(|| format!("no binding {index}"))?;
        let binding = Binding { port_id: port_id.to_owned(), port_name: port_name.to_owned(), ..record.binding.clone() };
        let taken = self
            .records
            .iter()
            .enumerate()
            .any(|(i, r)| i != index && !r.blocked && control(&r.binding) == control(&binding));
        if taken {
            return Err(format!("another binding on {port_name} already uses this message"));
        }
        let record = &mut Arc::make_mut(&mut self.records)[index];
        record.binding = binding;
        record.blocked = false;
        self.bump();
        Ok(())
    }

    /// Resolution moved the bindings on `old_id` to a present port's canonical `new_id`: persist the move.
    /// Blocked records stay where they are. Answers how many moved.
    pub fn reanchor(&mut self, old_id: &str, new_id: &str) -> usize {
        if old_id == new_id || !self.records.iter().any(|r| !r.blocked && r.binding.port_id == old_id) {
            return 0;
        }
        let mut moved = 0;
        for r in Arc::make_mut(&mut self.records).iter_mut().filter(|r| !r.blocked && r.binding.port_id == old_id) {
            r.binding.port_id = new_id.to_owned();
            moved += 1;
        }
        self.bump();
        moved
    }

    /// Import the web's list (`lf.midiLearn` verbatim; `"[]"` when the key is absent), once. Each record
    /// keeps its legacy port id; the collision rule blocks what a renumbered run may have relearned. A
    /// second call changes nothing and reports `already`, whether the first has been written yet or not.
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
                Arc::make_mut(&mut self.rejected).push(set_aside("legacy", Value::String(json.to_owned())));
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
                    Arc::make_mut(&mut self.rejected).push(set_aside("legacy", value));
                }
            }
        }
        // Decision 9: the legacy ids each (port name, message) was learned under.
        let mut ids: HashMap<(&str, u8, Kind, u8), BTreeSet<&str>> = HashMap::new();
        for (_, b) in &read {
            ids.entry((&b.port_name, b.channel, b.kind, b.number)).or_default().insert(&b.port_id);
        }
        let mut added = Vec::new();
        for (index, b) in &read {
            let on = |r: &Record| control(&r.binding) == control(b);
            if self.records.iter().any(on) || added.iter().any(on) {
                report.skipped.push(*index);
                continue;
            }
            let under = &ids[&(b.port_name.as_str(), b.channel, b.kind, b.number)];
            let blocked = under.len() > 1;
            if blocked {
                let list = under.iter().copied().collect::<Vec<_>>().join(", ");
                let why = format!(
                    "{:?} bound this message under several port ids ({list}); a renumbered run may have relearned it",
                    b.port_name
                );
                report.blocked.push(Blocked { index: *index, why });
            } else {
                report.imported.push(*index);
            }
            added.push(Record { origin: Origin::Legacy, blocked, binding: b.clone() });
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
    read_only: Option<Arc<str>>,
    slot: Arc<Slot>,
}

impl Snapshot {
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Write this revision to `dir`, unless the file already holds it or a newer one. Refused, with the
    /// load's reason, when the store is read-only. The slot's lock is held through the write, so writes of
    /// one store never interleave; the caller's own lock need not be.
    pub fn write(&self, dir: &Path) -> Result<Written, String> {
        let file = dir.join(FILE_NAME);
        if let Some(why) = &self.read_only {
            return Err(format!("not written, the file is kept as it is: {why}"));
        }
        let _write = self.slot.write.lock().unwrap_or_else(PoisonError::into_inner);
        if self.revision <= self.slot.written.load(Ordering::Acquire) {
            return Ok(Written::Skipped);
        }
        let doc = Document { version: VERSION, bindings: &self.records, rejected: &self.rejected, legacy: self.legacy };
        let json = serde_json::to_vec_pretty(&doc).map_err(|e| format!("serialize: {e}"))?;
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        write_atomic(&file, &json)?;
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

    fn blocked(store: &Store) -> Vec<bool> {
        store.listed().iter().map(|l| l.blocked).collect()
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
        store.replace(vec![pedal.clone()]);
        assert_eq!(store.snapshot().write(&scratch.0), Ok(Written::Wrote));
        assert_eq!(
            scratch.json(),
            serde_json::json!({
                "version": 1,
                "bindings": [{ "origin": "native", "blocked": false, "binding": serde_json::to_value(&pedal).unwrap() }],
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
            (br#"{"version":"1","bindings":[],"rejected":[],"legacy":{"imported":false}}"#, |r| {
                matches!(r, LoadResult::Invalid(_))
            }),
            (br#"{"version":1,"bindings":{},"rejected":[],"legacy":{"imported":false}}"#, |r| {
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
            store.replace(vec![binding("native-1", "Pedal", 21)]);
            assert_eq!(store.bindings().len(), 1);
            let refused = store.snapshot().write(&scratch.0);
            assert!(refused.is_err(), "{refused:?}");
            assert!(!store.legacy_durable(), "a refused write never makes the import durable");
            assert_eq!(std::fs::read(scratch.file()).unwrap(), bytes, "the file is left as it was");
            assert_eq!(scratch.files(), vec![FILE_NAME]);
        }
    }

    #[test]
    fn a_record_this_build_cannot_read_is_reported_and_kept_verbatim_across_a_write() {
        let scratch = Scratch::new("malformed");
        let good = serde_json::json!({
            "origin": "native", "blocked": false,
            "binding": serde_json::to_value(binding("native-1", "Pedal", 20)).unwrap(),
        });
        let mut bad = good.clone();
        bad["binding"]["action"] = "selfDestruct".into();
        let older = serde_json::json!({ "from": "store", "record": { "anything": [1, 2] } });
        let doc = serde_json::json!({
            "version": 1, "bindings": [good, bad.clone()], "rejected": [older.clone()],
            "legacy": { "imported": true },
        });
        std::fs::write(scratch.file(), serde_json::to_vec(&doc).unwrap()).unwrap();
        let (mut store, result) = load(&scratch.0);
        let LoadResult::Loaded { rejected } = result else { panic!("{result:?}") };
        assert_eq!(rejected.iter().map(|r| r.index).collect::<Vec<_>>(), [1]);
        assert!(rejected[0].reason.contains("selfDestruct"), "{}", rejected[0].reason);
        assert_eq!(store.bindings().len(), 1);

        store.replace(vec![binding("native-2", "Keys", 64)]);
        assert_eq!(store.snapshot().write(&scratch.0), Ok(Written::Wrote));
        let written = scratch.json();
        assert_eq!(written["rejected"], serde_json::json!([older, { "from": "store", "record": bad }]));
        assert_eq!(written["bindings"].as_array().unwrap().len(), 1);
        assert_eq!(written["bindings"][0]["binding"]["portName"], "Keys");
        // Read again, the set-aside record stays set aside: nothing is lost on a second round either.
        let (store, _) = load(&scratch.0);
        store.snapshot().write(&scratch.0).unwrap();
        assert_eq!(scratch.json()["rejected"], written["rejected"]);
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
        assert_eq!(scratch.json()["rejected"], serde_json::json!([{ "from": "legacy", "record": unknown }]));
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
        store.replace(vec![binding("native-1", "Pedal", 1)]);
        let before_import = store.snapshot();
        store.import_legacy(&legacy_list(&[legacy("input-1", "Pedal", 20)]));
        assert!(!store.legacy_durable());
        before_import.write(&scratch.0).unwrap();
        assert!(!store.legacy_durable(), "that write did not hold the import");

        // The file held open without delete sharing: the rename over it fails, a write's last step.
        let held = std::fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(scratch.file()).unwrap();
        assert!(store.snapshot().write(&scratch.0).is_err());
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
        store.replace(vec![binding("native-1", "Old", 1)]);
        let older = store.snapshot();
        store.replace(vec![binding("native-1", "New", 2)]);
        let newer = store.snapshot();
        assert!(older.revision() < newer.revision());
        assert_eq!(newer.write(&scratch.0), Ok(Written::Wrote));
        assert_eq!(older.write(&scratch.0), Ok(Written::Skipped));
        assert_eq!(newer.write(&scratch.0), Ok(Written::Skipped), "already on disk");
        assert_eq!(scratch.json()["bindings"][0]["binding"]["portName"], "New");

        // Two threads, each with its own snapshot, in either order: the newer always wins.
        for flip in [false, true] {
            store.replace(vec![binding("native-1", "A", 3 + u8::from(flip))]);
            let a = store.snapshot();
            store.replace(vec![binding("native-1", "B", 5 + u8::from(flip))]);
            let b = store.snapshot();
            let (first, second) = if flip { (a, b) } else { (b, a) };
            let dir = scratch.0.clone();
            std::thread::spawn(move || first.write(&dir).unwrap()).join().unwrap();
            second.write(&scratch.0).unwrap();
            assert_eq!(scratch.json()["bindings"][0]["binding"]["portName"], "B");
            assert_eq!(scratch.files(), vec![FILE_NAME]);
        }
    }

    #[test]
    fn replace_keeps_each_controls_origin_and_the_blocked_records() {
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
        store.replace(list);
        let listed: Vec<(String, Origin, bool)> =
            store.listed().into_iter().map(|l| (l.binding.port_id, l.origin, l.blocked)).collect();
        assert_eq!(
            listed,
            [
                ("input-1".into(), Origin::Legacy, false),
                ("native-1".into(), Origin::Native, false),
                ("input-2".into(), Origin::Legacy, true),
                ("input-4".into(), Origin::Legacy, true),
            ]
        );
        let revision = store.snapshot().revision();
        store.replace(store.bindings());
        assert_eq!(store.snapshot().revision(), revision, "an unchanged list is no change");
    }

    #[test]
    fn assign_unblocks_onto_a_present_port_and_reanchor_moves_only_active_records() {
        let (mut store, _) = load(&Scratch::new("assign").0);
        store.import_legacy(&legacy_list(&[
            legacy("input-1", "Pedal", 20),
            legacy("input-3", "Pedal", 20),
            legacy("input-1", "Pedal", 21),
        ]));
        assert_eq!(blocked(&store), [true, true, false]);

        store.assign(0, "dev-A#0", "Pedal (USB)").unwrap();
        let first = &store.listed()[0];
        assert_eq!((first.blocked, first.origin), (false, Origin::Legacy), "the origin stays");
        assert_eq!((first.binding.port_id.as_str(), first.display_name.as_str()), ("dev-A#0", "Pedal (USB)"));
        assert_eq!(store.bindings().len(), 2);
        assert!(store.assign(1, "dev-A#0", "Pedal (USB)").is_err(), "that control is taken");
        assert!(store.listed()[1].blocked, "a refused assignment changes nothing");
        assert!(store.assign(9, "dev-A#0", "Pedal (USB)").is_err());

        // Resolution moved input-1's records to a present port: the blocked one on input-3 stays.
        let revision = store.snapshot().revision();
        assert_eq!(store.reanchor("input-1", "dev-B#0"), 1);
        assert_eq!(store.reanchor("input-3", "dev-B#0"), 0, "blocked records do not move");
        assert_eq!(store.reanchor("gone", "dev-B#0"), 0);
        assert_eq!(store.snapshot().revision(), revision + 1, "only a move is a change");
        let ids: Vec<String> = store.listed().into_iter().map(|l| l.binding.port_id).collect();
        assert_eq!(ids, ["dev-A#0", "input-3", "dev-B#0"]);
    }
}
