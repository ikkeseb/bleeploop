//! A session export's .zip, written here so the player hears "Exported" only once the file is on disk
//! (an `<a download>` returns before it exists). The WebView sends the archive in parts
//! (`src/platform/host.tauri.ts`) and this side holds no state between them: all it knows is on disk,
//! in `<name>.part`. The last part closes that file and renames it onto a free name in the player's
//! Downloads folder. Any failure removes the part file. The full path goes only into the answer to the
//! WebView: the release log hides the user name (`app_log_dir`), so no error string carries it.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use tauri::Manager;

/// How many names a finished export tries (`<stem>.zip`, `<stem> (2).zip`, ...). The stem has minute
/// resolution, so exports of one minute do meet.
const FREE_NAMES: u32 = 100;

/// The stem of a plain `.zip` file name, or the refusal. A name never reaches outside `dir`.
fn zip_stem(name: &str) -> Result<&str, String> {
    let plain = !name.contains("..") && !name.chars().any(|c| matches!(c, '/' | '\\' | ':') || c.is_control());
    name.strip_suffix(".zip")
        .filter(|_| plain)
        .ok_or_else(|| "the export's file name was refused".to_string())
}

/// The failure's kind, never the `io::Error`'s text or a path.
fn io_failure(e: io::Error) -> String {
    match e.kind() {
        io::ErrorKind::StorageFull => "The disk is full.".to_string(),
        kind => format!("The file could not be written ({kind:?})."),
    }
}

/// Claim a free final name: `std::fs::rename` replaces on Windows, so the rename goes onto a file this
/// export created.
fn claim(dir: &Path, stem: &str) -> Result<PathBuf, String> {
    for n in 1..=FREE_NAMES {
        let name = if n == 1 { format!("{stem}.zip") } else { format!("{stem} ({n}).zip") };
        let path = dir.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => return Ok(path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io_failure(e)),
        }
    }
    Err("No free file name is left for this export.".to_string())
}

/// Move the finished part file onto a free name. A failed rename gives the claimed name back.
fn land(dir: &Path, stem: &str, part: &Path) -> Result<PathBuf, String> {
    let landed = claim(dir, stem)?;
    fs::rename(part, &landed).map_err(|e| {
        let _ = fs::remove_file(&landed);
        io_failure(e)
    })?;
    Ok(landed)
}

fn write_part(dir: &Path, stem: &str, part: &Path, offset: u64, total: u64, bytes: &[u8]) -> Result<Option<PathBuf>, String> {
    let end = offset
        .checked_add(bytes.len() as u64)
        .filter(|end| *end <= total)
        .ok_or("A part of the export ran past its end.")?;
    if total == 0 || (bytes.is_empty() && end < total) {
        return Err("A part of the export was empty.".to_string());
    }
    // File::create replaces what a crashed export left; a later part only ever continues.
    let opened = if offset == 0 { File::create(part) } else { OpenOptions::new().write(true).open(part) };
    let mut file = opened.map_err(io_failure)?;
    if file.metadata().map_err(io_failure)?.len() != offset {
        return Err("A part of the export arrived out of order.".to_string());
    }
    file.seek(SeekFrom::End(0)).map_err(io_failure)?;
    file.write_all(bytes).map_err(io_failure)?;
    if end < total {
        return Ok(None);
    }
    file.sync_all().map_err(io_failure)?;
    drop(file);
    land(dir, stem, part).map(Some)
}

/// One part of the export `name`, `total` bytes long, whose bytes belong at `offset`, into `dir`. The
/// answer is where the file landed for the part that ends at `total`, null for an earlier one.
pub fn save_part(dir: &Path, name: &str, offset: u64, total: u64, bytes: &[u8]) -> Result<Option<PathBuf>, String> {
    let stem = zip_stem(name)?;
    let part = dir.join(format!("{name}.part"));
    write_part(dir, stem, &part, offset, total, bytes).inspect_err(|_| {
        let _ = fs::remove_file(&part);
    })
}

