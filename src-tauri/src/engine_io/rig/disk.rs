//! The rig's files in the app-local data folder: [`lf_rig::Disk`] over the real file system (D8). The
//! record and the crash marker are written as the tone store writes its files (`host/tone.rs`
//! `write_atomic`: beside the file, synced, renamed over it), and one process owns the pair through an
//! exclusive lock on `rig[.dev].lock` (`File::try_lock`), held until this `Disk` drops: at close, or with
//! the process (a crash releases it).

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::PathBuf;

use lf_rig::{Disk, Files, DEV_FILES, RELEASE_FILES};

use crate::host::tone::write_atomic;

/// This build's files: a `debug_assertions` build keeps its own pair and lock beside the release's,
/// since `tauri dev` and the release share the data folder (Settled 6).
pub(crate) fn files() -> Files {
    if cfg!(debug_assertions) {
        DEV_FILES
    } else {
        RELEASE_FILES
    }
}

/// The data folder as the store reads and writes it.
pub(crate) struct FileDisk {
    /// `None` without an app-local data folder: the lock is refused, so the store is `unavailable`.
    dir: Option<PathBuf>,
    /// The lock file, held exclusively while this `Disk` lives.
    lock: Option<File>,
}

impl FileDisk {
    pub(crate) fn new(dir: Option<PathBuf>) -> FileDisk {
        FileDisk { dir, lock: None }
    }

    fn path(&self, name: &str) -> io::Result<PathBuf> {
        self.dir.as_ref().map(|dir| dir.join(name)).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no app-local data folder"))
    }
}

impl Disk for FileDisk {
    fn read(&mut self, name: &str) -> io::Result<Option<Vec<u8>>> {
        match std::fs::read(self.path(name)?) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn write_atomic(&mut self, name: &str, bytes: &[u8]) -> io::Result<()> {
        write_atomic(&self.path(name)?, bytes).map_err(io::Error::other)
    }

    fn remove(&mut self, name: &str) -> io::Result<()> {
        match std::fs::remove_file(self.path(name)?) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    fn try_lock(&mut self, name: &str) -> io::Result<()> {
        let path = self.path(name)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path)?;
        file.try_lock().map_err(|e| match e {
            TryLockError::WouldBlock => io::Error::new(io::ErrorKind::WouldBlock, format!("{} is held by another process", path.display())),
            TryLockError::Error(e) => e,
        })?;
        self.lock = Some(file);
        Ok(())
    }
}
