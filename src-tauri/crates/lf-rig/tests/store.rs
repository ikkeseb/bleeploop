//! The record's file (D3, D8, D13) over a fault-injecting fake `Disk`, and the tolerant reader
//! (`parseRig`'s rules). The app's real `Disk` and its tests on Windows are CP2b's.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io;
use std::rc::Rc;
use std::time::{Duration, Instant};

use lf_rig::{
    Commit, Disk, Opened, Refusal, RigRecord, RigStatus, SlotRecord, Store, UnitBody, UnitKind, UnitParam, UnitRecord, Version1, DEV_FILES,
    RELEASE_FILES, SAVE_QUIET, TICK,
};
use serde_json::Value;

// ── the fake data folder ─────────────────────────────────────────────────────────────────────────────

/// One data folder several fake `Disk`s (processes) share.
#[derive(Default)]
struct Folder {
    files: BTreeMap<String, Vec<u8>>,
    locked: bool,
    /// Every call, in order, as `"<op> <name>"`.
    calls: Vec<String>,
    /// The next this many `write_atomic`s fail before writing anything.
    fail_writes: u32,
    /// The next this many `write_atomic`s write and sync their temp file, then fail the rename.
    fail_renames: u32,
}

type Shared = Rc<RefCell<Folder>>;

struct FakeDisk {
    folder: Shared,
    holds_lock: bool,
}

fn disk(folder: &Shared) -> FakeDisk {
    FakeDisk { folder: folder.clone(), holds_lock: false }
}

impl Disk for FakeDisk {
    fn read(&mut self, name: &str) -> io::Result<Option<Vec<u8>>> {
        let mut f = self.folder.borrow_mut();
        f.calls.push(format!("read {name}"));
        Ok(f.files.get(name).cloned())
    }

    fn write_atomic(&mut self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let mut f = self.folder.borrow_mut();
        f.calls.push(format!("write {name}"));
        if f.fail_writes > 0 {
            f.fail_writes -= 1;
            return Err(io::Error::other("disk full"));
        }
        f.files.insert(format!("{name}.tmp"), bytes.to_vec());
        if f.fail_renames > 0 {
            f.fail_renames -= 1;
            return Err(io::Error::other("rename refused"));
        }
        let tmp = f.files.remove(&format!("{name}.tmp")).unwrap_or_default();
        f.files.insert(name.to_owned(), tmp);
        Ok(())
    }

    fn remove(&mut self, name: &str) -> io::Result<()> {
        let mut f = self.folder.borrow_mut();
        f.calls.push(format!("remove {name}"));
        f.files.remove(name);
        Ok(())
    }

    fn try_lock(&mut self, name: &str) -> io::Result<()> {
        let mut f = self.folder.borrow_mut();
        f.calls.push(format!("lock {name}"));
        if f.locked {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "locked"));
        }
        f.locked = true;
        self.holds_lock = true;
        Ok(())
    }
}

impl Drop for FakeDisk {
    fn drop(&mut self) {
        if self.holds_lock {
            self.folder.borrow_mut().locked = false;
        }
    }
}

fn folder_with(record: Option<&[u8]>) -> Shared {
    let folder = Shared::default();
    if let Some(bytes) = record {
        folder.borrow_mut().files.insert(RELEASE_FILES.record.to_owned(), bytes.to_vec());
    }
    folder
}

fn file(folder: &Shared) -> Option<Vec<u8>> {
    folder.borrow().files.get(RELEASE_FILES.record).cloned()
}

// ── records ──────────────────────────────────────────────────────────────────────────────────────────

fn echo(id: u32, level: f64) -> UnitRecord {
    UnitRecord { id, body: UnitBody::Builtin { kind: UnitKind::Echo, params: Some(vec![(UnitParam::from_key("echoLevel").expect("a key"), level)]) }, bypass: Some(false) }
}

