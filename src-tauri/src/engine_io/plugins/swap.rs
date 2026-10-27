//! OWNS: a slot's hot swap, the transaction behind `plugin_swap`: the slot's plugin plays on while the
//! next one loads beside it (`engine_slot::Entry::Replace`: staged muted, committed, crossfaded to),
//! and one `finish` moves the slot on whatever the load did: the new plugin plays and the one it
//! replaced retires on a thread of its own (`plugin:retired`), started before the swap takes the slot
//! (none starts: refused busy) and handed that plugin at the commit, or the slot goes back to the plugin
//! that plays, or every plugin nobody wants any more leaves through `Leaving`, as an unload's would
//! (whose failed live-off keeps the plugin the engine plays). At the commit the engine releases what
//! sounds on the slot and, when the plugin's kind changed, takes the slot off live (`lf_engine::slots`).
//!
//! The exit (`begin_closing`) refuses every swap from then on and calls off one in flight, whose
//! `finish` ends both its plugins: the exit's `unload_all` starts the playing one's teardown at once,
//! and waits for the swap to empty the slot.
//!
//! A swap takes a slot that plays a plugin for the asking document, once the plugin swapped out of it
//! before has ended its retire (`take_slot`). A swap onto the file that plays is refused: that one goes
//! cold (the UI routes it so; this is the backstop).

use std::sync::atomic::{AtomicBool, Ordering::{Acquire, Relaxed}};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, PoisonError};
use std::thread::JoinHandle;
use std::time::Instant;

use tauri::Emitter;

use super::{describe, format_of, lock, take_slot, vacate, window_open, EngineSlot, LoadTarget, Retiring, SlotsGuard, LIVE_OFF_WAIT};
use crate::engine_io::mode::EngineApp;
use crate::host::engine_slot::{EngineSlotHandle, Entry, SwapBounds};
use crate::host::{PluginHostState, PluginInfo};

/// The `plugin:retired` payload: the instance a swap replaced has ended its retire; `released`: its
/// plugin was released in order (else it was left loaded, or its owner's end is unknown).
#[derive(Clone, serde::Serialize)]
pub(super) struct Retired {
    pub(super) slot: u8,
    pub(super) instance: u64,
    pub(super) released: bool,
}

#[cfg(test)]
thread_local! {
    /// Test-only: runs once on the swap's thread, where a failed live-off's rollback is about to take
    /// the slots lock.
    pub(super) static ROLLBACK: std::cell::Cell<Option<Box<dyn FnOnce()>>> = const { std::cell::Cell::new(None) };
}

/// Where a retire reports its end: the window's `plugin:retired`, a test's record otherwise.
pub(super) type RetiredSink = Arc<dyn Fn(Retired) + Send + Sync>;

/// Where a swap lands: a load's target (with the app's exit flag), and where a retire reports.
pub(super) struct SwapTarget<'a> {
    pub(super) load: LoadTarget<'a>,
    pub(super) retired: RetiredSink,
}

