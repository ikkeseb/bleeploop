//! P9.1 CLAP scan. `scan_one` runs in the child process (loads one bundle via clack-host's
//! `PluginEntry::load` — API verified vs docs.rs/clack-host 0.1.0: the loader is `PluginEntry`, NOT
//! `PluginBundle`, and factory enumeration needs no `Host`). `scan_all` is the parent: it walks the
//! CLAP + VST3 search dirs and spawns one child per bundle, so a crashy bundle can't take down the
//! host — but only for bundles whose binary changed since the last scan: `ScanCache` remembers each
//! bundle's descriptors (or its failure) under a size + mtime fingerprint, so a launch spawns no
//! foreign code at all until a plugin is installed or updated, and a hung bundle costs its 20 s
//! timeout once, not once per launch. The picker's rescan button forces a fresh scan.

use super::state::PluginDescriptor;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

/// Per-bundle scan timeout. A `--scan-one` child loads the plugin's DLL + enumerates its factory;
/// a well-behaved plugin does this in well under a second. Neural DSP VST3s (~100 MB, licensing)
/// can occasionally HANG on load in a headless child — without a bound that would hang the whole
/// scan (and app startup) forever. On timeout the child is killed and the bundle marked `failed`.
const SCAN_CHILD_TIMEOUT: Duration = Duration::from_secs(20);
const SCAN_STDOUT_CAP: usize = 1024 * 1024;
const SCAN_STDERR_CAP: usize = 64 * 1024;

/// Owns a Windows Job Object that terminates every assigned process when dropped. Closing this
/// before joining the pipe readers guarantees a scan child cannot leave a descendant holding either
/// pipe open after the direct child exits or times out.
struct KillOnCloseJob(OwnedHandle);

impl KillOnCloseJob {
    fn new() -> Result<Self, String> {
        // SAFETY: null security attributes/name create a private job owned by this process.
        let handle = unsafe { CreateJobObjectW(None, PCWSTR::null()) }
            .map_err(|e| format!("create scan job: {e}"))?;
        // SAFETY: CreateJobObjectW returned a new owned handle. OwnedHandle closes it on every
        // return path, which also enforces the configured kill-on-close policy.
        let job = Self(unsafe { OwnedHandle::from_raw_handle(handle.0) });
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `limits` has the exact structure and byte count requested by the information
        // class, and the owned job handle remains live for the call.
        unsafe {
            SetInformationJobObject(
                HANDLE(job.0.as_raw_handle()),
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        }
        .map_err(|e| format!("configure scan job: {e}"))?;
        Ok(job)
    }

    fn assign(&self, child: &Child) -> Result<(), String> {
        // SAFETY: both handles are live for the call; the Child retains ownership of its process
        // handle and this wrapper retains ownership of the Job Object handle.
        unsafe {
            AssignProcessToJobObject(
                HANDLE(self.0.as_raw_handle()),
                HANDLE(child.as_raw_handle()),
            )
        }
        .map_err(|e| format!("assign scan child to job: {e}"))
    }
}

/// Drain a pipe to EOF so the child never blocks on a full pipe, while retaining at most `cap`
/// bytes in the parent process.
fn drain_capped(mut reader: impl Read, cap: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(cap.min(8192));
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let keep = cap.saturating_sub(out.len()).min(n);
                out.extend_from_slice(&chunk[..keep]);
            }
        }
    }
    out
}

/// Why one bundle produced no descriptors. `Bundle` is the plugin's own doing (crash, non-zero
/// exit, timeout, unparsable output) and is cached under its fingerprint so it is not retried every
/// launch; `Host` is this process failing to run a child at all (spawn, job object) and is never
/// cached — the next launch may well succeed.
#[derive(Debug)]
enum ScanError {
    Bundle(String),
    Host(String),
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScanError::Bundle(e) | ScanError::Host(e) => f.write_str(e),
        }
    }
}

/// The identity of a bundle's loadable binary: a plugin update changes its size or mtime (in
/// practice both). A folder bundle is keyed on its inner DLL, the file the loader actually maps.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
struct Fingerprint {
    len: u64,
    modified_unix_ns: u64,
}

impl Fingerprint {
    fn of(outer: &Path) -> Option<Self> {
        let binary = if is_vst3(outer) {
            resolve_vst3_binary(outer)?
        } else {
            outer.to_path_buf()
        };
        let meta = std::fs::metadata(binary).ok()?;
        let modified = meta.modified().ok()?;
        let modified_unix_ns = modified
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos()
            .try_into()
            .ok()?;
        Some(Self {
            len: meta.len(),
            modified_unix_ns,
        })
    }
}

