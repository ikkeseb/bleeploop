//! ASIO startup coordinator: ONE driver probe per process, requested by the frontend AFTER the UI is
//! up, never from `run()`.
//!
//! Why this exists: resolving the ASIO device loads and initialises a third-party driver DLL in-process
//! (`CoCreateInstance` + `ASIOInit`, holding asio-sys' global driver lock). A broken driver hangs or
//! crashes there, and nothing in-process can interrupt it — a timeout only bounds how long the
//! WAITER waits, the driver keeps its locks. So the rules are: (1) no driver contact before the window
//! and Audio Settings exist; (2) the saved "ASIO off" preference is honoured by never asking;
//! (3) a sentinel file marks an attempt in progress, and an attempt that did not complete blocks the
//! automatic probe on the next launch until the user explicitly retries; (4) after a timed-out attempt
//! the process never retries (the driver thread is still inside the DLL) — a restart is required;
//! (5) `--disable-asio` is a launch policy no preference can override.
//!
//! Platform-agnostic and free of cpal so `cargo test` covers the state machine on every build; the
//! ASIO resolver itself lives in `audio_output.rs`.

use serde::Serialize;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::Duration;

/// Mirrors `AsioStartupStatus` in `src/platform/host.ts` (kebab-case over IPC).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AsioStartupStatus {
    /// The binary was built without the `asio` feature.
    NotCompiled,
    /// `--disable-asio` was passed: no probe this launch, whatever the preference says.
    DisabledByFlag,
    /// Nothing has asked for the driver yet (the saved preference is off, or boot has not reached it).
    Unprobed,
    /// A probe thread is inside the driver right now.
    Probing,
    /// The duplex device and both configs are cached; ASIO arms can proceed.
    Ready,
    /// The probe completed and failed (no usable driver, config query failed). Explicitly retryable.
    Failed,
    /// A previous launch's probe never completed (sentinel found). Automatic probing is off until the
    /// user explicitly retries.
    Blocked,
    /// The probe did not return within the deadline. No retry in this process; restart to try again.
    TimedOut,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AsioStatusReport {
    pub status: AsioStartupStatus,
    /// Human-readable reason for `Failed` / `Blocked` / `TimedOut`; empty otherwise.
    pub detail: String,
}

struct Phase {
    status: AsioStartupStatus,
    detail: String,
}

/// The coordinator. `T` is the resolved payload (the ASIO cache); tests use a plain integer.
pub struct Coordinator<T> {
    compiled: bool,
    disabled_by_flag: AtomicBool,
    phase: Mutex<Phase>,
    payload: OnceLock<T>,
}

