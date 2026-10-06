//! P9.1 plugin scan. `scan_one` runs in the child process (a `.clap` loads via clack-host's
//! `PluginEntry::load` — API verified vs docs.rs/clack-host 0.1.0: the loader is `PluginEntry`, NOT
//! `PluginBundle`, and factory enumeration needs no `Host`). `scan_all` is the parent: it walks the
//! CLAP, VST3 and VST2 search dirs, then the player's own folders (`folders.rs`), and spawns one child per bundle, so a crashy bundle can't take down the
//! host — but only for bundles whose binary changed since the last scan: `ScanCache` remembers each
//! bundle's outcome (or its failure) under a size + mtime fingerprint, so a launch spawns no
//! foreign code at all until a plugin is installed or updated, and a hung bundle costs its 20 s
//! timeout once, not once per launch. The picker's rescan button forces a fresh scan.
//!
//! A VST2 plugin is a `.dll` among other DLLs: the walk reads each one's headers (`pe.rs`, no code
//! runs) and only a file that exports a VST2 entry becomes a bundle. A plugin this build cannot
//! host (32-bit, a shell) is reported as unsupported, with why, beside the descriptors.

use super::engine_slot::PluginFormat;
use super::pe;
use super::state::{PluginDescriptor, UnsupportedPlugin};
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
        let binary = if PluginFormat::of_path(outer) == Some(PluginFormat::Vst3) {
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

/// What a `--scan-one` child learned from one bundle, and prints as JSON: its descriptors (possibly
/// none: a module with no loadable class), and, for a plugin this build cannot host, why not.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanOutcome {
    pub plugins: Vec<PluginDescriptor>,
    pub unsupported: Option<String>,
}

impl ScanOutcome {
    fn plugins(plugins: Vec<PluginDescriptor>) -> Self {
        Self { plugins, unsupported: None }
    }
}

/// Why a 32-bit VST2 plugin is not listed (`pe::Class::PossiblyVst32`; the walk says so, no child runs).
pub(crate) const UNSUPPORTED_32_BIT: &str = "32-bit plugin: this build hosts 64-bit plugins only";
/// Why a VST2 shell is not listed (the scan child says so).
pub(crate) const UNSUPPORTED_SHELL: &str = "shell plugin (several plugins in one file) is not supported";

/// What the last scan learned about one bundle at one fingerprint.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct CacheEntry {
    fingerprint: Fingerprint,
    /// `Ok` = what its child reported; `Err` = the bundle-side failure the scan hit. A forced
    /// rescan retries the latter.
    outcome: Result<ScanOutcome, String>,
}

/// Bump when the entry shape or the descriptor fields change: an old file is discarded whole.
const SCAN_CACHE_VERSION: u32 = 2;

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

/// What one scan found: the plugins to list, and the files that are plugins this build cannot
/// host, each with why.
#[derive(Default, Debug, PartialEq, Eq)]
struct Scanned {
    plugins: Vec<PluginDescriptor>,
    unsupported: Vec<UnsupportedPlugin>,
}

impl Scanned {
    fn take(&mut self, path: &str, outcome: &ScanOutcome) {
        self.plugins.extend(outcome.plugins.iter().cloned());
        if let Some(reason) = &outcome.unsupported {
            self.unsupported.push(UnsupportedPlugin { path: path.to_string(), reason: reason.clone() });
        }
    }
}

/// The cache-aware core of `scan_all`, with the child runner injected so the cache logic is testable
/// without a plugin. `previous` is consulted unless `force`; the returned cache holds exactly the
/// bundles in `files` (reused or freshly scanned), ready to be written back.
fn scan_bundles(
    files: &[PathBuf],
    previous: &ScanCache,
    force: bool,
    mut scan: impl FnMut(&Path) -> Result<ScanOutcome, ScanError>,
) -> (Scanned, ScanCache, ScanStats) {
    let mut found = Scanned::default();
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
                    Ok(outcome) => Ok(outcome),
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
                            Ok(outcome) => found.take(&key, &outcome),
                            Err(_) => stats.failed.push(key.clone()),
                        }
                        continue;
                    }
                }
            }
        };
        match &entry.outcome {
            Ok(outcome) => found.take(&key, outcome),
            Err(_) => stats.failed.push(key.clone()),
        }
        next.bundles.insert(key, entry);
    }
    (found, next, stats)
}