/// What the last scan learned about one bundle at one fingerprint.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct CacheEntry {
    fingerprint: Fingerprint,
    /// `Ok` = its descriptors (possibly none: a module with no loadable class); `Err` = the
    /// bundle-side failure the scan hit. A forced rescan retries the latter.
    outcome: Result<Vec<PluginDescriptor>, String>,
}

/// Bump when the entry shape or the descriptor fields change: an old file is discarded whole.
const SCAN_CACHE_VERSION: u32 = 1;

/// The on-disk scan memory, keyed by the bundle's outer path (what the descriptor carries). Only
/// bundles present in the current walk are written back, so removed plugins prune themselves.
#[derive(Serialize, Deserialize, Default)]
struct ScanCache {
    version: u32,
    bundles: BTreeMap<String, CacheEntry>,
}

impl ScanCache {
    /// A missing, unreadable, unparsable or older-version file is an empty cache (logged, not an
    /// error): the scan then simply runs in full, as it did before the cache existed.
    fn load(path: &Path) -> Self {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                log::warn!("[scan] cache unreadable ({}): {e}; scanning everything", path.display());
                return Self::default();
            }
        };
        match serde_json::from_slice::<ScanCache>(&bytes) {
            Ok(c) if c.version == SCAN_CACHE_VERSION => c,
            Ok(c) => {
                log::info!("[scan] cache version {} ≠ {SCAN_CACHE_VERSION}; scanning everything", c.version);
                Self::default()
            }
            Err(e) => {
                log::warn!("[scan] cache unparsable ({}): {e}; scanning everything", path.display());
                Self::default()
            }
        }
    }

    /// Atomic replace (write a sibling temp file, then rename over) so a crash mid-write leaves
    /// the previous cache intact rather than a truncated one.
    fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_vec(self).map_err(|e| format!("serialize: {e}"))?;
        std::fs::write(&tmp, json).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("rename to {}: {e}", path.display()))
    }
}

/// What one scan did, for the log line and the DEV gate.
#[derive(Default, Debug, PartialEq, Eq)]
struct ScanStats {
    scanned: usize,
    cached: usize,
    fresh: usize,
    failed: Vec<String>,
}