impl<T: Send + Sync + 'static> Coordinator<T> {
    pub const fn new(compiled: bool) -> Self {
        Self {
            compiled,
            disabled_by_flag: AtomicBool::new(false),
            phase: Mutex::new(Phase { status: AsioStartupStatus::Unprobed, detail: String::new() }),
            payload: OnceLock::new(),
        }
    }

    /// Launch policy from `--disable-asio`. Set once in `run()` before any command can arrive.
    pub fn set_disabled_by_flag(&self) {
        self.disabled_by_flag.store(true, Relaxed);
    }

    /// The published payload (None until a probe succeeded). Never touches the driver.
    pub fn payload(&self) -> Option<&T> {
        self.payload.get()
    }

    /// Current status. Never touches the driver.
    pub fn status(&self) -> AsioStatusReport {
        if !self.compiled {
            return AsioStatusReport { status: AsioStartupStatus::NotCompiled, detail: String::new() };
        }
        if self.disabled_by_flag.load(Relaxed) {
            return AsioStatusReport {
                status: AsioStartupStatus::DisabledByFlag,
                detail: "started with --disable-asio".to_string(),
            };
        }
        let p = self.phase.lock().unwrap_or_else(|e| e.into_inner());
        AsioStatusReport { status: p.status, detail: p.detail.clone() }
    }

    /// Run the probe if the state machine allows it, and wait up to `timeout` for the result.
    ///
    /// `explicit` = a user action (the Audio Settings toggle / Retry), which may proceed past a
    /// sentinel or a completed failure; the boot-time request passes `false`. `sentinel` is the
    /// attempt-in-progress marker: written before the resolver runs, removed only when a result is
    /// published. A late result after a timeout is discarded and logged, never published.
    pub fn probe<F>(
        &'static self,
        sentinel: &Path,
        explicit: bool,
        resolver: F,
        timeout: Duration,
    ) -> AsioStatusReport
    where
        F: FnOnce() -> Result<T, String> + Send + 'static,
    {
        use AsioStartupStatus::*;
        if !self.compiled || self.disabled_by_flag.load(Relaxed) {
            return self.status();
        }
        {
            let mut p = self.phase.lock().unwrap_or_else(|e| e.into_inner());
            // Report from the held guard: `self.status()` would re-lock `phase` (std Mutex is not
            // reentrant) — that self-deadlock hung the first test run of this file.
            let current = AsioStatusReport { status: p.status, detail: p.detail.clone() };
            match p.status {
                Ready | Probing | TimedOut => return current,
                Failed | Blocked if !explicit => return current,
                Unprobed | Failed | Blocked => {}
                NotCompiled | DisabledByFlag => return current,
            }
            if !explicit && sentinel.exists() {
                p.status = Blocked;
                p.detail = "the previous ASIO start did not complete".to_string();
                log::warn!("[asio] probe blocked: sentinel {} exists from an earlier launch", sentinel.display());
                return AsioStatusReport { status: p.status, detail: p.detail.clone() };
            }
            if let Some(dir) = sentinel.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Err(e) = std::fs::write(sentinel, b"asio probe in progress\n") {
                p.status = Failed;
                p.detail = format!("could not record the attempt ({e}); ASIO skipped");
                log::error!("[asio] sentinel write failed at {}: {e}", sentinel.display());
                return AsioStatusReport { status: p.status, detail: p.detail.clone() };
            }
            p.status = Probing;
            p.detail.clear();
        }
        log::info!("[asio] probe starting (explicit={explicit}); sentinel {}", sentinel.display());
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let sentinel_owned = sentinel.to_path_buf();
        let spawned = std::thread::Builder::new().name("lf-asio-probe".into()).spawn(move || {
            let result = resolver();
            let mut p = self.phase.lock().unwrap_or_else(|e| e.into_inner());
            if p.status != Probing {
                log::warn!(
                    "[asio] late probe result discarded (status {:?}): {}",
                    p.status,
                    match &result { Ok(_) => "ok".to_string(), Err(e) => e.clone() }
                );
                return;
            }
            match result {
                Ok(v) => {
                    let _ = self.payload.set(v);
                    p.status = Ready;
                    log::info!("[asio] probe ready");
                }
                Err(e) => {
                    p.status = Failed;
                    log::warn!("[asio] probe failed: {e}");
                    p.detail = e;
                }
            }
            if let Err(e) = std::fs::remove_file(&sentinel_owned) {
                log::warn!("[asio] sentinel remove failed at {}: {e}", sentinel_owned.display());
            }
            drop(p);
            let _ = done_tx.send(());
        });
        if let Err(e) = spawned {
            let mut p = self.phase.lock().unwrap_or_else(|e| e.into_inner());
            p.status = Failed;
            p.detail = format!("could not start the probe thread: {e}");
            let _ = std::fs::remove_file(sentinel);
            return AsioStatusReport { status: p.status, detail: p.detail.clone() };
        }
        if done_rx.recv_timeout(timeout).is_err() {
            let mut p = self.phase.lock().unwrap_or_else(|e| e.into_inner());
            if p.status == Probing {
                p.status = TimedOut;
                p.detail = format!(
                    "the ASIO driver did not respond within {} s; restart BleepLoop to try again",
                    timeout.as_secs()
                );
                log::error!("[asio] probe timed out after {} s; sentinel kept", timeout.as_secs());
            }
        }
        self.status()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    fn temp_sentinel(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lf-asio-startup-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("asio-probe-in-progress")
    }

    fn fresh() -> &'static Coordinator<u32> {
        Box::leak(Box::new(Coordinator::<u32>::new(true)))
    }

    #[test]
    fn success_publishes_payload_and_clears_the_sentinel() {
        let c = fresh();
        let s = temp_sentinel("ok");
        let r = c.probe(&s, false, || Ok(7), Duration::from_secs(2));
        assert_eq!(r.status, AsioStartupStatus::Ready);
        assert_eq!(c.payload(), Some(&7));
        assert!(!s.exists(), "sentinel must be removed after a published result");
        // A second request is a no-op that does not run the resolver.
        let r2 = c.probe(&s, true, || panic!("must not run"), Duration::from_secs(2));
        assert_eq!(r2.status, AsioStartupStatus::Ready);
    }

    #[test]
    fn failure_needs_an_explicit_retry() {
        let c = fresh();
        let s = temp_sentinel("fail");
        let r = c.probe(&s, false, || Err("no usable driver".into()), Duration::from_secs(2));
        assert_eq!(r.status, AsioStartupStatus::Failed);
        assert_eq!(r.detail, "no usable driver");
        assert!(!s.exists());
        let r = c.probe(&s, false, || panic!("implicit retry must not run"), Duration::from_secs(2));
        assert_eq!(r.status, AsioStartupStatus::Failed);
        let r = c.probe(&s, true, || Ok(1), Duration::from_secs(2));
        assert_eq!(r.status, AsioStartupStatus::Ready);
        assert_eq!(c.payload(), Some(&1));
    }

    #[test]
    fn sentinel_blocks_the_automatic_probe_but_not_an_explicit_one() {
        let c = fresh();
        let s = temp_sentinel("blocked");
        std::fs::create_dir_all(s.parent().unwrap()).unwrap();
        std::fs::write(&s, b"stale").unwrap();
        let r = c.probe(&s, false, || panic!("blocked probe must not run"), Duration::from_secs(2));
        assert_eq!(r.status, AsioStartupStatus::Blocked);
        assert!(s.exists(), "a blocked probe leaves the sentinel for the explicit retry");
        assert_eq!(c.status().status, AsioStartupStatus::Blocked);
        let r = c.probe(&s, true, || Ok(3), Duration::from_secs(2));
        assert_eq!(r.status, AsioStartupStatus::Ready);
        assert!(!s.exists());
    }

    #[test]
    fn timeout_keeps_the_sentinel_and_discards_the_late_result() {
        let c = fresh();
        let s = temp_sentinel("hang");
        let (release_tx, release_rx) = channel::<()>();
        let r = c.probe(
            &s,
            false,
            move || {
                release_rx.recv().ok();
                Ok(99)
            },
            Duration::from_millis(80),
        );
        assert_eq!(r.status, AsioStartupStatus::TimedOut);
        assert!(s.exists(), "a timed-out attempt must leave the sentinel so the next launch is blocked");
        assert_eq!(c.payload(), None);
        // The driver thread eventually returns: its result must never be published.
        release_tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while s.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(c.payload(), None, "late result after timeout must be discarded");
        assert_eq!(c.status().status, AsioStartupStatus::TimedOut);
        assert!(s.exists(), "late thread must not clear the sentinel after a timeout");
        // And no retry in this process, even explicit.
        let r = c.probe(&s, true, || panic!("no retry after timeout"), Duration::from_secs(1));
        assert_eq!(r.status, AsioStartupStatus::TimedOut);
    }

    #[test]
    fn flag_and_not_compiled_never_run_the_resolver() {
        let c = fresh();
        c.set_disabled_by_flag();
        let s = temp_sentinel("flag");
        let r = c.probe(&s, true, || panic!("flag must win"), Duration::from_secs(1));
        assert_eq!(r.status, AsioStartupStatus::DisabledByFlag);
        let nc: &'static Coordinator<u32> = Box::leak(Box::new(Coordinator::new(false)));
        let r = nc.probe(&s, true, || panic!("not compiled"), Duration::from_secs(1));
        assert_eq!(r.status, AsioStartupStatus::NotCompiled);
        assert!(!s.exists());
    }

    #[test]
    fn a_concurrent_request_while_probing_runs_no_second_resolver() {
        let c = fresh();
        let s = temp_sentinel("concurrent");
        let s_for_thread = s.clone();
        let (release_tx, release_rx) = channel::<()>();
        let first = std::thread::spawn(move || {
            c.probe(
                &s_for_thread,
                false,
                move || {
                    release_rx.recv().ok();
                    Ok(5)
                },
                Duration::from_secs(5),
            )
        });
        while c.status().status != AsioStartupStatus::Probing {
            std::thread::sleep(Duration::from_millis(5));
        }
        let second = c.probe(&s, true, || panic!("second resolver must not run"), Duration::from_secs(1));
        assert_eq!(second.status, AsioStartupStatus::Probing);
        release_tx.send(()).unwrap();
        assert_eq!(first.join().unwrap().status, AsioStartupStatus::Ready);
        assert_eq!(c.payload(), Some(&5));
    }
}