fn record(a: Vec<UnitRecord>, volume: f64) -> RigRecord {
    RigRecord {
        version: Version1,
        slots: [SlotRecord { volume, units: a, pending: vec![] }, SlotRecord { volume: 1.0, units: vec![], pending: vec![] }],
    }
}

fn bytes(r: &RigRecord) -> Vec<u8> {
    serde_json::to_vec(r).expect("serializes")
}

fn open_present(r: &RigRecord) -> (Shared, Store<FakeDisk>) {
    let folder = folder_with(Some(&bytes(r)));
    let (store, opened) = Store::open(disk(&folder), RELEASE_FILES);
    assert_eq!(opened, Opened::Present(r.clone()));
    (folder, store)
}

// ── the reader ───────────────────────────────────────────────────────────────────────────────────────

#[test]
fn parse_is_tolerant() {
    let text = r#"{ "version": 1, "slots": [
        { "volume": 2.5, "units": [
            { "id": 7, "kind": "echo", "params": { "echoLevel": 0.4, "bogus": 1, "echoTime": "x" }, "bypass": true },
            { "id": 8, "kind": "warp" },
            { "id": 0, "kind": "ott" },
            { "id": 9, "kind": "ott", "bypass": "yes" },
            { "id": 10, "kind": "plugin", "plugin": { "format": "au", "path": "p", "id": "x" } },
            { "id": 11, "kind": "plugin", "plugin": { "format": "clap", "path": "C:\\a.clap", "id": "a", "name": 3 } },
            { "id": 12, "kind": "ring", "params": [] },
            "junk",
            { "id": 13.0, "kind": "ring" }
        ], "pending": [ { "id": 7, "kind": "reverb" }, { "id": 14, "kind": "reverb" } ] },
        { "volume": "loud", "units": { "not": "a list" }, "pending": [ { "id": 14, "kind": "ott" } ] }
    ] }"#;
    let r = RigRecord::read(text.as_bytes()).expect("a v1 record");
    let [a, b] = &r.slots;
    assert_eq!(a.volume, 1.0, "a volume out of range reads unity");
    assert_eq!(b.volume, 1.0, "a volume that is no number reads unity");
    assert_eq!(a.units.iter().map(|u| u.id).collect::<Vec<_>>(), [7, 11, 13], "bad units skipped");
    assert_eq!(a.units[0], UnitRecord {
        id: 7,
        body: UnitBody::Builtin { kind: UnitKind::Echo, params: Some(vec![(UnitParam::from_key("echoLevel").expect("a key"), 0.4)]) },
        bypass: Some(true),
    }, "an unknown parameter or one that is no number is dropped, the unit kept");
    match &a.units[1].body {
        UnitBody::Plugin(p) => assert_eq!((p.plugin.format.as_str(), p.name.as_deref()), ("clap", None), "a name that is no text is dropped"),
        other => panic!("a plugin, got {other:?}"),
    }
    assert_eq!(a.units[2].bypass, None, "an omitted bypass stays omitted");
    assert_eq!(a.pending.iter().map(|u| u.id).collect::<Vec<_>>(), [14], "an id met before (in units) is skipped in pending");
    assert!(b.units.is_empty());
    assert!(b.pending.is_empty(), "an id met in slot A is skipped in slot B");
}

