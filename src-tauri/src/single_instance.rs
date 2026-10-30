//! One running app per identifier, in release builds: two would share one IndexedDB and tone folder,
//! each with its own cache, so one's CLEAR ALL could delete the other's recovery. `run()` asks
//! `first_launch` before Tauri starts, and a launch that is refused ends there.
//!
//! The lock is the mere existence of a named kernel mutex, never who owns it. The first keeps its handle
//! to the end, so the name lasts as long as the process does however it ends: the kernel frees it, and a
//! crash can never lock the player out. A process that quits shows no window while it shuts down, so a
//! later launch tells it from a running app by looking for the main window (its class is fixed in
//! `tauri.conf.json`). A hung first must not hang the launch: nothing here sends the first a message.
//! A holder with no window to the bound is a leftover, and one launch takes over from it, under a second
//! name, so one leftover can never lock the player out.
//!
//! Two gaps stand, accepted. The lookup finds any window of the class, so a dev or smoke build beside the
//! release app can take the focus; the launch is still right to exit, and only the owner's machine meets
//! it. And a first that takes over 10 s to show its window can end up beside the launch that took over.
//!
//! `verify/guards/single-instance.mjs` holds the wiring. Runs before logging exists, so it logs nothing.

use std::thread::sleep;
use std::time::{Duration, Instant};

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HWND};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, IsIconic, IsWindowVisible, SetForegroundWindow, ShowWindowAsync, SW_RESTORE,
};

/// The main window's class: `windowClassname` in `tauri.conf.json`.
const WINDOW_CLASS: PCWSTR = w!("BleepLoopMainWindow");

/// How long a launch waits for a holder that shows no window. The old process's window goes first (its
/// destruction is what ends the event loop, and `RunEvent::Exit` comes after that), so a reopen during a
/// quit waits here through `EngineApp::shutdown`, 8 s at worst (`SHUTDOWN_WAIT`, `engine_io/mode.rs`).
/// After it only Tauri's cleanup and `process::exit` run, in milliseconds: 2 s covers those.
const QUIT_WAIT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(100);

/// What this launch does.
#[derive(Debug, PartialEq, Eq)]
pub enum Claim {
    /// Start: this process holds the name, for good.
    Run,
    /// The app already runs: end here.
    Exit,
}

/// The holder's window, from the lookup.
pub enum Found<W> {
    Nothing,
    /// There, not shown: a first still starting, or stuck.
    Hidden,
    Visible(W),
}

/// Open the name: the handle, and whether this call made the mutex. A failed open (a holder another user
/// made refuses the handle, say) has no handle and made nothing: it reads as held, so it waits like one.
fn open(name: &HSTRING) -> (Option<HANDLE>, bool) {
    // SAFETY: `name` outlives the call; no security attributes, so a child inherits no handle.
    let opened = unsafe { CreateMutexW(None, false, name) };
    // SAFETY: reads this thread's last error, which the call above just set.
    let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    match opened {
        Ok(handle) => (Some(handle), !existed),
        Err(_) => (None, false),
    }
}

fn close(handle: Option<HANDLE>) {
    if let Some(handle) = handle {
        // SAFETY: `open` returned this handle and nothing else closes it.
        let _ = unsafe { CloseHandle(handle) };
    }
}

/// Decide this launch: `name` is the lock, `takeover` the one takeover from a leftover, `find` looks for
/// the holder's window every `poll`, for `bound`, and `focus` brings a visible one forward. Waiters
/// converge: one makes the mutex once the holder is gone, and the others then meet it and its window.
pub fn claim<W>(
    name: &HSTRING,
    takeover: &HSTRING,
    bound: Duration,
    poll: Duration,
    mut find: impl FnMut() -> Found<W>,
    focus: impl FnOnce(W),
) -> Claim {
    let deadline = Instant::now() + bound;
    loop {
        let (handle, made) = open(name);
        if made {
            return Claim::Run; // The handle stays open to the process's end.
        }
        let found = find();
        let waited = Instant::now() >= deadline;
        match found {
            Found::Visible(window) => {
                close(handle);
                focus(window);
                return Claim::Exit;
            }
            Found::Hidden if waited => {
                close(handle);
                return Claim::Exit;
            }
            // A holder stuck with no window. A launch is never locked out by a leftover process:
            // availability beats exclusivity here, for one launch after the bound, the one that makes
            // `takeover`. Keeping both handles keeps the name once the leftover dies, so a later launch
            // still meets this one; the rest end here. A failed open comes here the same way.
            Found::Nothing if waited => {
                let (second, made) = open(takeover);
                if made {
                    return Claim::Run; // Both handles stay open to the process's end.
                }
                close(second);
                close(handle);
                return Claim::Exit;
            }
            // Closed before the wait: an open handle here would keep the name alive past the holder.
            _ => {
                close(handle);
                sleep(poll);
            }
        }
    }
}