/// The cache-aware core of `scan_all`, with the child runner injected so the cache logic is testable
/// without a plugin. `previous` is consulted unless `force`; the returned cache holds exactly the
/// bundles in `files` (reused or freshly scanned), ready to be written back.
fn scan_bundles(
    files: &[PathBuf],
    previous: &ScanCache,
    force: bool,
    mut scan: impl FnMut(&Path) -> Result<Vec<PluginDescriptor>, ScanError>,
) -> (Vec<PluginDescriptor>, ScanCache, ScanStats) {
    let mut ok: Vec<PluginDescriptor> = Vec::new();
    let mut next = ScanCache {
        version: SCAN_CACHE_VERSION,
        bundles: BTreeMap::new(),
    };
    let mut stats = ScanStats {
        scanned: files.len(),
        ..ScanStats::default()
    };
    for path in files {
        let key = path.to_string_lossy().to_string();
        let fingerprint = Fingerprint::of(path);
        let reusable = if force {
            None
        } else {
            previous
                .bundles
                .get(&key)
                .filter(|entry| Some(entry.fingerprint) == fingerprint)
        };
        let entry = match reusable {
            Some(entry) => {
                stats.cached += 1;
                entry.clone()
            }
            None => {
                stats.fresh += 1;
                let outcome = match scan(path) {
                    Ok(descs) => Ok(descs),
                    Err(ScanError::Bundle(e)) => {
                        log::warn!("[scan] child failed for {key}: {e}");
                        Err(e)
                    }
                    Err(ScanError::Host(e)) => {
                        // Not the bundle's fault: report it, remember nothing for it.
                        log::warn!("[scan] could not run a scan child for {key}: {e}");
                        stats.failed.push(key.clone());
                        continue;
                    }
                };
                match fingerprint {
                    Some(fingerprint) => CacheEntry {
                        fingerprint,
                        outcome,
                    },
                    None => {
                        // Unstat-able bundle: use the outcome, do not remember it.
                        match outcome {
                            Ok(mut descs) => ok.append(&mut descs),
                            Err(_) => stats.failed.push(key.clone()),
                        }
                        continue;
                    }
                }
            }
        };
        match &entry.outcome {
            Ok(descs) => ok.extend(descs.iter().cloned()),
            Err(_) => stats.failed.push(key.clone()),
        }
        next.bundles.insert(key, entry);
    }
    (ok, next, stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const FIXTURE_CHILD_ENV: &str = "LF_SCAN_FIXTURE_CHILD";

    /// The child half of `a_scan_childs_grandchild_dies_with_the_job`: this test exe re-invoked
    /// through the production gate. A no-op in a normal (or `--ignored`) run.
    #[test]
    #[ignore = "child half of a_scan_childs_grandchild_dies_with_the_job"]
    fn scan_gate_fixture_child() {
        if std::env::var_os(FIXTURE_CHILD_ENV).is_none() {
            return;
        }
        await_scan_gate().expect("the parent releases the gate");
        // Stand-in for plugin code that starts a helper process and leaves it running. It
        // inherits this process's stdout, i.e. the parent's pipe, for ~30 s.
        let grandchild = Command::new("ping")
            .args(["-n", "31", "127.0.0.1"])
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn the grandchild");
        println!("grandchild={}", grandchild.id());
    }

    fn process_exits_within(pid: u32, ms: u32) -> bool {
        use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
        // SAFETY: plain handle open/wait/close on a pid; the handle is closed on every path.
        unsafe {
            let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) else {
                return true; // no such process any more
            };
            let exited = WaitForSingleObject(h, ms) == WAIT_OBJECT_0;
            let _ = CloseHandle(h);
            exited
        }
    }

    /// Audit B9: the scan child waits at the gate until it sits in the kill-on-close Job, so a
    /// process it starts is in the Job too. Closing the Job after the child exits kills the
    /// grandchild, which releases the stdout pipe it inherited — the scan returns at once instead of
    /// waiting ~30 s for the grandchild (or 20 s for the timeout).
    #[test]
    fn a_scan_childs_grandchild_dies_with_the_job() {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            "host::scan::tests::scan_gate_fixture_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(FIXTURE_CHILD_ENV, "1");
        let started = Instant::now();
        let out = run_gated_child(cmd, Duration::from_secs(20))
            .unwrap_or_else(|e| panic!("the fixture child must exit cleanly: {e}"));
        let elapsed = started.elapsed();
        let text = String::from_utf8_lossy(&out);
        let pid: u32 = text
            .split("grandchild=")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|p| p.parse().ok())
            .unwrap_or_else(|| panic!("no grandchild pid in the child's output: {text}"));
        assert!(
            elapsed < Duration::from_secs(15),
            "the grandchild held the pipe: the scan took {elapsed:?}"
        );
        assert!(
            process_exits_within(pid, 5_000),
            "the grandchild {pid} outlived the Job"
        );
    }

    #[test]
    fn capped_drain_keeps_prefix_and_consumes_to_eof() {
        let bytes: Vec<u8> = (0u8..=255).cycle().take(20_000).collect();
        let mut input = Cursor::new(bytes);
        let kept = drain_capped(&mut input, 1024);

        assert_eq!(kept, input.get_ref()[..1024]);
        assert_eq!(input.position(), 20_000);
    }

    /// A scratch dir with fake bundle files (their bytes are the fingerprint, not plugin code).
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "lf-scan-cache-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn bundle(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let p = self.0.join(name);
            std::fs::write(&p, bytes).unwrap();
            p
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn desc(path: &Path, name: &str) -> PluginDescriptor {
        PluginDescriptor {
            id: format!("id-{name}"),
            name: name.to_string(),
            format: "clap".to_string(),
            path: path.to_string_lossy().to_string(),
            is_effect: Some(false),
        }
    }

    #[test]
    fn cache_reuses_unchanged_bundles_and_rescans_changed_or_forced_ones() {
        let scratch = Scratch::new("reuse");
        let good = scratch.bundle("good.clap", b"v1");
        let bad = scratch.bundle("bad.clap", b"v1");
        let files = vec![good.clone(), bad.clone()];
        let children = AtomicUsize::new(0);
        let scan = |p: &Path| {
            children.fetch_add(1, Ordering::Relaxed);
            if p == bad {
                Err(ScanError::Bundle("timed out".into()))
            } else {
                Ok(vec![desc(p, "Good")])
            }
        };

        // First launch: everything is scanned, the failure is remembered too.
        let (descs, cache, stats) = scan_bundles(&files, &ScanCache::default(), false, scan);
        assert_eq!(descs.len(), 1);
        assert_eq!(children.load(Ordering::Relaxed), 2);
        assert_eq!(stats.cached, 0);
        assert_eq!(stats.fresh, 2);
        assert_eq!(stats.failed, vec![bad.to_string_lossy().to_string()]);
        assert!(cache.bundles[&bad.to_string_lossy().to_string()].outcome.is_err());

        // Second launch: no child runs; the same descriptors and the same failure come from memory.
        let (descs2, cache2, stats2) = scan_bundles(&files, &cache, false, scan);
        assert_eq!(descs2, descs);
        assert_eq!(children.load(Ordering::Relaxed), 2, "an unchanged bundle spawns no child");
        assert_eq!(stats2.cached, 2);
        assert_eq!(stats2.fresh, 0);
        assert_eq!(stats2.failed, stats.failed);

        // The bad plugin was updated (bytes changed → size differs): only it is rescanned.
        std::fs::write(&bad, b"v2-fixed").unwrap();
        let (_, cache3, stats3) = scan_bundles(&files, &cache2, false, |p: &Path| {
            children.fetch_add(1, Ordering::Relaxed);
            Ok(vec![desc(p, "Fixed")])
        });
        assert_eq!(children.load(Ordering::Relaxed), 3);
        assert_eq!((stats3.cached, stats3.fresh), (1, 1));
        assert!(stats3.failed.is_empty());
        assert!(cache3.bundles[&bad.to_string_lossy().to_string()].outcome.is_ok());

        // The rescan button: everything is scanned again regardless of fingerprints.
        let (_, _, stats4) = scan_bundles(&files, &cache3, true, |p: &Path| {
            children.fetch_add(1, Ordering::Relaxed);
            Ok(vec![desc(p, "Forced")])
        });
        assert_eq!(children.load(Ordering::Relaxed), 5);
        assert_eq!((stats4.cached, stats4.fresh), (0, 2));
    }

    #[test]
    fn host_side_failures_and_removed_bundles_are_not_remembered() {
        let scratch = Scratch::new("host");
        let a = scratch.bundle("a.clap", b"a");
        let b = scratch.bundle("b.clap", b"b");
        let (descs, cache, stats) =
            scan_bundles(&[a.clone(), b.clone()], &ScanCache::default(), false, |p: &Path| {
                if p == a {
                    Err(ScanError::Host("spawn: exe missing".into()))
                } else {
                    Ok(vec![desc(p, "B")])
                }
            });
        assert_eq!(descs.len(), 1);
        assert_eq!(stats.failed, vec![a.to_string_lossy().to_string()]);
        assert!(
            !cache.bundles.contains_key(&a.to_string_lossy().to_string()),
            "a host-side failure must be retried next launch"
        );
        // `b` uninstalled: the written-back cache holds only what the walk found.
        let (_, cache2, _) = scan_bundles(&[a.clone()], &cache, false, |p: &Path| Ok(vec![desc(p, "A")]));
        assert_eq!(cache2.bundles.len(), 1);
        assert!(cache2.bundles.contains_key(&a.to_string_lossy().to_string()));
    }

    #[test]
    fn cache_file_round_trips_and_a_foreign_version_or_garbage_is_ignored() {
        let scratch = Scratch::new("file");
        let bundle = scratch.bundle("x.clap", b"x");
        let path = scratch.0.join("plugin-scan.json");
        let (_, cache, _) = scan_bundles(&[bundle.clone()], &ScanCache::default(), false, |p: &Path| {
            Ok(vec![desc(p, "X")])
        });
        cache.save(&path).unwrap();
        let loaded = ScanCache::load(&path);
        assert_eq!(loaded.bundles.len(), 1);
        assert_eq!(
            loaded.bundles[&bundle.to_string_lossy().to_string()].fingerprint,
            Fingerprint::of(&bundle).unwrap()
        );
        assert!(!path.with_extension("json.tmp").exists(), "the temp file was renamed over");

        std::fs::write(&path, b"{ not json").unwrap();
        assert!(ScanCache::load(&path).bundles.is_empty());
        let stale = ScanCache {
            version: SCAN_CACHE_VERSION + 1,
            bundles: cache.bundles.clone(),
        };
        std::fs::write(&path, serde_json::to_vec(&stale).unwrap()).unwrap();
        assert!(ScanCache::load(&path).bundles.is_empty());
        assert!(ScanCache::load(&scratch.0.join("missing.json")).bundles.is_empty());
    }
}