/// A part of a session export into the player's Downloads folder (`save_part`). The raw request body is
/// the part's bytes; the `name`, `offset` and `total` headers say where they go.
#[tauri::command]
pub async fn export_save_part(request: tauri::ipc::Request<'_>, app: tauri::AppHandle) -> Result<Option<String>, String> {
    let header = |key: &str| request.headers().get(key).and_then(|v| v.to_str().ok());
    let number = |key: &str| header(key).and_then(|v| v.parse::<u64>().ok());
    let name = header("name").ok_or("export_save_part needs a name header")?.to_string();
    let offset = number("offset").ok_or("export_save_part needs an offset header")?;
    let total = number("total").ok_or("export_save_part needs a total header")?;
    // A body that is not raw (Tauri's postMessage fallback sends JSON) reads as an empty part: it fails
    // like any other bad part, and that removes the part file.
    let bytes = match request.body() {
        tauri::ipc::InvokeBody::Raw(bytes) => bytes.clone(),
        _ => Vec::new(),
    };
    let dir = app.path().download_dir().map_err(|_| "Windows did not name a Downloads folder.")?;
    // Disk work, off the async runtime.
    let landed = tauri::async_runtime::spawn_blocking(move || save_part(&dir, &name, offset, total, &bytes))
        .await
        .map_err(|_| "The export's write stopped.")??;
    Ok(landed.map(|path| path.to_string_lossy().into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

    const NAME: &str = "bleeploop-2026-10-31-1234.zip";

    /// A temp folder of its own, removed with the test.
    struct Dir(PathBuf);
    impl Dir {
        fn new(tag: &str) -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!("lf-export-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Relaxed)));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
        fn names(&self) -> Vec<String> {
            let mut names: Vec<_> = fs::read_dir(&self.0).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
            names.sort();
            names
        }
        /// A failure: its answer names no path, and the folder holds `left` and nothing else.
        fn failed(&self, answer: Result<Option<PathBuf>, String>, left: &[&str]) {
            let error = answer.expect_err("a failure");
            assert!(!error.contains(self.0.to_str().unwrap()), "{error}");
            assert_eq!(self.names(), left);
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn save(dir: &Dir, offset: u64, total: u64, bytes: &[u8]) -> Result<Option<PathBuf>, String> {
        save_part(&dir.0, NAME, offset, total, bytes)
    }

    #[test]
    fn parts_land_as_one_file_at_the_last_one() {
        let dir = Dir::new("parts");
        assert_eq!(save(&dir, 0, 9, b"abc"), Ok(None));
        assert_eq!(dir.names(), [format!("{NAME}.part")]);
        assert_eq!(save(&dir, 3, 9, b"def"), Ok(None));
        assert_eq!(save(&dir, 6, 9, b"ghi"), Ok(Some(dir.path(NAME))));
        assert_eq!(dir.names(), [NAME]);
        assert_eq!(fs::read(dir.path(NAME)).unwrap(), b"abcdefghi");
    }

    #[test]
    fn a_single_part_is_the_whole_file() {
        let dir = Dir::new("single");
        assert_eq!(save(&dir, 0, 3, b"abc"), Ok(Some(dir.path(NAME))));
        assert_eq!(dir.names(), [NAME]);
    }

    #[test]
    fn a_wrong_offset_fails_and_removes_the_part_file() {
        for offset in [2, 4] {
            let dir = Dir::new("offset");
            save(&dir, 0, 9, b"abc").unwrap();
            dir.failed(save(&dir, offset, 9, b"def"), &[]);
        }
    }

    #[test]
    fn a_later_part_with_no_part_file_fails_and_creates_none() {
        let dir = Dir::new("orphan");
        dir.failed(save(&dir, 3, 9, b"def"), &[]);
    }

    #[test]
    fn an_overrun_fails_and_removes_the_part_file() {
        let dir = Dir::new("overrun");
        save(&dir, 0, 5, b"abc").unwrap();
        dir.failed(save(&dir, 3, 5, b"def"), &[]);
        // An offset whose end does not fit a u64 is an overrun too.
        save(&dir, 0, 5, b"abc").unwrap();
        dir.failed(save(&dir, u64::MAX, 5, b"def"), &[]);
    }

    #[test]
    fn an_empty_part_before_the_end_and_an_empty_export_fail() {
        let dir = Dir::new("empty");
        save(&dir, 0, 5, b"abc").unwrap();
        dir.failed(save(&dir, 3, 5, b""), &[]);
        dir.failed(save(&dir, 0, 0, b""), &[]);
    }

    #[test]
    fn a_refused_name_touches_nothing() {
        let dir = Dir::new("names");
        let before = dir.path("keep.zip.part");
        fs::write(&before, b"kept").unwrap();
        for name in [
            ".",
            "..",
            "../up.zip",
            "..\\up.zip",
            "sub/a.zip",
            "sub\\a.zip",
            "C:a.zip",
            "a.zip:stream.zip",
            "a\nb.zip",
            "a\0b.zip",
            "a..b.zip",
            "a.wav",
            "a.zip.exe",
            "",
        ] {
            let error = save_part(&dir.0, name, 0, 3, b"abc").expect_err(name);
            assert!(!error.contains(dir.0.to_str().unwrap()), "{error}");
            assert_eq!(dir.names(), ["keep.zip.part"], "{name:?}");
        }
    }

    #[test]
    fn an_existing_export_keeps_its_bytes_and_the_new_one_takes_a_free_name() {
        let dir = Dir::new("free");
        fs::write(dir.path(NAME), b"first").unwrap();
        let second = dir.path("bleeploop-2026-10-31-1234 (2).zip");
        assert_eq!(save(&dir, 0, 3, b"two"), Ok(Some(second.clone())));
        assert_eq!(save(&dir, 0, 5, b"three"), Ok(Some(dir.path("bleeploop-2026-10-31-1234 (3).zip"))));
        assert_eq!(fs::read(dir.path(NAME)).unwrap(), b"first");
        assert_eq!(fs::read(second).unwrap(), b"two");
    }

    #[test]
    fn no_free_name_fails_and_removes_the_part_file() {
        let dir = Dir::new("full");
        fs::write(dir.path(NAME), b"").unwrap();
        for n in 2..=FREE_NAMES {
            fs::write(dir.path(&format!("bleeploop-2026-10-31-1234 ({n}).zip")), b"").unwrap();
        }
        let before = dir.names();
        let left: Vec<&str> = before.iter().map(String::as_str).collect();
        dir.failed(save(&dir, 0, 3, b"abc"), &left);
    }

    #[test]
    fn a_failed_rename_gives_the_claimed_name_back() {
        let dir = Dir::new("rename");
        let error = land(&dir.0, "a", &dir.path("no-such.part")).expect_err("no part file to rename");
        assert!(!error.contains(dir.0.to_str().unwrap()), "{error}");
        assert_eq!(dir.names(), Vec::<String>::new());
    }

    #[test]
    fn a_leftover_part_is_replaced_at_offset_zero() {
        let dir = Dir::new("leftover");
        fs::write(dir.path(&format!("{NAME}.part")), b"a crashed export's bytes").unwrap();
        assert_eq!(save(&dir, 0, 3, b"abc"), Ok(Some(dir.path(NAME))));
        assert_eq!(fs::read(dir.path(NAME)).unwrap(), b"abc");
    }
}