/// The app's window, from any process. Neither call waits on the window's thread.
fn find_window() -> Found<HWND> {
    // SAFETY: a static class name and no title; a miss is an `Err`.
    match unsafe { FindWindowW(WINDOW_CLASS, PCWSTR::null()) } {
        // SAFETY: only reads the window's style.
        Ok(window) if unsafe { IsWindowVisible(window) }.as_bool() => Found::Visible(window),
        Ok(_) => Found::Hidden,
        Err(_) => Found::Nothing,
    }
}

/// Bring the first's window forward, restored if minimised: the asynchronous call, so a hung first cannot
/// hold this launch.
fn focus_window(window: HWND) {
    // SAFETY: a window that has gone since the lookup only fails these calls, and neither failure matters.
    unsafe {
        if IsIconic(window).as_bool() {
            let _ = ShowWindowAsync(window, SW_RESTORE);
        }
        let _ = SetForegroundWindow(window);
    }
}

/// This launch of the app named `identifier`.
pub fn first_launch(identifier: &str) -> Claim {
    let name = HSTRING::from(format!("Local\\{identifier}.single-instance"));
    let takeover = HSTRING::from(format!("Local\\{identifier}.single-instance.takeover"));
    claim(&name, &takeover, QUIT_WAIT, POLL, find_window, focus_window)
}