/// One scan over `roots`: the walk, then `scan_bundles` over what it found. The walk's own verdicts
/// (a 32-bit plugin) are made again on every scan, as a header read is all they cost; a child's
/// (a shell) come from the cache with the bundle's entry. A root that is gone takes both with it.
fn scan_roots(
    roots: &[Root],
    previous: &ScanCache,
    force: bool,
    scan: impl FnMut(&Path) -> Result<ScanOutcome, ScanError>,
) -> (Scanned, ScanCache, ScanStats) {
    let walked = find_bundles(roots);
    let (mut found, next, stats) = scan_bundles(&walked.bundles, previous, force, scan);
    found.unsupported.splice(0..0, walked.unsupported);
    (found, next, stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const FIXTURE_CHILD_ENV: &str = "LF_SCAN_FIXTURE_CHILD";

    /// The child half of the two scan-gate tests: this test exe re-invoked through the production
    /// gate. A no-op in a normal (or `--ignored`) run.
    #[test]
    #[ignore = "child half of the scan-gate tests"]
    fn scan_gate_fixture_child() {
        if std::env::var_os(FIXTURE_CHILD_ENV).is_none() {
            return;
        }
        if let Err(e) = await_scan_gate() {
            println!("gate refused: {e}");
            return;
        }
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

    fn fixture_child() -> Command {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            "host::scan::tests::scan_gate_fixture_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(FIXTURE_CHILD_ENV, "1");
        cmd
    }

    /// Audit B9: the scan child waits at the gate until it sits in the kill-on-close Job, so a
    /// process it starts is in the Job too. Closing the Job after the child exits kills the
    /// grandchild, which releases the stdout pipe it inherited — the scan returns at once instead of
    /// waiting ~30 s for the grandchild (or 20 s for the timeout). The parent holds the child
    /// between spawn and Job assignment for 2 s: a child that does not wait at the gate has
    /// started the grandchild and exited by then, and the late assignment fails.
    #[test]
    fn a_scan_childs_grandchild_dies_with_the_job() {
        let started = Instant::now();
        let out = run_gated_child(fixture_child(), Duration::from_secs(20), || {
            std::thread::sleep(Duration::from_secs(2))
        })
        .unwrap_or_else(|e| {
            panic!("the child must wait at the gate for the Job, then exit cleanly: {e}")
        });
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

    /// Audit B9, the parent that gives up: the gate pipe closes without the release byte (a failed
    /// Job assignment), and the child must start nothing.
    #[test]
    fn a_scan_child_whose_gate_closes_unreleased_starts_nothing() {
        let mut child = fixture_child()
            .env(SCAN_GATE_ENV, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the fixture child");
        drop(child.stdin.take()); // EOF before any release byte
        let out = child.wait_with_output().expect("the fixture child exits");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.contains("gate refused: scan gate closed before release"),
            "the child must refuse at the gate: {text}"
        );
        assert!(!text.contains("grandchild="), "nothing may start past a closed gate: {text}");
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

    impl Scratch {
        /// A file at `rel` under the scratch dir (parents made), as a fake plugin binary.
        fn file(&self, rel: &str) -> PathBuf {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"x").unwrap();
            p
        }
        /// A VST3 folder bundle at `rel`, its inner DLL in place; answers the OUTER path.
        fn vst3_bundle(&self, rel: &str) -> PathBuf {
            let outer = self.0.join(rel);
            let inner = outer.file_name().unwrap().to_string_lossy().into_owned();
            self.file(&format!("{rel}/Contents/x86_64-win/{inner}"));
            outer
        }
    }

    fn user(path: &Path) -> Root {
        Root { path: path.to_path_buf(), only: None }
    }
    fn builtin(path: &Path, format: PluginFormat) -> Root {
        Root { path: path.to_path_buf(), only: Some(format) }
    }
    fn sorted(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
        paths.sort();
        paths
    }

    #[test]
    fn a_user_folder_yields_every_format_and_a_bundle_is_never_entered() {
        let scratch = Scratch::new("walk-user");
        let clap = scratch.file("mixed/Vendor/Synth.clap");
        let bundle = scratch.vst3_bundle("mixed/Vendor/Bundle.vst3");
        let single = scratch.file("mixed/Single.VST3");
        // Inside the bundle: a stray `.clap` and a second inner `.vst3`. Neither is a plugin.
        scratch.file("mixed/Vendor/Bundle.vst3/Contents/Resources/stray.clap");
        scratch.file("mixed/Vendor/Bundle.vst3/Contents/x86_64-win/Other.vst3");
        // A directory that only looks like a CLAP, and a file of no format.
        std::fs::create_dir_all(scratch.0.join("mixed/Folder.clap")).unwrap();
        scratch.file("mixed/readme.txt");
        let nested_clap = scratch.file("mixed/Folder.clap/Real.clap");

        let found = find_bundles(&[user(&scratch.0.join("mixed"))]);
        assert_eq!(sorted(found.bundles), sorted(vec![clap, bundle, single, nested_clap]));
        assert_eq!(found.unsupported, vec![]);
    }

    #[test]
    fn a_builtin_root_yields_only_its_own_format() {
        let scratch = Scratch::new("walk-builtin");
        let clap = scratch.file("root/A.clap");
        let single = scratch.file("root/B.vst3");
        let bundle = scratch.vst3_bundle("root/C.vst3");
        let root = scratch.0.join("root");
        assert_eq!(find_bundles(&[builtin(&root, PluginFormat::Clap)]).bundles, vec![clap.clone()]);
        assert_eq!(
            sorted(find_bundles(&[builtin(&root, PluginFormat::Vst3)]).bundles),
            sorted(vec![single.clone(), bundle.clone()])
        );
        // The CLAP roots are walked first, as the scan always listed them.
        let both = find_bundles(&[builtin(&root, PluginFormat::Clap), builtin(&root, PluginFormat::Vst3)]).bundles;
        assert_eq!(both[0], clap);
        assert_eq!(both.len(), 3);
    }

    #[test]
    fn overlapping_roots_yield_a_plugin_once_in_the_first_spelling() {
        let scratch = Scratch::new("walk-overlap");
        let clap = scratch.file("Root/Sub/A.clap");
        let bundle = scratch.vst3_bundle("Root/Sub/B.vst3");
        let root = scratch.0.join("Root");
        // The same folder in another case, and a folder inside it: every plugin was met already.
        let shouted = PathBuf::from(root.to_string_lossy().to_uppercase());
        let inner = root.join("Sub");
        let roots = [
            builtin(&root, PluginFormat::Clap),
            builtin(&root, PluginFormat::Vst3),
            user(&shouted),
            user(&inner),
        ];
        assert_eq!(find_bundles(&roots).bundles, vec![clap.clone(), bundle.clone()]);
        // First seen wins, whichever root that is.
        let found = find_bundles(&[user(&shouted), builtin(&root, PluginFormat::Clap)]).bundles;
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|p| p.starts_with(&shouted)), "{found:?}");
        assert!(!found.contains(&clap));
    }

    #[test]
    fn a_root_inside_a_bundle_and_a_missing_root_yield_nothing() {
        let scratch = Scratch::new("walk-edges");
        let bundle = scratch.vst3_bundle("vst/Bundle.vst3");
        scratch.file("vst/Bundle.vst3/Contents/Resources/stray.clap");
        let good = scratch.file("good/A.clap");
        let roots = [
            user(&bundle.join("Contents")),
            user(&bundle.join("Contents").join("x86_64-win")),
            user(&scratch.0.join("never-made")),
            user(&scratch.0.join("good")),
        ];
        assert_eq!(find_bundles(&roots).bundles, vec![good]);
        // The bundle itself as a root is that one plugin (what a `VST3_PATH` entry naming a bundle
        // always found).
        assert_eq!(find_bundles(&[user(&bundle)]).bundles, vec![bundle]);
    }

    /// A directory junction: a link Windows makes without the privilege a symlink needs.
    fn junction(link: &Path, target: &Path) {
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(made.status.success(), "mklink /J: {}", String::from_utf8_lossy(&made.stderr));
    }

    #[test]
    fn a_linked_bundle_is_one_plugin_and_a_link_into_a_bundle_yields_nothing() {
        let scratch = Scratch::new("walk-links");
        scratch.vst3_bundle("real/Bundle.vst3");
        // A root that is a link to a bundle: the walk follows a root link, and must still stop there.
        // (`cmd` reads a `/` in a path as a switch: the junction's target is spelled with `\\`.)
        let bundle_dir = scratch.0.join("real").join("Bundle.vst3");
        let alias = scratch.0.join("Alias.vst3");
        junction(&alias, &bundle_dir);
        assert_eq!(find_bundles(&[user(&alias)]).bundles, vec![alias.clone()]);
        // A root whose spelling hides that it lies inside a bundle.
        let inside = scratch.0.join("Inside");
        junction(&inside, &bundle_dir.join("Contents").join("x86_64-win"));
        assert_eq!(find_bundles(&[user(&inside)]).bundles, Vec::<PathBuf>::new());
    }

    #[test]
    fn the_builtin_roots_are_each_formats_own_in_scan_order() {
        let roots = builtin_roots();
        let first_vst3 = roots.iter().position(|r| r.only == Some(PluginFormat::Vst3)).unwrap();
        let first_vst2 = roots.iter().position(|r| r.only == Some(PluginFormat::Vst2)).unwrap();
        assert!(first_vst3 > 0, "the CLAP roots come first");
        assert!(first_vst2 > first_vst3, "the VST2 roots come last");
        assert!(roots[..first_vst3].iter().all(|r| r.only == Some(PluginFormat::Clap)));
        assert!(roots[first_vst3..first_vst2].iter().all(|r| r.only == Some(PluginFormat::Vst3)));
        assert!(roots[first_vst2..].iter().all(|r| r.only == Some(PluginFormat::Vst2)));
        let common = std::env::var("CommonProgramFiles").unwrap();
        assert_eq!(roots[0].path, Path::new(&common).join("CLAP"));
        assert_eq!(roots[first_vst3].path, Path::new(&common).join("VST3"));
        // The four fixed VST2 folders, in order, then what the registry and `VST_PATH` add.
        let programs = std::env::var("ProgramFiles").unwrap();
        let fixed = [
            Path::new(&programs).join("VSTPlugins"),
            Path::new(&programs).join("Steinberg").join("VSTPlugins"),
            Path::new(&common).join("VST2"),
            Path::new(&common).join("Steinberg").join("VST2"),
        ];
        let vst2: Vec<PathBuf> = roots[first_vst2..].iter().map(|r| r.path.clone()).collect();
        assert_eq!(vst2[..4], fixed);
    }

    #[test]
    fn the_registrys_vst2_folder_follows_the_fixed_ones_and_is_listed_once() {
        let programs = std::env::var("ProgramFiles").unwrap();
        let fixed = vst2_roots(None);
        let added = vst2_roots(Some(r"D:\Audio\VstPlugins".to_string()));
        assert_eq!(added[..4], fixed[..4]);
        assert_eq!(added[4], Path::new(r"D:\Audio\VstPlugins"));
        assert_eq!(added.len(), fixed.len() + 1);
        // The registry naming a folder the scan walks anyway, in another case.
        let same = Path::new(&programs).join("vstplugins").to_string_lossy().into_owned();
        assert_eq!(vst2_roots(Some(same)), fixed);
    }

    /// A DLL the pre-filter reads as `machine`'s, exporting `names`; its path as the walk spells it.
    fn dll(scratch: &Scratch, rel: &str, machine: u16, names: &[&str]) -> PathBuf {
        std::fs::write(scratch.file(rel), pe::fixture::image(machine, names).bytes).unwrap();
        rel.split('/').fold(scratch.0.clone(), |path, part| path.join(part))
    }

    fn unsupported(path: &Path, reason: &str) -> UnsupportedPlugin {
        UnsupportedPlugin { path: path.to_string_lossy().into_owned(), reason: reason.to_string() }
    }

    #[test]
    fn a_vst2_root_yields_only_dlls_and_only_those_that_export_an_entry() {
        use pe::fixture::{AMD64, I386};
        let scratch = Scratch::new("walk-vst2");
        let plugin = dll(&scratch, "root/Vendor/Synth.dll", AMD64, &["VSTPluginMain"]);
        let old = dll(&scratch, "root/Old.DLL", AMD64, &["main"]);
        let narrow = dll(&scratch, "root/Narrow.dll", I386, &["VSTPluginMain"]);
        dll(&scratch, "root/helper.dll", AMD64, &["DllGetClassObject"]);
        dll(&scratch, "root/helper32.dll", I386, &["DllGetClassObject"]);
        scratch.file("root/notes.dll"); // Not a PE image at all.
        // A PE image cut short: its exports cannot be read, so its scan child decides.
        let cut = dll(&scratch, "root/Cut.dll", AMD64, &["VSTPluginMain"]);
        let whole = std::fs::read(&cut).unwrap();
        std::fs::write(&cut, &whole[..whole.len() - 4]).unwrap();
        // Other formats in a VST2 root are not this root's.
        scratch.file("root/Other.clap");
        scratch.file("root/Other.vst3");
        // A directory that only looks like a DLL holds one.
        let nested = dll(&scratch, "root/Folder.dll/Real.dll", AMD64, &["VSTPluginMain"]);
        // A bundle's inner files are never visited, a plugin DLL among them included.
        scratch.vst3_bundle("root/Bundle.vst3");
        dll(&scratch, "root/Bundle.vst3/Contents/x86_64-win/Inside.dll", AMD64, &["VSTPluginMain"]);
        dll(&scratch, "root/Bundle.vst3/Contents/Resources/Inside32.dll", I386, &["VSTPluginMain"]);

        let root = scratch.0.join("root");
        let found = find_bundles(&[builtin(&root, PluginFormat::Vst2)]);
        assert_eq!(sorted(found.bundles), sorted(vec![plugin.clone(), old.clone(), cut.clone(), nested.clone()]));
        assert_eq!(found.unsupported, vec![unsupported(&narrow, UNSUPPORTED_32_BIT)]);
        // The other formats' roots never yield a DLL.
        let others = find_bundles(&[builtin(&root, PluginFormat::Clap), builtin(&root, PluginFormat::Vst3)]);
        assert!(others.bundles.iter().all(|p| PluginFormat::of_path(p) != Some(PluginFormat::Vst2)), "{others:?}");
        assert_eq!(others.bundles.len(), 3);
        assert_eq!(others.unsupported, vec![]);
    }

    #[test]
    fn a_user_folder_yields_vst2_plugins_beside_the_other_formats() {
        use pe::fixture::{AMD64, I386};
        let scratch = Scratch::new("walk-user-vst2");
        let clap = scratch.file("mine/Synth.clap");
        let bundle = scratch.vst3_bundle("mine/Bundle.vst3");
        let plugin = dll(&scratch, "mine/Amp.dll", AMD64, &["VSTPluginMain"]);
        let narrow = dll(&scratch, "mine/Sub/Amp32.dll", I386, &["main"]);
        dll(&scratch, "mine/runtime.dll", AMD64, &["malloc", "free"]);
        dll(&scratch, "mine/Bundle.vst3/Contents/x86_64-win/Inside.dll", AMD64, &["VSTPluginMain"]);
        let mine = scratch.0.join("mine");
        let found = find_bundles(&[user(&mine)]);
        assert_eq!(sorted(found.bundles), sorted(vec![clap, bundle, plugin.clone()]));
        assert_eq!(found.unsupported, vec![unsupported(&narrow, UNSUPPORTED_32_BIT)]);
        // A plugin (and a 32-bit one) a built-in root already met is not met again in the folder.
        let twice = find_bundles(&[builtin(&mine, PluginFormat::Vst2), user(&mine)]);
        assert_eq!(twice.bundles.iter().filter(|p| **p == plugin).count(), 1);
        assert_eq!(twice.unsupported.len(), 1);
    }

    #[test]
    fn an_unsupported_outcome_round_trips_cold_cached_and_forced() {
        use pe::fixture::{AMD64, I386};
        let scratch = Scratch::new("unsupported");
        let good = dll(&scratch, "root/Good.dll", AMD64, &["VSTPluginMain"]);
        let shell = dll(&scratch, "root/Shell.dll", AMD64, &["VSTPluginMain"]);
        let narrow = dll(&scratch, "root/Narrow.dll", I386, &["VSTPluginMain"]);
        let roots = [builtin(&scratch.0.join("root"), PluginFormat::Vst2)];
        let children = AtomicUsize::new(0);
        let scan = |p: &Path| {
            children.fetch_add(1, Ordering::Relaxed);
            Ok(if p == shell {
                ScanOutcome { plugins: Vec::new(), unsupported: Some(UNSUPPORTED_SHELL.to_string()) }
            } else {
                ScanOutcome::plugins(vec![desc(p, "Good")])
            })
        };
        // The walk's verdict first, then the children's.
        let expected = vec![unsupported(&narrow, UNSUPPORTED_32_BIT), unsupported(&shell, UNSUPPORTED_SHELL)];

        // Cold: the 32-bit file gets no child; the shell's child says what it is.
        let (cold, cache, stats) = scan_roots(&roots, &ScanCache::default(), false, scan);
        assert_eq!(cold.plugins, vec![desc(&good, "Good")]);
        assert_eq!(cold.unsupported, expected);
        assert_eq!(children.load(Ordering::Relaxed), 2);
        assert!(stats.failed.is_empty(), "an unsupported plugin is not a failed scan");
        assert!(!cache.bundles.contains_key(&narrow.to_string_lossy().to_string()), "the walk's verdict is not cached");

        // Cached, through the file: no child, the same two reasons.
        let file = scratch.0.join("plugin-scan.json");
        cache.save(&file).unwrap();
        let (cached, cache2, stats2) = scan_roots(&roots, &ScanCache::load(&file), false, scan);
        assert_eq!(children.load(Ordering::Relaxed), 2, "a cached outcome spawns no child");
        assert_eq!((stats2.cached, stats2.fresh), (2, 0));
        assert_eq!(cached, cold);

        // Forced: both children run again and say the same.
        let (forced, _, stats3) = scan_roots(&roots, &cache2, true, scan);
        assert_eq!(children.load(Ordering::Relaxed), 4);
        assert_eq!((stats3.cached, stats3.fresh), (0, 2));
        assert_eq!(forced, cold);
    }

    #[test]
    fn a_removed_root_takes_its_unsupported_plugins_with_it() {
        use pe::fixture::{AMD64, I386};
        let scratch = Scratch::new("removed-root");
        let kept = dll(&scratch, "kept/Good.dll", AMD64, &["VSTPluginMain"]);
        let kept_narrow = dll(&scratch, "kept/Narrow.dll", I386, &["main"]);
        let shell = dll(&scratch, "gone/Shell.dll", AMD64, &["VSTPluginMain"]);
        let narrow = dll(&scratch, "gone/Narrow.dll", I386, &["VSTPluginMain"]);
        let both = [user(&scratch.0.join("kept")), user(&scratch.0.join("gone"))];
        let scan = |p: &Path| {
            Ok(if p == shell {
                ScanOutcome { plugins: Vec::new(), unsupported: Some(UNSUPPORTED_SHELL.to_string()) }
            } else {
                ScanOutcome::plugins(vec![desc(p, "Good")])
            })
        };
        let (before, cache, _) = scan_roots(&both, &ScanCache::default(), false, scan);
        assert_eq!(
            before.unsupported,
            vec![
                unsupported(&kept_narrow, UNSUPPORTED_32_BIT),
                unsupported(&narrow, UNSUPPORTED_32_BIT),
                unsupported(&shell, UNSUPPORTED_SHELL),
            ]
        );
        // The player removed the folder: the next scan knows neither of its two any more.
        let (after, cache2, _) = scan_roots(&both[..1], &cache, false, scan);
        assert_eq!(after.plugins, vec![desc(&kept, "Good")]);
        assert_eq!(after.unsupported, vec![unsupported(&kept_narrow, UNSUPPORTED_32_BIT)]);
        assert_eq!(cache2.bundles.len(), 1, "the shell's cached outcome went with its root");
    }

    #[test]
    fn a_vst2_effect_is_described_by_its_names_its_id_and_its_kind() {
        use super::super::vst2::fixture::{self, Shape};
        use super::super::vst2::HostContext;
        use super::super::vst2_abi::{EFF_FLAGS_CAN_REPLACING, EFF_FLAGS_IS_SYNTH, PLUG_CATEGORY_SHELL};
        let path = r"C:\Plugins\Some Amp.dll";
        let describe = |shape: Shape| {
            let ctx = HostContext::new(VST2_SCAN_RATE, VST2_SCAN_BLOCK).unwrap();
            // SAFETY: the fixture's entry is in this process and `ctx` outlives the call.
            let outcome = fixture::with_shape(shape, || unsafe { describe_vst2(fixture::entry, &ctx, path) });
            // What the constructor was told, and the calls the effect got.
            (outcome, fixture::take_seen(), fixture::take_calls())
        };
        let vst2 = |id: &str, name: &str, is_effect: bool| PluginDescriptor {
            id: id.to_string(),
            name: name.to_string(),
            format: "vst2".to_string(),
            path: path.to_string(),
            is_effect: Some(is_effect),
        };

        let (effect, seen, calls) = describe(Shape { unique_id: 0x0000_beef, ..Shape::default() });
        assert_eq!(effect, Ok(ScanOutcome::plugins(vec![vst2("0000beef", "Fixture", true)])));
        assert_eq!(seen, vec![48_000, 48_000, 512, 512], "the scan's fixed rate and block, no device");
        assert_eq!(calls.first().map(|c| c.0), Some(0), "effOpen first");
        assert_eq!(calls.last().map(|c| c.0), Some(1), "effClose last");

        // An instrument, a high unique id, and the two name fallbacks.
        let synth = Shape {
            flags: EFF_FLAGS_CAN_REPLACING | EFF_FLAGS_IS_SYNTH,
            unique_id: 0xfedc_ba98_u32 as i32,
            effect_name: "",
            ..Shape::default()
        };
        assert_eq!(describe(synth).0, Ok(ScanOutcome::plugins(vec![vst2("fedcba98", "Fixture Product", false)])));
        let nameless = Shape { effect_name: "", product: "", unique_id: 1, ..Shape::default() };
        assert_eq!(describe(nameless).0, Ok(ScanOutcome::plugins(vec![vst2("00000001", "Some Amp", true)])));

        // A shell: no descriptor, the reason, and it is closed like any other.
        let (shell, _, calls) = describe(Shape { category: PLUG_CATEGORY_SHELL, ..Shape::default() });
        assert_eq!(shell, Ok(ScanOutcome { plugins: Vec::new(), unsupported: Some(UNSUPPORTED_SHELL.to_string()) }));
        assert_eq!(calls.last().map(|c| c.0), Some(1));

        // An effect the host refuses is a scan error, and nothing of it was called.
        let (refused, _, calls) = describe(Shape { outputs: 0, ..Shape::default() });
        assert!(refused.unwrap_err().contains("channel count 0"));
        assert_eq!(calls, vec![]);
    }

    #[test]
    fn a_scan_child_routes_by_format_and_refuses_any_other_file() {
        let e = scan_one(r"C:\Plugins\readme.txt").unwrap_err();
        assert!(e.contains("not a .clap, .vst3 or .dll plugin"), "{e}");
        assert!(scan_one(r"C:\Plugins\no-extension").is_err());
        // The outcome a child prints, and the parent reads back.
        let printed = serde_json::to_value(ScanOutcome::plugins(Vec::new())).unwrap();
        assert_eq!(printed, serde_json::json!({ "plugins": [], "unsupported": null }));
        let shell = ScanOutcome { plugins: Vec::new(), unsupported: Some(UNSUPPORTED_SHELL.to_string()) };
        let printed = serde_json::to_string(&shell).unwrap();
        assert_eq!(printed, r#"{"plugins":[],"unsupported":"shell plugin (several plugins in one file) is not supported"}"#);
        assert_eq!(serde_json::from_str::<ScanOutcome>(&printed).unwrap(), shell);
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
                Ok(ScanOutcome::plugins(vec![desc(p, "Good")]))
            }
        };

        // First launch: everything is scanned, the failure is remembered too.
        let (found, cache, stats) = scan_bundles(&files, &ScanCache::default(), false, scan);
        let descs = found.plugins;
        assert_eq!(descs.len(), 1);
        assert_eq!(children.load(Ordering::Relaxed), 2);
        assert_eq!(stats.cached, 0);
        assert_eq!(stats.fresh, 2);
        assert_eq!(stats.failed, vec![bad.to_string_lossy().to_string()]);
        assert!(cache.bundles[&bad.to_string_lossy().to_string()].outcome.is_err());

        // Second launch: no child runs; the same descriptors and the same failure come from memory.
        let (found2, cache2, stats2) = scan_bundles(&files, &cache, false, scan);
        assert_eq!(found2.plugins, descs);
        assert_eq!(children.load(Ordering::Relaxed), 2, "an unchanged bundle spawns no child");
        assert_eq!(stats2.cached, 2);
        assert_eq!(stats2.fresh, 0);
        assert_eq!(stats2.failed, stats.failed);

        // The bad plugin was updated (bytes changed → size differs): only it is rescanned.
        std::fs::write(&bad, b"v2-fixed").unwrap();
        let (_, cache3, stats3) = scan_bundles(&files, &cache2, false, |p: &Path| {
            children.fetch_add(1, Ordering::Relaxed);
            Ok(ScanOutcome::plugins(vec![desc(p, "Fixed")]))
        });
        assert_eq!(children.load(Ordering::Relaxed), 3);
        assert_eq!((stats3.cached, stats3.fresh), (1, 1));
        assert!(stats3.failed.is_empty());
        assert!(cache3.bundles[&bad.to_string_lossy().to_string()].outcome.is_ok());

        // The rescan button: everything is scanned again regardless of fingerprints.
        let (_, _, stats4) = scan_bundles(&files, &cache3, true, |p: &Path| {
            children.fetch_add(1, Ordering::Relaxed);
            Ok(ScanOutcome::plugins(vec![desc(p, "Forced")]))
        });
        assert_eq!(children.load(Ordering::Relaxed), 5);
        assert_eq!((stats4.cached, stats4.fresh), (0, 2));
    }

    #[test]
    fn host_side_failures_and_removed_bundles_are_not_remembered() {
        let scratch = Scratch::new("host");
        let a = scratch.bundle("a.clap", b"a");
        let b = scratch.bundle("b.clap", b"b");
        let (found, cache, stats) =
            scan_bundles(&[a.clone(), b.clone()], &ScanCache::default(), false, |p: &Path| {
                if p == a {
                    Err(ScanError::Host("spawn: exe missing".into()))
                } else {
                    Ok(ScanOutcome::plugins(vec![desc(p, "B")]))
                }
            });
        assert_eq!(found.plugins.len(), 1);
        assert_eq!(stats.failed, vec![a.to_string_lossy().to_string()]);
        assert!(
            !cache.bundles.contains_key(&a.to_string_lossy().to_string()),
            "a host-side failure must be retried next launch"
        );
        // `b` uninstalled: the written-back cache holds only what the walk found.
        let (_, cache2, _) = scan_bundles(&[a.clone()], &cache, false, |p: &Path| Ok(ScanOutcome::plugins(vec![desc(p, "A")])));
        assert_eq!(cache2.bundles.len(), 1);
        assert!(cache2.bundles.contains_key(&a.to_string_lossy().to_string()));
    }

    #[test]
    fn cache_file_round_trips_and_a_foreign_version_or_garbage_is_ignored() {
        let scratch = Scratch::new("file");
        let bundle = scratch.bundle("x.clap", b"x");
        let path = scratch.0.join("plugin-scan.json");
        let (_, cache, _) = scan_bundles(&[bundle.clone()], &ScanCache::default(), false, |p: &Path| {
            Ok(ScanOutcome::plugins(vec![desc(p, "X")]))
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

/// One folder the scan walks. `only`: the one format a built-in root yields; `None` is a folder
/// the player added (`folders.rs`), which yields every format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Root {
    pub(crate) path: PathBuf,
    pub(crate) only: Option<PluginFormat>,
}

/// The roots the scan walks by itself, in scan order. CLAP, then VST3: `%COMMONPROGRAMFILES%\<FORMAT>`,
/// `%LOCALAPPDATA%\Programs\Common\<FORMAT>`, then every `;`-separated entry of `CLAP_PATH` /
/// `VST3_PATH`. Then VST2, which has no one standard folder (`vst2_roots`). The player's own folders
/// come after these (`scan_all`). A new format is one more variant of `PluginFormat`, its roots here
/// and a candidate rule in `find_bundles`.
pub(crate) fn builtin_roots() -> Vec<Root> {
    let mut roots = Vec::new();
    for (format, dir, path_var) in [(PluginFormat::Clap, "CLAP", "CLAP_PATH"), (PluginFormat::Vst3, "VST3", "VST3_PATH")] {
        let mut push = |path: PathBuf| roots.push(Root { path, only: Some(format) });
        if let Ok(cpf) = std::env::var("CommonProgramFiles") {
            push(Path::new(&cpf).join(dir));
        }
        if let Ok(lad) = std::env::var("LOCALAPPDATA") {
            push(Path::new(&lad).join("Programs").join("Common").join(dir));
        }
        if let Ok(paths) = std::env::var(path_var) {
            for p in paths.split(';').filter(|s| !s.is_empty()) {
                push(PathBuf::from(p));
            }
        }
    }
    roots.extend(vst2_roots(registry_vst2_path()).into_iter().map(|path| Root { path, only: Some(PluginFormat::Vst2) }));
    roots
}

/// The folders VST2 installers use, in scan order: `%ProgramFiles%\VSTPlugins`,
/// `%ProgramFiles%\Steinberg\VSTPlugins`, `%CommonProgramFiles%\VST2`,
/// `%CommonProgramFiles%\Steinberg\VST2`, the folder the registry names (`registry`), then every
/// `;`-separated entry of `VST_PATH`. The registry and `VST_PATH` often name one of the others: a
/// folder is listed once.
fn vst2_roots(registry: Option<String>) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut push = |path: PathBuf| {
        if !roots.iter().any(|known| known.as_os_str().eq_ignore_ascii_case(path.as_os_str())) {
            roots.push(path);
        }
    };
    if let Ok(pf) = std::env::var("ProgramFiles") {
        push(Path::new(&pf).join("VSTPlugins"));
        push(Path::new(&pf).join("Steinberg").join("VSTPlugins"));
    }
    if let Ok(cpf) = std::env::var("CommonProgramFiles") {
        push(Path::new(&cpf).join("VST2"));
        push(Path::new(&cpf).join("Steinberg").join("VST2"));
    }
    if let Some(folder) = registry {
        push(PathBuf::from(folder));
    }
    if let Ok(paths) = std::env::var("VST_PATH") {
        for p in paths.split(';').filter(|s| !s.is_empty()) {
            push(PathBuf::from(p));
        }
    }
    roots
}

/// `HKLM\SOFTWARE\VST` → `VSTPluginsPath`: the VST2 folder installers register and read. `None`
/// when the value is not there (or is not a string).
fn registry_vst2_path() -> Option<String> {
    use windows::core::w;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    let mut buffer = [0u16; 1024];
    let mut bytes = std::mem::size_of_val(&buffer) as u32;
    // SAFETY: the key and value names are NUL-terminated literals; `buffer` is writable for the
    // `bytes` passed with it, and both outlive the call.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\VST"),
            w!("VSTPluginsPath"),
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
    };
    if status.is_err() {
        return None;
    }
    let written = &buffer[..(bytes as usize / 2).min(buffer.len())];
    let folder = String::from_utf16_lossy(written);
    let folder = folder.trim_end_matches('\0').trim();
    (!folder.is_empty()).then(|| folder.to_string())
}

/// What a walk found: the bundles to scan, and the files it already knows this build cannot host.
#[derive(Default, Debug, PartialEq, Eq)]
struct Walked {
    bundles: Vec<PathBuf>,
    unsupported: Vec<UnsupportedPlugin>,
}

/// Walk `roots` in order and collect each plugin's OUTER path, spelled as the walk met it: that
/// string becomes the descriptor's `path`, which the tone store and the frontend hash byte for
/// byte, so it is never canonicalised (a changed spelling would orphan saved tones). A plugin
/// reached twice (overlapping roots, one folder in two spellings) is kept once, in the FIRST
/// spelling seen (`folders::path_key`); the built-in roots come first, so a folder the player adds
/// never respells a plugin they already find. A missing root is skipped.
///
/// The walk recurses (Surge installs to a nested `CLAP\Surge Synth Team\`). A `.clap` candidate
/// must be a file. On Windows a `.vst3` is EITHER a folder bundle
/// (`Name.vst3/Contents/x86_64-win/Inner.vst3`, e.g. Surge XT) OR a single DLL with that extension
/// (e.g. Neural DSP); both yield their outer path (`resolve_vst3_binary` finds the DLL later) and
/// the walk never descends into a bundle, whose inner DLL ends `.vst3` too. A root that lies inside
/// a bundle yields nothing, also when it is a link into one.
///
/// A `.dll` is a VST2 candidate only by what its headers say (`pe::classify`, which loads nothing):
/// one that exports a VST2 entry is a bundle, and so is one whose exports cannot be read (the scan
/// child, isolated, finds out); a 32-bit one is unsupported; any other DLL is not a plugin and is
/// passed over in silence. A DLL inside a `.vst3` bundle is never reached.
fn find_bundles(roots: &[Root]) -> Walked {
    let mut found = Walked::default();
    let mut seen = std::collections::HashSet::new();
    let is_vst3 = |p: &Path| PluginFormat::of_path(p) == Some(PluginFormat::Vst3);
    for root in roots {
        // Where the root really is decides whether it lies inside a bundle: a link's own spelling
        // can hide that. Only the check uses the resolved path; what is walked and yielded stays
        // spelled as given.
        let resolved = std::fs::canonicalize(&root.path).unwrap_or_else(|_| root.path.clone());
        if !root.path.is_dir() || [&root.path, &resolved].iter().any(|p| p.ancestors().skip(1).any(is_vst3)) {
            continue;
        }
        let mut walk = walkdir::WalkDir::new(&root.path).into_iter();
        while let Some(entry) = walk.next() {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            let Some(format) = PluginFormat::of_path(path) else { continue };
            // The walk follows a ROOT that is a link (and reports it as a link, not a directory);
            // a link met further down is never followed, so there is nothing to skip there.
            if format == PluginFormat::Vst3 && (entry.file_type().is_dir() || (entry.depth() == 0 && path.is_dir())) {
                walk.skip_current_dir();
            }
            let wanted = root.only.is_none_or(|only| only == format);
            let candidate = match format {
                PluginFormat::Clap | PluginFormat::Vst2 => path.is_file(),
                PluginFormat::Vst3 => true,
            };
            if !(wanted && candidate && seen.insert(super::folders::path_key(path))) {
                continue;
            }
            if format != PluginFormat::Vst2 {
                found.bundles.push(path.to_path_buf());
                continue;
            }
            match pe::classify(path) {
                pe::Class::Candidate => found.bundles.push(path.to_path_buf()),
                pe::Class::Inconclusive(why) => {
                    log::info!("[scan] {}: {why}; left to its scan child", path.display());
                    found.bundles.push(path.to_path_buf());
                }
                pe::Class::PossiblyVst32 => found.unsupported.push(UnsupportedPlugin {
                    path: path.to_string_lossy().into_owned(),
                    reason: UNSUPPORTED_32_BIT.to_string(),
                }),
                pe::Class::NotAPlugin => {}
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
                if PluginFormat::of_path(&p) == Some(PluginFormat::Vst3) && p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    None
}

/// CHILD side: load ONE plugin (`.clap`, `.vst3` or a VST2 `.dll`) and return what it holds. Routes
/// by format (`PluginFormat::of_path`); every one runs in the throwaway `--scan-one` child process
/// for crash isolation.
pub fn scan_one(path: &str) -> Result<ScanOutcome, String> {
    match PluginFormat::of_path(Path::new(path)) {
        Some(PluginFormat::Clap) => scan_one_clap(path).map(ScanOutcome::plugins),
        Some(PluginFormat::Vst3) => scan_one_vst3(path).map(ScanOutcome::plugins),
        Some(PluginFormat::Vst2) => scan_one_vst2(path),
        None => Err(format!("{path}: not a .clap, .vst3 or .dll plugin")),
    }
}

/// What the scan tells a VST2 plugin its host runs at: no device is open, and none is needed.
const VST2_SCAN_RATE: f64 = 48_000.0;
const VST2_SCAN_BLOCK: i32 = 512;

/// CHILD side: load ONE VST2 `.dll`, open its effect and describe it. No `FreeLibrary` and the
/// host context is never freed: the child process exits right after printing, and a plugin that
/// started a thread of its own may still call the host.
fn scan_one_vst2(path: &str) -> Result<ScanOutcome, String> {
    use super::vst2::{HostContext, Vst2Module};
    let module = Vst2Module::load(Path::new(path))?;
    let ctx = HostContext::new(VST2_SCAN_RATE, VST2_SCAN_BLOCK)?;
    // SAFETY: the entry of the module just loaded, which is leaked below with the context. It runs
    // the plugin's foreign code: accepted because this is the throwaway `--scan-one` child process
    // (same isolation as the CLAP and VST3 scans).
    let outcome = unsafe { describe_vst2(module.entry(), &ctx, path) };
    module.leak();
    std::mem::forget(ctx);
    outcome
}

/// Open one effect through `entry` and describe it: its name from `effGetEffectName`, else
/// `effGetProductString`, else the file's stem; its `id` the effect's unique id as 8 hex digits. A
/// shell (one file, several plugins) yields no descriptor and says so. The effect is closed again.
///
/// # Safety
/// As `vst2::open_effect`: `entry` is a VST2 entry whose module stays loaded, and the caller keeps
/// `ctx` alive for as long as the plugin may call the host.
unsafe fn describe_vst2(
    entry: super::vst2_abi::EntryFn,
    ctx: &std::sync::Arc<super::vst2::HostContext>,
    path: &str,
) -> Result<ScanOutcome, String> {
    use super::vst2_abi::{EFF_GET_EFFECT_NAME, EFF_GET_PLUG_CATEGORY, EFF_GET_PRODUCT_STRING, PLUG_CATEGORY_SHELL};
    // SAFETY: the caller's contract.
    let effect = unsafe { super::vst2::open_effect(entry, ctx) }.map_err(|e| e.to_string())?;
    let info = *effect.info();
    // SAFETY: the thread that opened the effect; the category takes no pointer, the names a buffer
    // `Vst2Effect::string` provides.
    let (category, name) = unsafe {
        let category = effect.dispatch(EFF_GET_PLUG_CATEGORY, 0, 0, std::ptr::null_mut(), 0.0);
        let name = effect.string(EFF_GET_EFFECT_NAME, 0).or_else(|| effect.string(EFF_GET_PRODUCT_STRING, 0));
        effect.close();
        (category, name)
    };
    if category == PLUG_CATEGORY_SHELL {
        return Ok(ScanOutcome { plugins: Vec::new(), unsupported: Some(UNSUPPORTED_SHELL.to_string()) });
    }
    let stem = || Path::new(path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    Ok(ScanOutcome::plugins(vec![PluginDescriptor {
        id: format!("{:08x}", info.unique_id as u32),
        name: name.unwrap_or_else(stem),
        format: PluginFormat::Vst2.as_str().to_string(),
        path: path.to_string(),
        is_effect: Some(!info.is_synth()),
    }]))
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
                format: PluginFormat::Clap.as_str().to_string(),
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
                format: PluginFormat::Vst3.as_str().to_string(),
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
    run_gated_child(cmd, timeout, || {})
}

/// Spawn `cmd` gated (`SCAN_GATE_ENV`), put it in a fresh kill-on-close Job, THEN release it, and
/// collect its stdout under `timeout`. The child is our own exe, so it waits at the gate before any
/// foreign code runs: there is no window in which a plugin can start a process outside the Job.
/// `before_assign` runs between the spawn and the Job assignment; a test widens that window with it.
fn run_gated_child(
    mut cmd: Command,
    timeout: Duration,
    before_assign: impl FnOnce(),
) -> Result<Vec<u8>, ScanError> {
    let job = KillOnCloseJob::new().map_err(ScanError::Host)?;
    let mut child = cmd
        .env(SCAN_GATE_ENV, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ScanError::Host(format!("spawn: {e}")))?;
    before_assign();
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

/// At most one `scan_all` runs in the process: two at once would spawn each bundle's child twice and
/// race their cache writes. Held for the whole scan, so only ever taken on a blocking thread.
static SCAN_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// PARENT side: walk the built-in roots and then the player's folders (`folders_path`, the list in
/// `folders.rs`; None or unusable = the built-in roots only), reuse the cache for every bundle whose binary is unchanged
/// (unless `force`), spawn `app.exe --scan-one <path>` for the rest, aggregate, write the cache
/// back. A bundle whose child exits non-zero (handled error OR hard crash), times out, or whose
/// stdout doesn't parse is logged as `failed` and remembered as such; the parent always survives.
/// `cache_path` None (no app data dir) = scan everything, remember nothing. The plugins this build
/// cannot host (path and why) replace `unsupported`, the host state's list, before the scan gives
/// way to the next one: the descriptors returned and that list are one scan's, published together.
/// Emits the P9.1 gate diag in debug builds.
pub fn scan_all(
    cache_path: Option<&Path>,
    folders_path: Option<&Path>,
    force: bool,
    unsupported: &std::sync::Mutex<Vec<UnsupportedPlugin>>,
) -> Result<Vec<PluginDescriptor>, String> {
    let _scan = SCAN_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let started = Instant::now();
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    // The roots as they are when this scan starts: a folder added while it runs is the next scan's.
    let mut roots = builtin_roots();
    match folders_path.map(super::folders::load) {
        Some(Ok(folders)) => roots.extend(folders.into_iter().map(|f| Root { path: PathBuf::from(f), only: None })),
        Some(Err(e)) => log::warn!("[scan] plugin folder list unusable ({e}); scanning the built-in folders only"),
        None => {}
    }
    let previous = match cache_path {
        Some(p) if !force => ScanCache::load(p),
        _ => ScanCache::default(),
    };
    let (found, next, stats) = scan_roots(&roots, &previous, force, |path| {
        let stdout = scan_one_child(&exe, path, SCAN_CHILD_TIMEOUT)?;
        serde_json::from_slice::<ScanOutcome>(&stdout).map_err(|e| ScanError::Bundle(format!("parse failed: {e}")))
    });
    if let Some(p) = cache_path {
        if let Err(e) = next.save(p) {
            log::warn!("[scan] cache not written ({}): {e}", p.display());
        }
    }
    let elapsed_ms = started.elapsed().as_millis();
    log::info!(
        "[scan] {} bundle(s): {} from cache, {} scanned{}, {} failed, {} unsupported, {} plugin(s), {elapsed_ms} ms",
        stats.scanned,
        stats.cached,
        stats.fresh,
        if force { " (forced)" } else { "" },
        stats.failed.len(),
        found.unsupported.len(),
        found.plugins.len()
    );
    if !stats.failed.is_empty() {
        log::warn!(
            "[scan] not listed (rescan from the picker to retry): {}",
            stats.failed.join("; ")
        );
    }
    for plugin in &found.unsupported {
        log::info!("[scan] not listed: {}: {}", plugin.path, plugin.reason);
    }
    if cfg!(debug_assertions) {
        let diag = serde_json::json!({
            "step": "scan",
            "scanned": stats.scanned,
            "cached": stats.cached,
            "fresh": stats.fresh,
            "forced": force,
            "elapsed_ms": elapsed_ms as u64,
            "ok": found.plugins.len(),
            "failed": stats.failed,
            "unsupported": found.unsupported.len(),
        });
        println!("[diag] {diag}");
    }
    // Still under the scan lock: a later scan cannot publish before this one.
    *unsupported.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = found.unsupported;
    Ok(found.plugins)
}