impl SwapTarget<'_> {
    /// Swap `slot`'s plugin for plugin `id` from `path`, crossfaded to at `gain`, for the document with
    /// `frontend_epoch`; `tone_token` as a load's. The new plugin's info.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn swap(
        self,
        state: &PluginHostState,
        slot: u8,
        path: String,
        id: String,
        gain: f32,
        frontend_epoch: u32,
        tone_token: Option<u32>,
    ) -> Result<PluginInfo, String> {
        let format = format_of(&path)?;
        // Before the reservation: a swap with no thread to retire the plugin it replaces changes nothing.
        let retirer = spawn_retire(slot, self.retired.clone())?;
        let cancel = self.reserve_swap(state, slot, frontend_epoch, &path)?;
        // Past the reservation, so nothing below may return early: `finish` moves the slot on.
        let tone = self.load.tone(slot, format, &path, &id, tone_token);
        let began = Instant::now();
        let entry = Entry::Replace { gain, cancel: cancel.clone(), bounds: SwapBounds::default() };
        let loaded = (self.load.open)(format, path.clone(), id.clone(), self.load.host.slot(usize::from(slot)), tone, entry);
        let info = self.finish(state, slot, &cancel, retirer, loaded.map(|new| (describe(slot, format, path, id, &new), new)))?;
        log::info!(
            "[plugin_host] engine slot {slot}: {} swapped in as instance {} in {} ms",
            info.descriptor.name,
            info.instance,
            began.elapsed().as_millis()
        );
        Ok(info)
    }

    /// Take `slot` for a swap onto `path`: it must play a plugin for this document, the exit not begun;
    /// a plugin swapped out of it still retiring is waited for (≤ `vacate_wait`). The swap's cancel.
    fn reserve_swap(&self, state: &PluginHostState, slot: u8, frontend_epoch: u32, path: &str) -> Result<Arc<AtomicBool>, String> {
        take_slot(self.load.slots, slot, (state, frontend_epoch), self.load.vacate_wait, |taken, retiring| {
            if self.load.closing.load(Acquire) {
                return Some(Err("busy: closing".to_string()));
            }
            let EngineSlot::Loaded { info, .. } = &*taken else {
                return Some(Err(format!("no plugin plays in slot {slot}; load it")));
            };
            if same_file(&info.descriptor.path, path) {
                return Some(Err("same plugin file: swap cold".to_string()));
            }
            if retiring {
                return None;
            }
            let EngineSlot::Loaded { handle, info } = std::mem::take(taken) else { unreachable!("the slot was loaded") };
            let cancel = Arc::new(AtomicBool::new(false));
            *taken = EngineSlot::Swapping { epoch: frontend_epoch, playing: handle, info, cancel: cancel.clone() };
            Some(Ok(cancel))
        })
    }

    /// Move the slot on from the swap `cancel` names, under the lock, whatever its load did (`loaded`:
    /// the new plugin with its info). Wanted (the slot still swaps for it, uncancelled, its document
    /// current, the exit not begun), the new plugin plays and the one it replaced retires on `retirer`
    /// (`play`); a failed swap gives the slot back to the plugin that plays, unless that one was retired
    /// meanwhile (a commit the load never reported). Every plugin this thread owns that nobody wants
    /// leaves (`call_off`). A `retirer` handed nothing ends as it drops.
    fn finish(
        &self,
        state: &PluginHostState,
        slot: u8,
        cancel: &Arc<AtomicBool>,
        retirer: Retirer,
        loaded: Result<(PluginInfo, EngineSlotHandle), String>,
    ) -> Result<PluginInfo, String> {
        let mut slots = lock(self.load.slots)?;
        let entry = &mut slots[usize::from(slot)];
        // Still this swap's: `playing` is this thread's from here.
        let ours = match std::mem::replace(&mut entry.state, EngineSlot::Leaving) {
            EngineSlot::Swapping { epoch, playing, info, cancel: c } if Arc::ptr_eq(&c, cancel) => Some((epoch, playing, info)),
            other => {
                entry.state = other;
                None
            }
        };
        let wanted = |epoch: u32| !cancel.load(Acquire) && state.frontend_epoch.load(Relaxed) == epoch && !self.load.closing.load(Acquire);
        let (old, new, error) = match (ours, loaded) {
            (Some((epoch, old, _)), Ok((info, new))) if wanted(epoch) => {
                self.play(slots, slot, info.clone(), new, old, retirer);
                return Ok(info);
            }
            (Some((epoch, old, info)), Err(e)) if wanted(epoch) && !old.retired() => {
                entry.state = EngineSlot::Loaded { handle: old, info };
                return Err(e);
            }
            (Some((epoch, old, info)), Err(e)) if wanted(epoch) => {
                (Some((old, info)), None, format!("{e}; the plugin it was to replace was retired meanwhile, so slot {slot} is empty"))
            }
            (ours, Ok(new)) => {
                let error = format!("plugin swap for slot {slot} was superseded by a frontend reload, an unload or the exit");
                (ours.map(|(_, old, info)| (old, info)), Some(new), error)
            }
            (ours, Err(e)) => (ours.map(|(_, old, info)| (old, info)), None, e),
        };
        drop(slots);
        self.call_off(slot, old, new, error, retirer)
    }

    /// `new` plays in `slot` (`slots`, locked) from here, as `info`, and `old`, the owner it replaced,
    /// retires on `retirer`'s thread, its mark kept until that has ended so no swap or cold load takes
    /// the slot before.
    fn play(&self, mut slots: SlotsGuard<'_>, slot: u8, info: PluginInfo, new: EngineSlotHandle, old: Arc<EngineSlotHandle>, retirer: Retirer) {
        let entry = &mut slots[usize::from(slot)];
        entry.state = EngineSlot::Loaded { handle: Arc::new(new), info };
        entry.retiring = Some(Retiring { instance: old.instance(), join: retirer.join });
        drop(slots);
        // Its thread waits on this from its start: the send fails only if it panicked, and then the
        // handle's drop here tears the owner down.
        let _ = retirer.old.send(old);
    }

    /// Tear down what a called-off swap leaves this thread, `error` saying why: the plugin it was to
    /// replace (`old`, with its info; the slot is this thread's, `Leaving`, while it is held) and the
    /// new one, each through its owner's ordered teardown, the slot off live first; both teardowns start
    /// before either is joined, so a release that hangs holds up only its own. A live-off that fails
    /// keeps the plugin the engine plays loaded, as an unload's does (`unload`; the one it replaced
    /// retires on `retirer`), and only the other leaves; with neither playing, the slot ends empty and
    /// may pass its input dry. Once the exit has begun (seen under the slots lock, so its snapshot may
    /// hold the slot `Leaving`), nothing is put back: both leave and the slot ends empty.
    fn call_off(
        &self,
        slot: u8,
        old: Option<(Arc<EngineSlotHandle>, PluginInfo)>,
        new: Option<(PluginInfo, EngineSlotHandle)>,
        error: String,
        retirer: Retirer,
    ) -> Result<PluginInfo, String> {
        let unload_new = |new: Option<(PluginInfo, EngineSlotHandle)>| {
            if let Some(Err(e)) = new.map(|(_, new)| new.unload()) {
                log::error!("[plugin_host] engine slot {slot}: unloading a superseded swap's plugin: {e}");
            }
        };
        let Some((old, old_info)) = old else {
            drop(retirer);
            unload_new(new);
            return Err(error);
        };
        let error = match self.load.host.silence_slot(slot, LIVE_OFF_WAIT) {
            Ok(()) => error,
            Err(e) => {
                #[cfg(test)]
                ROLLBACK.with(|hook| hook.take().map(|hook| hook()));
                let mut slots = self.load.slots.lock().unwrap_or_else(PoisonError::into_inner);
                // `begin_closing` sets the flag before it takes this lock; the exit's snapshot comes after.
                if self.load.closing.load(Acquire) {
                    drop(slots);
                    log::error!("[plugin_host] engine slot {slot}: live-off before a called-off swap's unloads: {e}");
                    error
                } else {
                    let stays = format!("{error}; the plugin playing in slot {slot} stays loaded, as its live input could not be stopped first: {e}");
                    if let Some((info, new)) = new {
                        self.play(slots, slot, info, new, old, retirer);
                        return Err(stays);
                    }
                    if !old.retired() {
                        slots[usize::from(slot)].state = EngineSlot::Loaded { handle: old, info: old_info };
                        return Err(stays);
                    }
                    format!("{error}; slot {slot} is empty and may pass its input dry, as its live input could not be stopped first: {e}")
                }
            }
        };
        drop(retirer);
        if let Some((_, new)) = &new {
            new.stop();
        }
        old.stop();
        unload_new(new);
        if let Err(e) = old.unload() {
            log::error!("[plugin_host] engine slot {slot}: unloading the plugin a called-off swap was to replace: {e}");
        }
        vacate(self.load.slots, slot);
        Err(error)
    }
}