#[cfg(all(test, windows))]
mod tests {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};

    use super::*;

    /// For a case that runs to the bound.
    const SHORT: Duration = Duration::from_millis(30);
    /// For a case that ends early: far past what it takes, so a loaded machine cannot reach it.
    const LONG: Duration = Duration::from_secs(5);
    const TICK: Duration = Duration::from_millis(2);

    /// The real kernel mutex, under a name of this test's alone.
    fn name(test: &str) -> HSTRING {
        HSTRING::from(format!("Local\\bleeploop-test-{}-{test}", std::process::id()))
    }

    /// The takeover name beside `name`.
    fn takeover(name: &HSTRING) -> HSTRING {
        HSTRING::from(format!("{name}.takeover"))
    }

    /// The first's hold on `name`.
    fn hold(name: &HSTRING) -> HANDLE {
        let (handle, made) = open(name);
        assert!(made, "the name starts free");
        handle.unwrap()
    }

    /// The name exists; a second open in one process reports that while a handle is open.
    fn taken(name: &HSTRING) -> bool {
        let (handle, made) = open(name);
        close(handle);
        !made
    }

    /// How many handles on `name` the kernel counts, this look's own left out.
    fn handles(name: &HSTRING) -> u32 {
        /// `PUBLIC_OBJECT_BASIC_INFORMATION`, class 0; the crate's binding sits behind a feature off here.
        #[repr(C)]
        #[derive(Default)]
        struct Basic {
            attributes: u32,
            access: u32,
            handles: u32,
            rest: [u32; 11],
        }
        #[link(name = "ntdll")]
        unsafe extern "system" {
            fn NtQueryObject(handle: HANDLE, class: u32, info: *mut Basic, size: u32, written: *mut u32) -> i32;
        }
        let (handle, _) = open(name);
        let mut info = Basic::default();
        let size = size_of::<Basic>() as u32;
        // SAFETY: `info` is that class's buffer, of `size` bytes, and the handle is open.
        let status = unsafe { NtQueryObject(handle.unwrap(), 0, &mut info, size, std::ptr::null_mut()) };
        close(handle);
        assert_eq!(status, 0, "the kernel answers");
        info.handles - 1
    }

    /// `claim` where nothing may be focused.
    fn unfocused(name: &HSTRING, bound: Duration, find: impl FnMut() -> Found<()>) -> Claim {
        claim(name, &takeover(name), bound, TICK, find, |()| panic!("nothing to focus"))
    }

    /// `claim`'s answer with `find`, and how many times it focused.
    fn focused(name: &HSTRING, find: impl FnMut() -> Found<()>) -> (Claim, u32) {
        let times = Cell::new(0);
        let claim = claim(name, &takeover(name), LONG, TICK, find, |()| times.set(times.get() + 1));
        (claim, times.get())
    }

    #[test]
    fn a_free_name_runs_and_holds_it() {
        let name = name("free");
        assert_eq!(unfocused(&name, SHORT, || panic!("nothing to look for")), Claim::Run);
        assert!(taken(&name));
    }

    #[test]
    fn a_visible_window_exits_and_focuses_once() {
        let name = name("visible");
        let first = hold(&name);
        let looks = Cell::new(0);
        let find = || {
            looks.set(looks.get() + 1);
            Found::Visible(())
        };
        assert_eq!(focused(&name, find), (Claim::Exit, 1));
        assert_eq!(looks.get(), 1, "a visible window ends the wait at once");
        // The refused launch left no handle behind: the name goes with the first.
        close(Some(first));
        assert!(!taken(&name));
    }

    #[test]
    fn a_holder_gone_inside_the_bound_runs_and_holds_it() {
        let name = name("released");
        let first = hold(&name);
        let looks = Cell::new(0);
        let find = || {
            looks.set(looks.get() + 1);
            if looks.get() == 2 {
                close(Some(first));
            }
            Found::Nothing
        };
        assert_eq!(unfocused(&name, LONG, find), Claim::Run);
        assert_eq!(looks.get(), 2, "the next poll after the holder went takes the name");
        assert!(taken(&name));
    }

    #[test]
    fn no_window_to_the_bound_runs_and_keeps_the_name() {
        let name = name("no-window");
        let leftover = hold(&name);
        let started = Instant::now();
        assert_eq!(unfocused(&name, SHORT, || Found::Nothing), Claim::Run);
        assert!(started.elapsed() >= SHORT, "it waited out the bound");
        close(Some(leftover));
        assert!(taken(&name), "the name outlives the leftover, so a later launch meets this one");
    }

    #[test]
    fn a_hidden_window_to_the_bound_exits() {
        let name = name("hidden");
        let first = hold(&name);
        let started = Instant::now();
        assert_eq!(unfocused(&name, SHORT, || Found::Hidden), Claim::Exit);
        assert!(started.elapsed() >= SHORT, "it waited out the bound");
        close(Some(first));
        assert!(!taken(&name), "the refused launch left no handle behind");
    }

    #[test]
    fn a_window_that_appears_during_the_wait_exits_with_focus() {
        let name = name("appears");
        let _first = hold(&name);
        let looks = Cell::new(0);
        let find = || {
            looks.set(looks.get() + 1);
            if looks.get() < 3 { Found::Nothing } else { Found::Visible(()) }
        };
        assert_eq!(focused(&name, find), (Claim::Exit, 1));
    }

    #[test]
    fn a_window_that_goes_visible_during_the_wait_exits_with_focus() {
        let name = name("shown");
        let _first = hold(&name);
        let looks = Cell::new(0);
        let find = || {
            looks.set(looks.get() + 1);
            if looks.get() < 3 { Found::Hidden } else { Found::Visible(()) }
        };
        assert_eq!(focused(&name, find), (Claim::Exit, 1));
    }

    #[test]
    fn two_waiters_converge_on_one_winner() {
        let name = name("waiters");
        let first = hold(&name);
        let won = AtomicBool::new(false);
        let waiter = || {
            let find = || if won.load(SeqCst) { Found::Visible(()) } else { Found::Nothing };
            let claim = claim(&name, &takeover(&name), LONG, TICK, find, |()| {});
            if claim == Claim::Run {
                won.store(true, SeqCst);
            }
            claim
        };
        let claims = std::thread::scope(|scope| {
            let (one, other) = (scope.spawn(waiter), scope.spawn(waiter));
            sleep(TICK * 5);
            close(Some(first));
            [one.join().unwrap(), other.join().unwrap()]
        });
        assert_eq!(claims.iter().filter(|claim| **claim == Claim::Run).count(), 1, "{claims:?}");
    }

    #[test]
    fn launches_at_the_bound_with_a_holder_that_stays_leave_one_winner() {
        let name = name("takeover");
        let takeover = takeover(&name);
        let leftover = hold(&name);
        let claims: Vec<Claim> = std::thread::scope(|scope| {
            let launch = || unfocused(&name, SHORT, || Found::Nothing);
            let launches: Vec<_> = (0..4).map(|_| scope.spawn(launch)).collect();
            launches.into_iter().map(|launch| launch.join().unwrap()).collect()
        });
        assert_eq!(claims.iter().filter(|claim| **claim == Claim::Run).count(), 1, "{claims:?}");
        assert_eq!(handles(&takeover), 1, "the winner alone holds the takeover");
        assert_eq!(handles(&name), 2, "the leftover and the winner hold the name");
        close(Some(leftover));
        assert_eq!(handles(&name), 1, "the name outlives the leftover, on the winner's handle alone");
    }
}
