//! The crash marker's file and its per-load terminal rule (D6), apart from the restore that drives
//! them (`tests/restore.rs`).

use std::cell::RefCell;

use lf_rig::marker::{self, Cleared};
use lf_rig::{Loads, MarkerRead};

#[test]
fn failed_unlink_writes_empty_marker() {
    let written = RefCell::new(None::<Vec<u8>>);
    let write = |bytes: &[u8]| -> Result<(), ()> {
        *written.borrow_mut() = Some(bytes.to_vec());
        Ok(())
    };
    assert_eq!(marker::clear(|| Err(()), write), Cleared::Emptied);
    let empty = written.borrow().clone().expect("an empty marker written in its place");
    assert_eq!(marker::read(Some(&empty)), MarkerRead::None, "it reads as none");

    // The unlink works: nothing written.
    let mut wrote = false;
    assert_eq!(marker::clear(|| Ok::<(), ()>(()), |_| {
        wrote = true;
        Ok(())
    }), Cleared::Removed);
    assert!(!wrote);

    // Both fail: the marker stands, and the caller logs once.
    assert_eq!(marker::clear(|| Err(()), |_| Err(())), Cleared::Failed);
}

#[test]
fn marker_file_reads_and_writes() {
    assert_eq!(marker::read(None), MarkerRead::None);
    assert_eq!(marker::read(Some(&marker::text(&[3, 9]))), MarkerRead::Names(vec![3, 9]));
    assert_eq!(marker::read(Some(br#"{"version":1,"units":[4]}"#)), MarkerRead::Names(vec![4]));
    for bad in [&b""[..], b"[4]", b"{\"version\":2,\"units\":[4]}", b"{\"version\":1,\"units\":[0]}", b"{\"version\":1}", b"not json"] {
        assert_eq!(marker::read(Some(bad)), MarkerRead::Unreadable, "{}", String::from_utf8_lossy(bad));
    }
}

#[test]
fn loads_go_terminal_by_settle_or_failure_with_latch() {
    let mut loads = Loads::default();
    for id in [1, 2, 3, 4] {
        loads.issue(id);
    }
    loads.answered(1, 11);
    loads.answered(2, 12);
    loads.answered(3, 13);
    // 1 applied: inside its settle it stays, except at a clean close.
    assert!(loads.applied(11));
    assert!(!loads.applied(11), "the settle starts once");
    assert_eq!(loads.at_close(), [2, 3, 4]);
    assert!(loads.settled(11));
    // 2 failed with its owner still running: not terminal until the latch.
    assert!(!loads.failed(12, false));
    assert!(loads.exited(12));
    // 3's owner ended before it failed.
    assert!(!loads.exited(13));
    assert!(loads.failed(13, false));
    // 4 refused by the host: gone at the next write, with no rewrite of its own.
    loads.withdraw(4);
    assert!(loads.is_empty());
}