/// `.vst3` by extension (either form); everything else scans as CLAP.
fn is_vst3(path: &Path) -> bool {
    path.extension()
        .map(|e| e.eq_ignore_ascii_case("vst3"))
        .unwrap_or(false)
}

/// CLAP search paths (Windows): `%COMMONPROGRAMFILES%\CLAP`,
/// `%LOCALAPPDATA%\Programs\Common\CLAP`, and every `;`-separated entry of `CLAP_PATH`.
fn search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(cpf) = std::env::var("CommonProgramFiles") {
        dirs.push(Path::new(&cpf).join("CLAP"));
    }
    if let Ok(lad) = std::env::var("LOCALAPPDATA") {
        dirs.push(Path::new(&lad).join("Programs").join("Common").join("CLAP"));
    }
    if let Ok(cp) = std::env::var("CLAP_PATH") {
        for p in cp.split(';').filter(|s| !s.is_empty()) {
            dirs.push(PathBuf::from(p));
        }
    }
    dirs
}

/// Recursively find every `*.clap` under the search dirs (Surge installs to a nested
/// `CLAP\Surge Synth Team\` subdir, so the walk must recurse).
fn find_clap_files() -> Vec<PathBuf> {
    let mut found = Vec::new();
    for dir in search_dirs() {
        if !dir.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&dir).into_iter().filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.extension()
                .map(|e| e.eq_ignore_ascii_case("clap"))
                .unwrap_or(false)
            {
                found.push(p.to_path_buf());
            }
        }
    }
    found
}

