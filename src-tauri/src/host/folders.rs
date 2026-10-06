//! The player's own plugin folders: `plugin-folders.json` beside the scan cache, the folders the
//! scan walks after its built-in roots (`scan::builtin_roots`), and the native dialog that picks one.
//!
//! This file is USER DATA, not a cache: only a MISSING file is an empty list. One that cannot be
//! read or parsed, or that a newer build wrote, is an error that reaches the caller, and since every
//! mutation loads the list first, no add or remove ever writes over such a file. A mutation holds
//! the host state's `folders_write` lock from its read to its rename; nothing is kept in memory, so
//! a reader (the scan, `plugin_folders`) sees a change only once it is on disk, and reads without the
//! lock (the rename is atomic). The lock is never held while the dialog is open or a scan child runs.

use super::scan::builtin_roots;
use super::state::{PluginFolder, PluginFolders, UnsupportedPlugin};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Mutex, PoisonError};

/// Bump when the shape changes; a file of any other version is refused, never rewritten.
const FOLDERS_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct FoldersFile {
    version: u32,
    folders: Vec<String>,
}

/// The stored folders, in the order they were added: `Ok(empty)` when there is no file yet, `Err`
/// when the file is there and cannot be used.
pub(crate) fn load(file: &Path) -> Result<Vec<String>, String> {
    let bytes = match std::fs::read(file) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", file.display())),
    };
    let stored: FoldersFile =
        serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", file.display()))?;
    if stored.version != FOLDERS_VERSION {
        return Err(format!(
            "{}: version {} (this build reads version {FOLDERS_VERSION})",
            file.display(),
            stored.version
        ));
    }
    Ok(stored.folders)
}

fn save(file: &Path, folders: &[String]) -> Result<(), String> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let stored = FoldersFile {
        version: FOLDERS_VERSION,
        folders: folders.to_vec(),
    };
    let json = serde_json::to_vec_pretty(&stored).map_err(|e| format!("serialize: {e}"))?;
    super::tone::write_atomic(file, &json)
}

/// What two spellings of one place share: the canonical path, lowercased (Windows paths compare
/// without case). A path that does not resolve (a folder that is gone) keeps its own spelling,
/// lowercased. The scan de-duplicates what it finds on the same key. Never store or show a key.
pub(crate) fn path_key(path: &Path) -> String {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    resolved.to_string_lossy().to_lowercase()
}

/// Add `folder`, spelled as the dialog returned it, unless the list already holds that place
/// (`path_key`). Answers the list as it is on disk afterwards.
pub(crate) fn add(write: &Mutex<()>, file: &Path, folder: &str) -> Result<Vec<String>, String> {
    let _write = write.lock().unwrap_or_else(PoisonError::into_inner);
    let mut folders = load(file)?;
    let key = path_key(Path::new(folder));
    if folders.iter().any(|f| path_key(Path::new(f)) == key) {
        return Ok(folders);
    }
    folders.push(folder.to_string());
    save(file, &folders)?;
    Ok(folders)
}

/// Remove the entry equal to `folder`, byte for byte (the UI sends back what it was shown). A
/// folder that is not in the list changes nothing and writes nothing.
pub(crate) fn remove(write: &Mutex<()>, file: &Path, folder: &str) -> Result<Vec<String>, String> {
    let _write = write.lock().unwrap_or_else(PoisonError::into_inner);
    let mut folders = load(file)?;
    let before = folders.len();
    folders.retain(|f| f != folder);
    if folders.len() != before {
        save(file, &folders)?;
    }
    Ok(folders)
}

/// What Audio Settings lists: the scan's built-in roots (read-only) and the stored `user` folders,
/// each with whether it is there now, and `unsupported`, what the last scan found and cannot host.
pub(crate) fn view(user: &[String], unsupported: Vec<UnsupportedPlugin>) -> PluginFolders {
    let folder = |path: String| PluginFolder {
        exists: Path::new(&path).is_dir(),
        path,
    };
    PluginFolders {
        builtin: builtin_roots()
            .into_iter()
            .map(|root| folder(root.path.to_string_lossy().into_owned()))
            .collect(),
        user: user.iter().cloned().map(folder).collect(),
        unsupported,
    }
}

