//! OWNS: one slot's bookkeeping between its plugin owners and the engine's port ([`Desk`], one per
//! slot in `Core::ports`, under that lock): each owner's standing, whether its unit is in the engine,
//! and the replies the engine sent it that it has not read yet (its mail). A Retiring owner's entry
//! outlives its unit's return until the owner drops (`forget`): it reads Retiring from the commit on.
//!
//! [`Desk::pump`] reads the port and routes each reply by its tag (the owner's token):
//! - `Committed { t }`: every other Active owner becomes Retiring, wherever its unit is (in the engine,
//!   or waiting in mail after an eviction): its role ends at the commit, not when its unit comes back.
//!   A replace's gain enters the settings replay. The answer is mailed to `t`, unless `t` gave up
//!   waiting: then its unit is removed at once and leaks when it comes back.
//! - A unit handed back leaves the engine; it is mailed to its owner, or leaked (`mem::forget`) when its
//!   owner gave up on it or has no entry.
//! - `NotHeld { t }` answers a withdraw that lost the race to a commit (dropped here) or a remove (mailed).
//!
//! Nothing here drops a unit: a drop frees memory and calls into the plugin's DLL off its owner thread.

use std::collections::VecDeque;
use std::sync::Mutex;

use lf_engine::{Command, Handback, HandbackReason, SlotPort, SlotProcessor, SlotReply};

use crate::engine_io::settings::Settings;

/// An owner's standing on its slot. An owner with no entry is released: nothing of it is in the engine
/// or waits in its mail, and it was never retired (or has dropped).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    /// Its unit plays, stages, or waits to be installed again after an eviction.
    Active,
    /// Another owner's replace committed: its unit fades out (or waits in mail, or is back) and never goes
    /// in again.
    Retiring,
    /// It gave up on its unit (and leaks its plugin), or the unit was lost with a leaked engine: whatever
    /// comes back of it leaks here.
    Abandoned,
}

struct OwnerEntry {
    tag: u64,
    standing: Standing,
    /// Its unit is in the engine: sent, not yet handed back.
    in_engine: bool,
    /// A replace's gain, recorded into the settings replay at its commit.
    swap_gain: Option<f32>,
    /// Withdraws sent whose `NotHeld` has not come back: that answer is dropped, never mailed.
    withdraws: u32,
    /// Its replace committed after it gave up: the remove the ring had no room for, retried each pump.
    remove_owed: bool,
}

pub(crate) struct Desk {
    slot: usize,
    pub(crate) port: Option<SlotPort>,
    owners: Vec<OwnerEntry>,
    mail: VecDeque<SlotReply>,
}

fn tag_of(reply: &SlotReply) -> u64 {
    match reply {
        SlotReply::Handback(back) => back.tag,
        SlotReply::Committed { tag } | SlotReply::NotHeld { tag } => *tag,
    }
}

impl Desk {
    pub(crate) fn new(slot: usize) -> Desk {
        Desk { slot, port: None, owners: Vec::new(), mail: VecDeque::new() }
    }

    /// Route every reply the port holds to its owner (the header's rules): how many it routed.
    pub(crate) fn pump(&mut self, settings: &Mutex<Settings>) -> usize {
        let mut routed = 0;
        while let Some(reply) = self.port.as_mut().and_then(SlotPort::reply) {
            self.route(reply, settings);
            routed += 1;
        }
        self.send_owed_removes();
        routed
    }

    /// The engine this desk's units were in was leaked (a unit panicked on its way out): those still in
    /// it never come back, so their owners leak their plugins (`SlotHost::remove` fails) and the slot frees.
    pub(crate) fn lose(&mut self) {
        for entry in self.owners.iter_mut().filter(|e| e.in_engine) {
            entry.in_engine = false;
            entry.standing = Standing::Abandoned;
        }
    }

    /// An Active owner's unit is in the engine.
    pub(crate) fn holds_active(&self) -> bool {
        self.owners.iter().any(|e| e.in_engine && e.standing == Standing::Active)
    }

    /// An install would find the slot taken: a unit is in the engine, or an Active owner's eviction
    /// waits to go in again.
    pub(super) fn occupied(&self) -> bool {
        self.owners.iter().any(|e| e.in_engine)
            || self.mail.iter().any(|r| matches!(r, SlotReply::Handback(b) if b.reason == HandbackReason::Evicted && self.standing(b.tag) == Some(Standing::Active)))
    }