#[test]
fn whole_record_faults_are_unreadable() {
    for text in ["", "not json", "[]", r#"{"version":1}"#, r#"{"version":1,"slots":[{}]}"#, r#"{"version":1,"slots":[{},3]}"#, r#"{"slots":[{},{}]}"#] {
        assert!(RigRecord::read(text.as_bytes()).is_err(), "{text:?} is unreadable");
    }
    assert!(RigRecord::read(&[0xff, 0xfe]).is_err(), "not UTF-8");
    assert!(RigRecord::read(br#"{"version":1,"slots":[{},{}]}"#).is_ok(), "two empty slots are a record");
}

/// The tolerant reader and the strict serde read the fixture (and the records the page writes) as one
/// value, so the file the store reads back is the record the import stored.
#[test]
fn tolerant_read_holds_the_fixture() {
    let fixture: Value = serde_json::from_str(include_str!("../../../../verify/fixtures/rig-record.json")).expect("JSON");
    let written = fixture["written"].as_array().cloned().expect("written records");
    for json in std::iter::once(fixture["record"].clone()).chain(written) {
        let strict: RigRecord = serde_json::from_value(json.clone()).expect("strict parse");
        let tolerant = RigRecord::read(json.to_string().as_bytes()).expect("tolerant read");
        assert_eq!(tolerant, strict);
        assert_eq!(RigRecord::read(&bytes(&strict)).expect("its own output"), strict);
    }
}

// ── the store ────────────────────────────────────────────────────────────────────────────────────────

#[test]
fn unknown_version_is_unreadable_and_kept_byte_for_byte() {
    for text in [&br#"{ "version": 2, "slots": [ {"volume": 1, "units": []}, {"volume": 1, "units": []} ], "new": true }"#[..], b"{ not json"] {
        let folder = folder_with(Some(text));
        folder.borrow_mut().files.insert(RELEASE_FILES.marker.to_owned(), b"{}".to_vec());
        let (mut store, opened) = Store::open(disk(&folder), RELEASE_FILES);
        assert!(matches!(opened, Opened::Unreadable(_)), "{opened:?}");
        assert_eq!(store.status(), RigStatus::Unreadable);
        let r = record(vec![echo(1, 0.5)], 1.0);
        assert_eq!(store.import(&r), Commit::Refused(Refusal::Unreadable));
        assert_eq!(store.commit(&r), Commit::Refused(Refusal::Unreadable));
        assert_eq!(store.tick(r.clone(), Instant::now()), Some(Commit::Refused(Refusal::Unreadable)));
        assert!(store.write_marker(b"[]").is_err() && store.remove_marker().is_err(), "nor the crash marker (D8)");
        assert_eq!(store.close(&r), Commit::Refused(Refusal::Unreadable));
        assert_eq!(file(&folder).as_deref(), Some(text), "kept byte for byte");
        assert!(!folder.borrow().calls.iter().any(|c| c.starts_with("write") || c.starts_with("remove")), "nothing written");
        assert_eq!(folder.borrow().files.get(RELEASE_FILES.marker).map(Vec::as_slice), Some(&b"{}"[..]), "the marker untouched");
    }
}

#[test]
fn a_missing_file_is_absent_and_only_the_import_stores_the_first_record() {
    let folder = folder_with(None);
    let (mut store, opened) = Store::open(disk(&folder), RELEASE_FILES);
    assert_eq!(opened, Opened::Absent);
    let r = record(vec![echo(1, 0.5)], 0.8);
    assert_eq!(store.commit(&r), Commit::Refused(Refusal::Absent));
    assert_eq!(store.tick(r.clone(), Instant::now()), None);
    assert_eq!(file(&folder), None);
    assert_eq!(store.import(&r), Commit::Committed);
    assert_eq!(store.status(), RigStatus::Present);
    assert_eq!(store.import(&record(vec![], 1.0)), Commit::Refused(Refusal::Present), "a second import stores nothing");
    assert_eq!(RigRecord::read(&file(&folder).expect("stored")).expect("readable"), r);
}

#[test]
fn failed_import_answers_absent_next_launch() {
    let folder = folder_with(None);
    let r = record(vec![echo(1, 0.5)], 0.8);
    {
        let (mut store, _) = Store::open(disk(&folder), RELEASE_FILES);
        folder.borrow_mut().fail_renames = 1;
        assert!(matches!(store.import(&r), Commit::Failed { log: true, .. }));
        assert_eq!(store.status(), RigStatus::Absent, "nothing restored this launch");
    }
    let (mut store, opened) = Store::open(disk(&folder), RELEASE_FILES);
    assert_eq!(opened, Opened::Absent, "the next launch imports again");
    assert_eq!(store.import(&r), Commit::Committed);
    drop(store);
    let (_, opened) = Store::open(disk(&folder), RELEASE_FILES);
    assert_eq!(opened, Opened::Present(r));
}

#[test]
fn lock_refused_is_unavailable_and_reads_nothing() {
    let r = record(vec![echo(1, 0.5)], 1.0);
    let (folder, _first) = open_present(&r);
    folder.borrow_mut().files.insert(RELEASE_FILES.marker.to_owned(), b"{}".to_vec());
    let calls_before = folder.borrow().calls.len();

    let (mut second, opened) = Store::open(disk(&folder), RELEASE_FILES);
    assert!(matches!(opened, Opened::Unavailable { .. }), "{opened:?}");
    assert_eq!(second.status(), RigStatus::Unavailable);
    let changed = record(vec![], 0.2);
    assert_eq!(second.import(&changed), Commit::Refused(Refusal::Unavailable));
    assert_eq!(second.commit(&changed), Commit::Refused(Refusal::Unavailable));
    assert_eq!(second.tick(changed.clone(), Instant::now()), Some(Commit::Refused(Refusal::Unavailable)));
    assert!(second.read_marker().is_err() && second.write_marker(b"[]").is_err() && second.remove_marker().is_err());
    let calls: Vec<String> = folder.borrow().calls[calls_before..].to_vec();
    assert_eq!(calls, [format!("lock {}", RELEASE_FILES.lock)], "the lock first, then nothing");
    assert_eq!(folder.borrow().files.get(RELEASE_FILES.marker).map(Vec::as_slice), Some(&b"{}"[..]), "the marker untouched");
}

#[test]
fn the_lock_is_taken_before_the_first_read_and_freed_with_the_process() {
    let r = record(vec![], 1.0);
    let (folder, first) = open_present(&r);
    assert_eq!(folder.borrow().calls[..2], [format!("lock {}", RELEASE_FILES.lock), format!("read {}", RELEASE_FILES.record)]);
    drop(first);
    let (_, opened) = Store::open(disk(&folder), RELEASE_FILES);
    assert_eq!(opened, Opened::Present(r));
}

#[test]
fn dev_and_release_file_names() {
    assert_eq!((RELEASE_FILES.record, RELEASE_FILES.marker, RELEASE_FILES.lock), ("rig.json", "rig-inflight.json", "rig.lock"));
    assert_eq!((DEV_FILES.record, DEV_FILES.marker, DEV_FILES.lock), ("rig.dev.json", "rig-inflight.dev.json", "rig.dev.lock"));
}

#[test]
fn write_failure_keeps_dirty_and_retries() {
    let old = record(vec![echo(1, 0.5)], 1.0);
    let (folder, mut store) = open_present(&old);
    let t0 = Instant::now();
    let new = record(vec![echo(1, 0.5), echo(2, 0.5)], 1.0);
    folder.borrow_mut().fail_writes = 2;
    assert!(matches!(store.tick(new.clone(), t0), Some(Commit::Failed { log: true, .. })), "a structure change goes at once; the run's first failure logs");
    assert!(store.dirty());
    assert!(matches!(store.tick(new.clone(), t0 + TICK), Some(Commit::Failed { log: false, .. })), "retried at the next tick, no second log line");
    assert_eq!(store.tick(new.clone(), t0 + TICK * 2), Some(Commit::Committed));
    assert!(!store.dirty());
    assert_eq!(RigRecord::read(&file(&folder).expect("stored")).expect("readable"), new);
    assert_eq!(store.tick(new.clone(), t0 + TICK * 3), None, "clean: nothing written");

    // A value change waits for the quiet; failing, it is retried at the next tick, not after another quiet.
    let louder = record(vec![echo(1, 0.9), echo(2, 0.5)], 1.0);
    let t1 = t0 + TICK * 4;
    assert_eq!(store.tick(louder.clone(), t1), None);
    folder.borrow_mut().fail_writes = 1;
    assert!(matches!(store.tick(louder.clone(), t1 + SAVE_QUIET), Some(Commit::Failed { log: true, .. })), "a new failure run logs again");
    assert_eq!(store.tick(louder.clone(), t1 + SAVE_QUIET + TICK), Some(Commit::Committed));

    // A durable step's commit that failed stays owed: the store is dirty and the next tick retries it,
    // a value change too, without waiting for the quiet.
    let placed = record(vec![echo(1, 0.9), echo(2, 0.5)], 0.6);
    folder.borrow_mut().fail_writes = 1;
    assert!(matches!(store.commit(&placed), Commit::Failed { log: true, .. }));
    assert!(store.dirty(), "the failed commit is the dirty target");
    assert_eq!(store.tick(placed.clone(), t1 + SAVE_QUIET + TICK * 2), Some(Commit::Committed));
    assert!(!store.dirty());
    assert_eq!(RigRecord::read(&file(&folder).expect("stored")).expect("readable"), placed);
}

#[test]
fn values_wait_for_quiet_and_structure_goes_at_the_next_tick() {
    let old = record(vec![echo(1, 0.5)], 1.0);
    let (folder, mut store) = open_present(&old);
    let writes = || folder.borrow().calls.iter().filter(|c| c.starts_with("write")).count();
    let t0 = Instant::now();
    // A fader drag: a new value each tick for a while, then still.
    for i in 0..10u32 {
        assert_eq!(store.tick(record(vec![echo(1, 0.5)], 0.5 + f64::from(i) * 0.01), t0 + TICK * i), None);
    }
    let last = t0 + TICK * 9;
    let settled = record(vec![echo(1, 0.5)], 0.59);
    assert_eq!(store.tick(settled.clone(), last + SAVE_QUIET - Duration::from_millis(1)), None, "not quiet long enough");
    assert_eq!(store.tick(settled.clone(), last + SAVE_QUIET), Some(Commit::Committed));
    assert_eq!(writes(), 1, "one write for the whole drag");
    // A unit added goes at the next tick.
    let t1 = last + SAVE_QUIET + TICK;
    assert_eq!(store.tick(record(vec![echo(1, 0.5), echo(2, 0.1)], 0.59), t1), Some(Commit::Committed));
    assert_eq!(writes(), 2);
}

#[test]
fn rename_failure_is_no_commit() {
    let old = record(vec![echo(1, 0.5)], 1.0);
    let (folder, mut store) = open_present(&old);
    let new = record(vec![echo(1, 0.5), echo(2, 0.5)], 1.0);
    folder.borrow_mut().fail_renames = 1;
    assert!(matches!(store.commit(&new), Commit::Failed { .. }), "a durable step is not committed when the rename failed");
    assert_eq!(store.stored(), Some(&old));
    assert_eq!(file(&folder), Some(bytes(&old)), "the file holds the old record");
    assert!(store.dirty());
    assert_eq!(store.tick(new.clone(), Instant::now()), Some(Commit::Committed), "retried at the next tick");
    drop(store);
    let (_, opened) = Store::open(disk(&folder), RELEASE_FILES);
    assert_eq!(opened, Opened::Present(new));
}

#[test]
fn close_writes_the_last_projection_then_nothing() {
    let old = record(vec![echo(1, 0.5)], 1.0);
    let (folder, mut store) = open_present(&old);
    let last = record(vec![echo(1, 0.5)], 0.3);
    assert_eq!(store.close(&last), Commit::Committed, "a value not yet quiet is taken at close");
    let after = record(vec![], 0.3);
    assert_eq!(store.commit(&after), Commit::Refused(Refusal::Closed));
    assert_eq!(store.tick(after, Instant::now()), Some(Commit::Refused(Refusal::Closed)));
    assert_eq!(RigRecord::read(&file(&folder).expect("stored")).expect("readable"), last);
    assert!(store.write_marker(b"[]").is_ok(), "the marker stays this process's after close (D7)");
}