/// VST3 search paths (Windows): `%COMMONPROGRAMFILES%\VST3`,
/// `%LOCALAPPDATA%\Programs\Common\VST3`, and every `;`-separated entry of `VST3_PATH`.
fn vst3_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(cpf) = std::env::var("CommonProgramFiles") {
        dirs.push(Path::new(&cpf).join("VST3"));
    }
    if let Ok(lad) = std::env::var("LOCALAPPDATA") {
        dirs.push(Path::new(&lad).join("Programs").join("Common").join("VST3"));
    }
    if let Ok(vp) = std::env::var("VST3_PATH") {
        for p in vp.split(';').filter(|s| !s.is_empty()) {
            dirs.push(PathBuf::from(p));
        }
    }
    dirs
}

/// Find every `*.vst3` entry under the VST3 search dirs. On Windows a `.vst3` is EITHER a folder
/// bundle (`Name.vst3/Contents/x86_64-win/Inner.vst3` — e.g. Surge XT, often under a vendor
/// subdir) OR a single DLL file with a `.vst3` extension (e.g. Neural DSP, ~100 MB). We collect
/// the OUTER path for both forms; `resolve_vst3_binary` later resolves the loadable DLL. We must
/// NOT also yield the inner DLL inside a bundle (its path ends `.vst3` too) — so we drop any
/// match that has a `.vst3` ancestor directory. Verified against the on-PC install layout.
fn find_vst3_paths() -> Vec<PathBuf> {
    let mut found = Vec::new();
    for dir in vst3_search_dirs() {
        if !dir.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&dir).into_iter().filter_map(|e| e.ok()) {
            let p = entry.path();
            let is_vst3 = p
                .extension()
                .map(|e| e.eq_ignore_ascii_case("vst3"))
                .unwrap_or(false);
            if !is_vst3 {
                continue;
            }
            // Skip the inner DLL inside a bundle: an ancestor ending in `.vst3` means this entry
            // is `<bundle>.vst3/Contents/x86_64-win/<inner>.vst3`, not a top-level plugin.
            let inside_bundle = p.ancestors().skip(1).any(|a| {
                a.extension()
                    .map(|e| e.eq_ignore_ascii_case("vst3"))
                    .unwrap_or(false)
            });
            if !inside_bundle {
                found.push(p.to_path_buf());
            }
        }
    }
    found
}