    pub(super) fn standing(&self, tag: u64) -> Option<Standing> {
        self.entry(tag).map(|e| e.standing)
    }

    pub(super) fn in_engine(&self, tag: u64) -> bool {
        self.entry(tag).is_some_and(|e| e.in_engine)
    }

    /// `tag` sent a unit into the engine (an install, or a replace at `swap_gain`): Active from here.
    pub(super) fn enter(&mut self, tag: u64, swap_gain: Option<f32>) {
        match self.entry_mut(tag) {
            Some(entry) => {
                entry.in_engine = true;
                entry.swap_gain = swap_gain;
            }
            None => self.owners.push(OwnerEntry { tag, standing: Standing::Active, in_engine: true, swap_gain, withdraws: 0, remove_owed: false }),
        }
    }

    /// An install activated at a rate the engine no longer runs: the unit waits in `tag`'s mail as an
    /// eviction, to be re-activated, as if it had gone in and been evicted.
    pub(super) fn park(&mut self, tag: u64, unit: Box<dyn SlotProcessor>) {
        self.enter(tag, None);
        self.entry_mut(tag).expect("just entered").in_engine = false;
        self.mail.push_back(SlotReply::Handback(Handback { tag, reason: HandbackReason::Evicted, unit }));
    }

    /// `tag` sent a withdraw.
    pub(super) fn withdrew(&mut self, tag: u64) {
        if let Some(entry) = self.entry_mut(tag) {
            entry.withdraws += 1;
        }
    }

    /// The first unit in `tag`'s mail whose reason `pick` maps to `Some`, with that value. Taking a unit
    /// whose owner has none left in the engine releases the owner (`release`).
    pub(super) fn take_unit<T>(&mut self, tag: u64, pick: impl Fn(HandbackReason) -> Option<T>) -> Option<(T, Box<dyn SlotProcessor>)> {
        let (at, value) = self.mail.iter().enumerate().find_map(|(at, r)| match r {
            SlotReply::Handback(back) if back.tag == tag => pick(back.reason).map(|value| (at, value)),
            _ => None,
        })?;
        let Some(SlotReply::Handback(back)) = self.mail.remove(at) else { unreachable!("found a handback at {at}") };
        if !self.in_engine(tag) {
            self.release(tag);
        }
        Some((value, back.unit))
    }

    /// Take `tag`'s `Committed` from its mail.
    pub(super) fn take_committed(&mut self, tag: u64) -> bool {
        self.take_note(|r| matches!(r, SlotReply::Committed { tag: t } if *t == tag))
    }

    /// Take a `NotHeld` for `tag` from its mail.
    pub(super) fn take_not_held(&mut self, tag: u64) -> bool {
        self.take_note(|r| matches!(r, SlotReply::NotHeld { tag: t } if *t == tag))
    }

    /// `tag` gives up on its unit: what waits in its mail leaks, an owner with nothing in the engine is
    /// released, and one whose replace already committed has its unit removed. Whatever comes back of it
    /// later leaks in `pump`.
    pub(super) fn abandon(&mut self, tag: u64) {
        let slot = self.slot;
        let Some(entry) = self.entry_mut(tag) else { return };
        entry.standing = Standing::Abandoned;
        let (theirs, rest): (VecDeque<_>, VecDeque<_>) = std::mem::take(&mut self.mail).into_iter().partition(|r| tag_of(r) == tag);
        self.mail = rest;
        let mut committed = false;
        for reply in theirs {
            match reply {
                SlotReply::Committed { .. } => committed = true,
                SlotReply::Handback(back) => {
                    log::warn!("[engine_io] slot {slot}: an abandoned unit was waiting ({:?}); leaking it", back.reason);
                    std::mem::forget(back.unit);
                }
                SlotReply::NotHeld { .. } => {}
            }
        }
        let entry = self.entry_mut(tag).expect("looked up above");
        entry.remove_owed |= committed && entry.in_engine;
        if !entry.in_engine {
            self.release(tag);
        }
        self.send_owed_removes();
    }