/// A swap's retire thread (`spawn_retire`), started before the swap takes its slot: `old` hands it the
/// owner the commit replaced. Dropped unhanded (a swap refused or called off), it ends at once, reporting
/// nothing.
struct Retirer {
    old: Sender<Arc<EngineSlotHandle>>,
    join: JoinHandle<bool>,
}

/// Start `slot`'s retire thread (`lf-retire-{slot}`), refused busy when none starts. It touches no slot
/// state: the slot's next reservation reaps it.
fn spawn_retire(slot: u8, retired: RetiredSink) -> Result<Retirer, String> {
    let (old, handed) = channel::<Arc<EngineSlotHandle>>();
    let join = std::thread::Builder::new()
        .name(format!("lf-retire-{slot}"))
        .spawn(move || handed.recv().is_ok_and(|old| retire(slot, &old, &retired)))
        .map_err(|e| format!("busy: no retire thread for slot {slot}: {e}"))?;
    Ok(Retirer { old, join })
}

/// Stop and join the replaced owner through the handle commands may still share (its own ordered
/// teardown, `EngineSlotHandle::unload`), then report `plugin:retired`. Whether it released its
/// plugin in order.
fn retire(slot: u8, old: &EngineSlotHandle, retired: &RetiredSink) -> bool {
    let instance = old.instance();
    let released = match old.unload() {
        Ok(()) => true,
        Err(e) => {
            log::error!("[plugin_host] engine slot {slot}: retiring instance {instance}: {e}");
            false
        }
    };
    retired(Retired { slot, instance, released });
    released
}

/// Whether two plugin paths name the same file: case aside, `/` as `\`, nothing more (no path is ever
/// canonicalised: `src-tauri/AGENTS.md` § Plugin hosting).
fn same_file(a: &str, b: &str) -> bool {
    fn folded(path: &str) -> impl Iterator<Item = char> + '_ {
        path.chars().flat_map(char::to_lowercase).map(|c| if c == '/' { '\\' } else { c })
    }
    folded(a).eq(folded(b))
}

impl EngineApp {
    /// `plugin_swap`: swap `slot`'s plugin for plugin `id` from `path` (its load ≤ 15 s, then the
    /// commit), crossfaded to at `gain` (the UI's saved level for it), for the document with
    /// `frontend_epoch`; `tone_token` as `plugin_load`'s. At the commit the engine releases what sounds
    /// on the slot and, when the plugin's kind changed, takes the slot off live.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn plugin_swap(
        &self,
        state: &PluginHostState,
        window: &tauri::WebviewWindow,
        slot: u8,
        path: String,
        id: String,
        gain: f32,
        frontend_epoch: u32,
        tone_token: Option<u32>,
    ) -> Result<PluginInfo, String> {
        let host = self.host()?;
        let open = window_open(window, slot);
        let events = window.clone();
        let retired: RetiredSink = Arc::new(move |retired| {
            let _ = events.emit("plugin:retired", retired);
        });
        let target = SwapTarget { load: self.load_target(&host, &open), retired };
        target.swap(state, slot, path, id, gain, frontend_epoch, tone_token)
    }
}