/// Resolve a top-level `.vst3` path to its loadable DLL. Single-file form → the file itself;
/// folder-bundle form → the `.vst3` DLL under `Contents/x86_64-win/`. Pub: the VST3 loader
/// (`clap::vst3_host`) reuses it to resolve the binary at load time.
pub fn resolve_vst3_binary(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    if path.is_dir() {
        let arch_dir = path.join("Contents").join("x86_64-win");
        if arch_dir.is_dir() {
            for entry in std::fs::read_dir(&arch_dir).ok()?.filter_map(|e| e.ok()) {
                let p = entry.path();
                let is_vst3 = p
                    .extension()
                    .map(|e| e.eq_ignore_ascii_case("vst3"))
                    .unwrap_or(false);
                if is_vst3 && p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    None
}

/// CHILD side: load ONE plugin (`.clap` OR `.vst3`) and return its exported descriptors. Routes
/// by extension; both forms run in the throwaway `--scan-one` child process for crash isolation.
pub fn scan_one(path: &str) -> Result<Vec<PluginDescriptor>, String> {
    if is_vst3(Path::new(path)) {
        scan_one_vst3(path)
    } else {
        scan_one_clap(path)
    }
}

/// CHILD side: load ONE `.clap` and return its exported descriptors.
fn scan_one_clap(path: &str) -> Result<Vec<PluginDescriptor>, String> {
    use clack_host::entry::PluginEntry;
    // SAFETY: loading a dylib runs the bundle's foreign CLAP entry-init code (per
    // PluginEntry::load's safety docs — even loading can trigger arbitrary behaviour).
    // Accepted because this runs in a throwaway child process; a bad bundle crashes only it.
    let entry = unsafe { PluginEntry::load(path) }.map_err(|e| format!("load failed: {e}"))?;
    let factory = entry
        .get_plugin_factory()
        .ok_or_else(|| "no plugin factory".to_string())?;
    // The &CStr fields borrow from `entry`; own them into Strings before `entry` drops.
    let out = factory
        .plugin_descriptors()
        .map(|d| {
            // Classify by CLAP feature tags (each yields a &CStr): "instrument" ⇒ synth,
            // "audio-effect" ⇒ FX. Neither ⇒ None (JS falls back to the input-bus count).
            let mut has_instrument = false;
            let mut has_audio_effect = false;
            for f in d.features() {
                match f.to_str() {
                    Ok("instrument") => has_instrument = true,
                    Ok("audio-effect") => has_audio_effect = true,
                    _ => {}
                }
            }
            let is_effect = if has_instrument {
                Some(false)
            } else if has_audio_effect {
                Some(true)
            } else {
                None
            };
            PluginDescriptor {
                id: cstr_owned(d.id()),
                name: cstr_owned(d.name()),
                format: "clap".to_string(),
                path: path.to_string(),
                is_effect,
            }
        })
        .collect();
    Ok(out)
}

fn cstr_owned(s: Option<&std::ffi::CStr>) -> String {
    s.map(|c| c.to_string_lossy().into_owned()).unwrap_or_default()
}

/// CHILD side: load ONE `.vst3` (file or bundle) and return its `Audio Module Class` descriptors.
/// Loads the module with the `windows` loader (the vst3 crate provides no loader), wraps the
/// `GetPluginFactory()` return in a `ComPtr`, and enumerates the factory's classes. The descriptor
/// `id` is the class TUID hex-encoded — a stable id `plugin_load` parses back to `createInstance`
/// the exact class. No `FreeLibrary`: the child process exits right after printing.
fn scan_one_vst3(path: &str) -> Result<Vec<PluginDescriptor>, String> {
    use std::os::windows::ffi::OsStrExt;
    use vst3::ComPtr;
    use vst3::Steinberg::{
        kResultOk, IPluginFactory, IPluginFactory2, IPluginFactory2Trait, IPluginFactoryTrait,
        PClassInfo, PClassInfo2,
    };
    use windows::core::{s, PCWSTR};
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

    let binary = resolve_vst3_binary(Path::new(path))
        .ok_or_else(|| format!("no loadable VST3 binary inside {path}"))?;
    let wide: Vec<u16> = binary
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: loading the module runs the plugin's foreign entry-init code (VST3 `InitDll`); we
    // FFI into the factory vtbl. Accepted because this runs in the throwaway `--scan-one` child
    // process — a bad bundle crashes only it (same isolation as the CLAP scan).
    unsafe {
        let hmod =
            LoadLibraryW(PCWSTR(wide.as_ptr())).map_err(|e| format!("LoadLibraryW: {e}"))?;
        // InitDll is the optional VST3 module entry; if present and it returns false, decline.
        if let Some(init_ptr) = GetProcAddress(hmod, s!("InitDll")) {
            let init: unsafe extern "system" fn() -> bool = std::mem::transmute(init_ptr);
            if !init() {
                return Err("InitDll returned false".to_string());
            }
        }
        let gpf = GetProcAddress(hmod, s!("GetPluginFactory"))
            .ok_or_else(|| "GetPluginFactory not exported".to_string())?;
        let get_factory: unsafe extern "system" fn() -> *mut IPluginFactory =
            std::mem::transmute(gpf);
        let factory = ComPtr::from_raw(get_factory())
            .ok_or_else(|| "GetPluginFactory returned null".to_string())?;

        let count = factory.countClasses();
        // IPluginFactory2 (when the factory implements it — most modern plugins) carries
        // `subCategories`, which classifies instrument vs Fx. Fall back to PClassInfo (no
        // subCategories ⇒ `is_effect = None`) for the rare factory that lacks it.
        let factory2: Option<ComPtr<IPluginFactory2>> = factory.cast();
        let mut out = Vec::new();
        for i in 0..count {
            // (cid, name, category, is_effect) — prefer getClassInfo2 for the subCategories.
            let (cid, name, category, is_effect) = if let Some(f2) = &factory2 {
                let mut info2: PClassInfo2 = std::mem::zeroed();
                if f2.getClassInfo2(i, &mut info2) != kResultOk {
                    continue;
                }
                let sub = c_chars_to_string(&info2.subCategories);
                // VST3 subCategories: "Instrument" ⇒ synth, "Fx" ⇒ effect; neither ⇒ None.
                let is_effect = if sub.contains("Instrument") {
                    Some(false)
                } else if sub.contains("Fx") {
                    Some(true)
                } else {
                    None
                };
                (
                    info2.cid,
                    c_chars_to_string(&info2.name),
                    c_chars_to_string(&info2.category),
                    is_effect,
                )
            } else {
                let mut info: PClassInfo = std::mem::zeroed();
                if factory.getClassInfo(i, &mut info) != kResultOk {
                    continue;
                }
                (
                    info.cid,
                    c_chars_to_string(&info.name),
                    c_chars_to_string(&info.category),
                    None,
                )
            };
            // Only "Audio Module Class" entries are loadable IComponents (skip controllers etc.).
            if category != "Audio Module Class" {
                continue;
            }
            out.push(PluginDescriptor {
                id: tuid_to_hex(&cid),
                name,
                format: "vst3".to_string(),
                path: path.to_string(),
                is_effect,
            });
        }
        Ok(out)
    }
}

/// A VST3 fixed C-char buffer (`[c_char; N]`, NUL-terminated) → owned String.
pub fn c_chars_to_string(buf: &[std::os::raw::c_char]) -> String {
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// VST3 class id (`TUID = [c_char; 16]`) → lowercase hex (the stable descriptor `id`).
pub fn tuid_to_hex(cid: &[std::os::raw::c_char; 16]) -> String {
    cid.iter().map(|&b| format!("{:02x}", b as u8)).collect()
}

/// Parse a hex descriptor `id` (from `tuid_to_hex`) back into a `TUID`. Used by the VST3 loader to
/// pick the exact class to `createInstance`. Returns `None` on a malformed id.
pub fn hex_to_tuid(hex: &str) -> Option<[std::os::raw::c_char; 16]> {
    if hex.len() != 32 {
        return None;
    }
    let mut out = [0 as std::os::raw::c_char; 16];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()? as std::os::raw::c_char;
    }
    Some(out)
}

/// Set by the parent on every `--scan-one` child it spawns: the child then waits for one byte on
/// stdin before it loads any foreign code (`await_scan_gate`). The parent sends it only after the
/// child sits in the kill-on-close Job, so nothing the plugin starts can escape the Job (audit B9).
/// A manual `app.exe --scan-one <path>` has no gate and runs straight through.
const SCAN_GATE_ENV: &str = "LF_SCAN_GATE";

/// Child side of the scan gate: with `SCAN_GATE_ENV` set, block until the parent's release byte.
/// EOF first (the parent gave up or failed to assign the Job) is an error: load nothing.
pub fn await_scan_gate() -> Result<(), String> {
    if std::env::var_os(SCAN_GATE_ENV).is_none() {
        return Ok(());
    }
    let mut byte = [0u8; 1];
    match std::io::stdin().read(&mut byte) {
        Ok(1) => Ok(()),
        Ok(_) => Err("scan gate closed before release".to_string()),
        Err(e) => Err(format!("scan gate read: {e}")),
    }
}

/// Run ONE `--scan-one` child with a hard timeout. Returns its stdout bytes on a clean exit, or
/// an error string (non-zero exit, crash, spawn failure, or timeout → child killed). The output
/// is drained concurrently with bounded retention. A kill-on-close Job Object contains descendants
/// as well as the direct child, so inherited pipe handles cannot strand the reader threads.
fn scan_one_child(exe: &Path, path: &Path, timeout: Duration) -> Result<Vec<u8>, ScanError> {
    let mut cmd = Command::new(exe);
    cmd.arg("--scan-one").arg(path);
    run_gated_child(cmd, timeout)
}

/// Spawn `cmd` gated (`SCAN_GATE_ENV`), put it in a fresh kill-on-close Job, THEN release it, and
/// collect its stdout under `timeout`. The child is our own exe, so it waits at the gate before any
/// foreign code runs: there is no window in which a plugin can start a process outside the Job.
fn run_gated_child(mut cmd: Command, timeout: Duration) -> Result<Vec<u8>, ScanError> {
    let job = KillOnCloseJob::new().map_err(ScanError::Host)?;
    let mut child = cmd
        .env(SCAN_GATE_ENV, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ScanError::Host(format!("spawn: {e}")))?;
    let released = job.assign(&child).and_then(|()| {
        // Dropping stdin after the byte closes it; the child reads exactly one byte.
        let mut stdin = child.stdin.take().ok_or("scan child has no stdin pipe")?;
        stdin
            .write_all(&[1])
            .map_err(|e| format!("release scan child: {e}"))
    });
    if let Err(e) = released {
        let _ = child.kill();
        let _ = child.wait();
        return Err(ScanError::Host(e));
    }
    // Drain BOTH pipes concurrently from dedicated reader threads while we poll for exit. The
    // child loads FOREIGN plugin init code (JUCE/VST3 factory enumeration), which can emit more
    // than the OS pipe buffer (~4-64KB) to stderr (or a large VST3 to stdout). If we read only
    // AFTER exit, a chatty plugin's write() blocks on a full pipe, the child never exits, and we
    // stall to the 20s timeout and wrongly mark a good plugin failed. Readers can't deadlock —
    // they read continuously — so the child always makes progress. (Bug-hunt 2026-06-21.)
    let stdout_reader = child
        .stdout
        .take()
        .map(|so| std::thread::spawn(move || drain_capped(so, SCAN_STDOUT_CAP)));
    let stderr_reader = child
        .stderr
        .take()
        .map(|se| std::thread::spawn(move || drain_capped(se, SCAN_STDERR_CAP)));
    let deadline = Instant::now() + timeout;
    // The loop only decides the exit OUTCOME; the reader threads above are joined once after it,
    // so the child handles are never moved inside the loop body.
    let outcome: Result<std::process::ExitStatus, String> = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    break Err(format!("timed out after {}s; killed process tree", timeout.as_secs()));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                break Err(format!("wait: {e}"));
            }
        }
    };
    // Close the job BEFORE joining readers: this terminates the direct child on errors and any
    // descendants on every path, closing inherited pipe handles. Kill/wait remains as a direct-child
    // fallback and reaper if the outcome was not a clean exit.
    drop(job);
    if outcome.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let out = stdout_reader.and_then(|h| h.join().ok()).unwrap_or_default();
    let err_bytes = stderr_reader.and_then(|h| h.join().ok()).unwrap_or_default();
    let err = String::from_utf8_lossy(&err_bytes);
    match outcome {
        Ok(status) if status.success() => Ok(out),
        Ok(status) => Err(ScanError::Bundle(format!(
            "exit={:?} stderr={}",
            status.code(),
            err.trim()
        ))),
        Err(e) => Err(ScanError::Bundle(e)),
    }
}