    fn route(&mut self, reply: SlotReply, settings: &Mutex<Settings>) {
        let slot = self.slot;
        match reply {
            SlotReply::Committed { tag } => {
                for entry in self.owners.iter_mut().filter(|e| e.tag != tag && e.standing == Standing::Active) {
                    entry.standing = Standing::Retiring;
                }
                let Some(entry) = self.entry_mut(tag) else {
                    log::error!("[engine_io] slot {slot}: a replace committed for an owner the desk does not know");
                    return;
                };
                if let Some(gain) = entry.swap_gain.take() {
                    // Lock order: a slot's desk, then `settings`, as `swap_engine` takes them; nothing takes a
                    // desk while it holds `settings`. A `SetSlotGain` sent between the commit and this pump is
                    // overwritten here: at most one answer poll while the owner waits, and the UI sends no gain
                    // for a slot while its swap is pending.
                    let mut settings = settings.lock().unwrap_or_else(|e| e.into_inner());
                    settings.record(&Command::SetSlotGain(slot as u8, gain));
                }
                if entry.standing == Standing::Abandoned {
                    entry.remove_owed = true;
                } else {
                    self.mail.push_back(SlotReply::Committed { tag });
                }
            }
            SlotReply::NotHeld { tag } => {
                let Some(entry) = self.entry_mut(tag) else { return };
                if entry.withdraws > 0 {
                    entry.withdraws -= 1;
                } else if entry.standing != Standing::Abandoned {
                    self.mail.push_back(SlotReply::NotHeld { tag });
                }
            }
            SlotReply::Handback(back) => match self.owners.iter().position(|e| e.tag == back.tag) {
                Some(at) if self.owners[at].standing != Standing::Abandoned => {
                    self.owners[at].in_engine = false;
                    self.mail.push_back(SlotReply::Handback(back));
                }
                at => {
                    if let Some(at) = at {
                        self.owners.swap_remove(at);
                    }
                    log::warn!("[engine_io] slot {slot}: a unit came back ({:?}) to no owner; leaking it", back.reason);
                    std::mem::forget(back.unit);
                }
            },
        }
    }

    fn send_owed_removes(&mut self) {
        let Some(port) = self.port.as_mut() else { return };
        for entry in self.owners.iter_mut().filter(|e| e.remove_owed) {
            if port.remove(entry.tag) {
                entry.remove_owed = false;
            } else {
                log::error!("[engine_io] slot {}: the port is full; an abandoned unit's remove waits", self.slot);
            }
        }
    }

    fn take_note(&mut self, pick: impl Fn(&SlotReply) -> bool) -> bool {
        let at = self.mail.iter().position(|r| !matches!(r, SlotReply::Handback(_)) && pick(r));
        at.and_then(|at| self.mail.remove(at)).is_some()
    }

    /// `tag` has nothing left in the engine or its mail: drop its unread notes, and its entry unless it
    /// is Retiring (kept until the owner drops: `forget`).
    fn release(&mut self, tag: u64) {
        self.owners.retain(|e| e.tag != tag || e.standing == Standing::Retiring);
        self.mail.retain(|r| tag_of(r) != tag || matches!(r, SlotReply::Handback(_)));
    }

    /// `tag`'s owner dropped: its entry goes once nothing of it is in the engine or its mail; otherwise
    /// it stays, and whatever comes back of it leaks as it would have.
    pub(super) fn forget(&mut self, tag: u64) {
        if self.holds_nothing(tag) {
            self.owners.retain(|e| e.tag != tag);
        }
    }

    /// Nothing of `tag`'s is in the engine or waits in its mail.
    pub(super) fn holds_nothing(&self, tag: u64) -> bool {
        !self.in_engine(tag) && !self.mail.iter().any(|r| tag_of(r) == tag)
    }

    fn entry(&self, tag: u64) -> Option<&OwnerEntry> {
        self.owners.iter().find(|e| e.tag == tag)
    }

    fn entry_mut(&mut self, tag: u64) -> Option<&mut OwnerEntry> {
        self.owners.iter_mut().find(|e| e.tag == tag)
    }
}

impl Drop for Desk {
    /// A unit no owner took, from the mail or the port, leaks here (the header).
    fn drop(&mut self) {
        let unread = std::iter::from_fn(|| self.port.as_mut()?.reply());
        let mut leaked = 0;
        for reply in self.mail.drain(..).chain(unread) {
            if let SlotReply::Handback(back) = reply {
                std::mem::forget(back.unit);
                leaked += 1;
            }
        }
        if leaked > 0 {
            log::warn!("[engine_io] slot {}: {leaked} unit(s) never reached their owners; leaking them", self.slot);
        }
    }
}