/// The native folder dialog, modal over `owner`: `Ok(None)` when the player cancels. Call it ON THE
/// UI THREAD, which owns `owner` (the main window): a dialog owned across threads is the editor-hang
/// deadlock (`editor_window.rs`). It returns when the dialog closes; the caller holds no lock.
pub(crate) fn pick_folder(owner: windows::Win32::Foundation::HWND) -> Result<Option<String>, String> {
    use windows::Win32::Foundation::ERROR_CANCELLED;
    use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::Shell::{
        FileOpenDialog, IFileOpenDialog, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, SIGDN_FILESYSPATH,
    };
    // SAFETY: plain COM calls on the UI thread, whose apartment the window runtime initialised;
    // the one out-string is copied and then freed with the allocator that made it.
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| format!("folder dialog: {e}"))?;
        let options = dialog.GetOptions().map_err(|e| format!("folder dialog options: {e}"))?;
        dialog
            .SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM)
            .map_err(|e| format!("folder dialog options: {e}"))?;
        if let Err(e) = dialog.Show(Some(owner)) {
            return if e.code() == ERROR_CANCELLED.to_hresult() {
                Ok(None)
            } else {
                Err(format!("folder dialog: {e}"))
            };
        }
        let item = dialog.GetResult().map_err(|e| format!("folder dialog result: {e}"))?;
        let name = item
            .GetDisplayName(SIGDN_FILESYSPATH)
            .map_err(|e| format!("folder dialog path: {e}"))?;
        let path = name.to_string().map_err(|e| format!("folder dialog path: {e}"));
        CoTaskMemFree(Some(name.0 as *const _));
        path.map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    /// A scratch dir holding one `plugin-folders.json`, removed on drop.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "lf-folders-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn file(&self) -> PathBuf {
            self.0.join("plugin-folders.json")
        }
        /// A real folder under the scratch dir, as the dialog would return one.
        fn folder(&self, name: &str) -> String {
            let dir = self.0.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            dir.to_string_lossy().into_owned()
        }
        /// Every file beside the list (a temp file a write left behind shows up here).
        fn files(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(&self.0)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_file())
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

    #[test]
    fn a_missing_file_is_an_empty_list_and_an_add_creates_it() {
        let scratch = Scratch::new("missing");
        let write = Mutex::new(());
        assert_eq!(load(&scratch.file()), Ok(Vec::new()));
        let a = scratch.folder("a");
        assert_eq!(add(&write, &scratch.file(), &a), Ok(vec![a.clone()]));
        assert_eq!(load(&scratch.file()), Ok(vec![a.clone()]), "the add is on disk");
        let stored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(scratch.file()).unwrap()).unwrap();
        assert_eq!(stored, serde_json::json!({ "version": 1, "folders": [a] }));
        assert_eq!(scratch.files(), vec!["plugin-folders.json"], "no temp file is left");
    }

    #[test]
    fn a_garbage_or_newer_file_is_an_error_and_no_mutation_writes_over_it() {
        let scratch = Scratch::new("refused");
        let write = Mutex::new(());
        let a = scratch.folder("a");
        let newer = serde_json::to_vec(&serde_json::json!({
            "version": FOLDERS_VERSION + 1,
            "folders": ["C:\\kept"],
            "later": true,
        }))
        .unwrap();
        for bytes in [b"{ not json".to_vec(), newer] {
            std::fs::write(scratch.file(), &bytes).unwrap();
            assert!(load(&scratch.file()).is_err());
            assert!(add(&write, &scratch.file(), &a).is_err());
            assert!(remove(&write, &scratch.file(), "C:\\kept").is_err());
            assert_eq!(std::fs::read(scratch.file()).unwrap(), bytes, "the file is left as it was");
            assert_eq!(scratch.files(), vec!["plugin-folders.json"]);
        }
    }

    #[test]
    fn an_add_keeps_the_dialogs_spelling_and_refuses_the_same_place_twice() {
        let scratch = Scratch::new("dedupe");
        let write = Mutex::new(());
        let a = scratch.folder("Plugins");
        let b = scratch.folder("Other");
        assert_eq!(add(&write, &scratch.file(), &a), Ok(vec![a.clone()]));
        // The same place in another case, and with a trailing separator and a `.` step.
        assert_eq!(add(&write, &scratch.file(), &a.to_uppercase()), Ok(vec![a.clone()]));
        assert_eq!(add(&write, &scratch.file(), &format!("{a}\\.\\")), Ok(vec![a.clone()]));
        assert_eq!(add(&write, &scratch.file(), &b), Ok(vec![a.clone(), b.clone()]));
        // A folder that is gone still compares without case.
        let gone = scratch.0.join("Gone").to_string_lossy().into_owned();
        assert_eq!(add(&write, &scratch.file(), &gone).unwrap().len(), 3);
        assert_eq!(add(&write, &scratch.file(), &gone.to_lowercase()).unwrap().len(), 3);
        assert_eq!(load(&scratch.file()), Ok(vec![a, b, gone]));
    }

    #[test]
    fn a_remove_takes_only_the_entry_equal_to_a_stored_one() {
        let scratch = Scratch::new("remove");
        let write = Mutex::new(());
        let a = scratch.folder("a");
        let b = scratch.folder("b");
        add(&write, &scratch.file(), &a).unwrap();
        add(&write, &scratch.file(), &b).unwrap();
        let before = std::fs::read(scratch.file()).unwrap();
        assert_eq!(
            remove(&write, &scratch.file(), &a.to_uppercase()),
            Ok(vec![a.clone(), b.clone()]),
            "another spelling of a stored folder is not that entry"
        );
        assert_eq!(std::fs::read(scratch.file()).unwrap(), before);
        assert_eq!(remove(&write, &scratch.file(), &a), Ok(vec![b.clone()]));
        assert_eq!(load(&scratch.file()), Ok(vec![b]));
    }

    /// The list is held open without delete sharing, so the rename over it is refused while reads
    /// still work: a write that fails at its last step.
    #[test]
    fn a_failed_write_leaves_the_old_file_and_no_temp_file() {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 1;
        let scratch = Scratch::new("failed");
        let write = Mutex::new(());
        let a = scratch.folder("a");
        let b = scratch.folder("b");
        add(&write, &scratch.file(), &a).unwrap();
        let before = std::fs::read(scratch.file()).unwrap();
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(scratch.file())
            .unwrap();
        assert!(add(&write, &scratch.file(), &b).is_err(), "the rename must be refused");
        assert!(remove(&write, &scratch.file(), &a).is_err());
        assert_eq!(load(&scratch.file()), Ok(vec![a.clone()]), "readers still see the old list");
        assert_eq!(std::fs::read(scratch.file()).unwrap(), before);
        assert_eq!(scratch.files(), vec!["plugin-folders.json"], "the temp file is removed");
        drop(held);
        assert_eq!(add(&write, &scratch.file(), &b), Ok(vec![a, b]));
    }

    #[test]
    fn the_view_marks_a_folder_that_is_gone() {
        let scratch = Scratch::new("view");
        let here = scratch.folder("here");
        let gone = scratch.0.join("gone").to_string_lossy().into_owned();
        let narrow = UnsupportedPlugin { path: r"C:\VST\Old.dll".into(), reason: "32-bit".into() };
        let view = view(&[here.clone(), gone.clone()], vec![narrow.clone()]);
        let user: Vec<(String, bool)> = view.user.iter().map(|f| (f.path.clone(), f.exists)).collect();
        assert_eq!(user, vec![(here, true), (gone, false)]);
        assert_eq!(view.unsupported, vec![narrow]);
        // The built-in list ends with the VST2 roots, and the wire names are camelCase.
        let programs = std::env::var("ProgramFiles").unwrap();
        let vst2 = Path::new(&programs).join("VSTPlugins").to_string_lossy().into_owned();
        assert!(view.builtin.iter().any(|f| f.path == vst2));
        let wire = serde_json::to_value(&view).unwrap();
        assert_eq!(wire["unsupported"], serde_json::json!([{ "path": r"C:\VST\Old.dll", "reason": "32-bit" }]));
    }
}