/// PARENT side: walk the search dirs, reuse the cache for every bundle whose binary is unchanged
/// (unless `force`), spawn `app.exe --scan-one <path>` for the rest, aggregate, write the cache
/// back. A bundle whose child exits non-zero (handled error OR hard crash), times out, or whose
/// stdout doesn't parse is logged as `failed` and remembered as such; the parent always survives.
/// `cache_path` None (no app data dir) = scan everything, remember nothing. Emits the P9.1 gate
/// diag in debug builds.
pub fn scan_all(cache_path: Option<&Path>, force: bool) -> Result<Vec<PluginDescriptor>, String> {
    let started = Instant::now();
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let mut files = find_clap_files();
    files.extend(find_vst3_paths());
    let previous = match cache_path {
        Some(p) if !force => ScanCache::load(p),
        _ => ScanCache::default(),
    };
    let (ok, next, stats) = scan_bundles(&files, &previous, force, |path| {
        let stdout = scan_one_child(&exe, path, SCAN_CHILD_TIMEOUT)?;
        serde_json::from_slice::<Vec<PluginDescriptor>>(&stdout)
            .map_err(|e| ScanError::Bundle(format!("parse failed: {e}")))
    });
    if let Some(p) = cache_path {
        if let Err(e) = next.save(p) {
            log::warn!("[scan] cache not written ({}): {e}", p.display());
        }
    }
    let elapsed_ms = started.elapsed().as_millis();
    log::info!(
        "[scan] {} bundle(s): {} from cache, {} scanned{}, {} failed, {} plugin(s), {elapsed_ms} ms",
        stats.scanned,
        stats.cached,
        stats.fresh,
        if force { " (forced)" } else { "" },
        stats.failed.len(),
        ok.len()
    );
    if !stats.failed.is_empty() {
        log::warn!(
            "[scan] not listed (rescan from the picker to retry): {}",
            stats.failed.join("; ")
        );
    }
    if cfg!(debug_assertions) {
        let diag = serde_json::json!({
            "step": "scan",
            "scanned": stats.scanned,
            "cached": stats.cached,
            "fresh": stats.fresh,
            "forced": force,
            "elapsed_ms": elapsed_ms as u64,
            "ok": ok.len(),
            "failed": stats.failed,
        });
        println!("[diag] {diag}");
    }
    Ok(ok)
}
