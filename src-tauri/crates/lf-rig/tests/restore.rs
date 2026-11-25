//! The restore machine (D1, D5, D6, D9, D10) driven by events, judged by the actions it answers. A
//! small driver plays the app's glue: it answers each blocking action as a fake host would (a device
//! runs, the engine takes the seed, the scan finds the record's plugins, every write commits, every
//! add seats), and each test scripts what differs: an add's answer, a refused seed, a failed or
//! unanswered write, a take held back, a view.

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use lf_rig::restore::{SEED_BACKOFF_MAX, SEED_LOG_AFTER};
use lf_rig::{
    project, Action, AddFail, AddOk, AddState, Disposition, Event, Fate, InstallToken, MarkerRead, Place, PlateState, PlateView, PluginRecord,
    PluginRefView, Restore, Retry, RigRecord, RigView, SlotRecord, UnitBody, UnitId, UnitKind, UnitRecord, Version1,
};

// ── units and records ────────────────────────────────────────────────────────────────────────────

fn fx(id: UnitId, kind: UnitKind) -> UnitRecord {
    UnitRecord { id, body: UnitBody::Builtin { kind, params: None }, bypass: None }
}

fn bypassed(mut unit: UnitRecord) -> UnitRecord {
    unit.bypass = Some(true);
    unit
}

fn plugin_ref(id: UnitId) -> PluginRefView {
    PluginRefView { format: "vst3".into(), path: format!("C:\\Plugins\\p{id}.vst3"), id: format!("CLASS{id}") }
}

fn plug(id: UnitId) -> UnitRecord {
    UnitRecord { id, body: UnitBody::Plugin(PluginRecord { plugin: plugin_ref(id), name: Some(format!("Plug {id}")) }), bypass: None }
}

fn slot(units: Vec<UnitRecord>, pending: Vec<UnitRecord>) -> SlotRecord {
    SlotRecord { volume: 0.8, units, pending }
}

fn record(a: SlotRecord, b: SlotRecord) -> RigRecord {
    RigRecord { version: Version1, slots: [a, b] }
}

fn plate(unit: &UnitRecord, token: InstallToken, state: PlateState, applied: bool, source: bool) -> PlateView {
    let (kind, plugin) = match &unit.body {
        UnitBody::Plugin(p) => (UnitKind::Plugin, Some(p.plugin.clone())),
        UnitBody::Builtin { kind, .. } => (*kind, None),
    };
    PlateView {
        id: unit.id,
        added_by: token,
        kind,
        plugin,
        name: String::new(),
        params: None,
        bypass: false,
        source,
        state,
        applied,
        failed_add: state == PlateState::Failed,
    }
}

// ── the driver ───────────────────────────────────────────────────────────────────────────────────

/// How the fake host answers an add.
#[derive(Clone, Copy, Debug)]
enum Ans {
    /// Applied; `source` as the plugin's activation said.
    Seated(bool),
    Waiting,
    Host(Retry),
    /// The host refused after the mint (its failed plate carries the token).
    HostAfterMint,
    Seat,
    Plugin,
    SourceTaken,
    Unresolved { exited: bool },
    /// Unresolved, and its plate came and went before the next view: no view shows it.
    UnresolvedUnseen { exited: bool },
    /// The host found the unit already in the slot under its lock (a plate that appeared after the
    /// machine's last view) and added nothing.
    Matched,
    /// No answer yet (a plugin in setup, a built-in that hangs).
    Hang,
}

struct Rig {
    m: Restore,
    trace: Vec<Action>,
    /// Seeds the ring refuses before it takes one.
    refusals: usize,
    device: bool,
    /// Whether the engine takes the seed when asked, and its generation.
    takes: bool,
    gen: u64,
    /// `None`: the scan finds every plugin it is asked for.
    found: Option<Vec<PluginRefView>>,
    plugins: Vec<PluginRefView>,
    answers: BTreeMap<UnitId, VecDeque<Ans>>,
    /// Which durable writes fail, by their 1-based count.
    fail_writes: Vec<usize>,
    /// Writes from this 1-based count on get no answer (still out when the test goes on).
    hold_writes_from: usize,
    writes: usize,
    /// The crash marker on disk, as this fake keeps it.
    disk: Option<Vec<UnitId>>,
    view: RigView,
    next_token: InstallToken,
    tokens: BTreeMap<UnitId, InstallToken>,
    units: BTreeMap<UnitId, UnitRecord>,
}

impl Rig {
    fn new() -> Self {
        Rig {
            m: Restore::new(),
            trace: Vec::new(),
            refusals: 0,
            device: true,
            takes: true,
            gen: 1,
            found: None,
            plugins: Vec::new(),
            answers: BTreeMap::new(),
            fail_writes: Vec::new(),
            hold_writes_from: usize::MAX,
            writes: 0,
            disk: None,
            view: RigView::default(),
            next_token: 100,
            tokens: BTreeMap::new(),
            units: BTreeMap::new(),
        }
    }

    fn answer(mut self, id: UnitId, answers: &[Ans]) -> Self {
        self.answers.insert(id, answers.iter().copied().collect());
        self
    }

    fn open(&mut self, rec: RigRecord, marker: MarkerRead) {
        for s in &rec.slots {
            for u in s.units.iter().chain(&s.pending) {
                self.units.insert(u.id, u.clone());
                if let UnitBody::Plugin(p) = &u.body {
                    self.plugins.push(p.plugin.clone());
                }
            }
        }
        if let MarkerRead::Names(ids) = &marker {
            self.disk = Some(ids.clone());
        }
        self.feed(Event::Opened { record: rec, marker });
    }

    /// Feed `event` and every answer the fake host gives, in order; the actions it caused.
    fn feed(&mut self, event: Event) -> Vec<Action> {
        let start = self.trace.len();
        let mut queue = VecDeque::from([event]);
        while let Some(event) = queue.pop_front() {
            for action in self.m.handle(event) {
                self.trace.push(action.clone());
                queue.extend(self.reply(&action));
            }
        }
        self.trace[start..].to_vec()
    }

    fn reply(&mut self, action: &Action) -> Vec<Event> {
        match action {
            Action::Seed { .. } if self.refusals > 0 => {
                self.refusals -= 1;
                vec![Event::Refused]
            }
            Action::Seed { .. } => vec![Event::Seeded],
            Action::WaitDevice if self.device => vec![Event::DeviceRuns],
            Action::WaitTaken if self.takes => vec![Event::Taken { gen: self.gen }],
            Action::Scan(paths) => {
                let found = self.found.clone().unwrap_or_else(|| self.plugins.iter().filter(|p| paths.contains(&p.path)).cloned().collect());
                vec![Event::Scanned(found)]
            }
            Action::Commit | Action::CommitMarker(_) | Action::RemoveMarker => {
                self.writes += 1;
                if self.writes >= self.hold_writes_from {
                    return Vec::new();
                }
                if self.fail_writes.contains(&self.writes) {
                    return vec![Event::CommitFailed];
                }
                match action {
                    Action::CommitMarker(ids) => self.disk = Some(ids.clone()),
                    Action::RemoveMarker => self.disk = None,
                    _ => {}
                }
                vec![Event::Committed]
            }
            Action::Add { slot, id, spec, .. } => self.add(*slot, *id, spec),
            _ => Vec::new(),
        }
    }

    fn add(&mut self, slot: usize, id: UnitId, spec: &UnitRecord) -> Vec<Event> {
        let source_by_kind = matches!(spec.body, UnitBody::Builtin { kind: UnitKind::Synth, .. });
        let ans = self.answers.get_mut(&id).and_then(VecDeque::pop_front).unwrap_or(Ans::Seated(source_by_kind));
        let token = self.next_token;
        self.next_token += 1;
        self.tokens.insert(id, token);
        let put = |rig: &mut Rig, state, applied, source| {
            rig.view.slots[slot].retain(|p| !(p.id == id && p.failed_add));
            rig.view.slots[slot].push(plate(spec, token, state, applied, source));
        };
        let answer = match ans {
            Ans::Seated(source) => {
                put(self, PlateState::Seated, true, source);
                Ok(AddOk { state: AddState::Seated, token })
            }
            Ans::Waiting => {
                put(self, PlateState::Waiting, false, false);
                Ok(AddOk { state: AddState::Waiting, token })
            }
            Ans::Host(retry) => {
                if retry == Retry::Device {
                    self.device = false;
                }
                Err(AddFail::Host { why: "no engine".into(), retry, token: None })
            }
            Ans::HostAfterMint => {
                put(self, PlateState::Failed, false, false);
                Err(AddFail::Host { why: "owner spawn".into(), retry: Retry::Launch, token: Some(token) })
            }
            Ans::Seat => {
                put(self, PlateState::Failed, false, false);
                Err(AddFail::Seat { why: "port full".into(), token, exited: true })
            }
            Ans::Plugin => {
                put(self, PlateState::Failed, false, false);
                Err(AddFail::Plugin { why: "activation failed".into(), token, exited: true })
            }
            Ans::SourceTaken => {
                put(self, PlateState::Failed, false, false);
                Err(AddFail::SourceTaken { why: "source taken".into(), token, exited: true })
            }
            Ans::Unresolved { exited } => {
                put(self, PlateState::Loading, false, false);
                Err(AddFail::Unresolved { why: "setup timed out".into(), token, exited })
            }
            Ans::UnresolvedUnseen { exited } => Err(AddFail::Unresolved { why: "setup timed out".into(), token, exited }),
            Ans::Matched => {
                put(self, PlateState::Seated, true, false);
                Ok(AddOk { state: AddState::Matched, token })
            }
            Ans::Hang => {
                put(self, PlateState::Loading, false, false);
                return Vec::new();
            }
        };
        vec![Event::AddAnswered(answer), self.view_event()]
    }

    fn view_event(&mut self) -> Event {
        self.view.rev += 1;
        Event::ViewChanged(self.view.clone())
    }

    /// Set plate `id`'s state (and `applied`, `source`) in the model and feed the view.
    fn set_plate(&mut self, id: UnitId, state: PlateState, applied: bool, source: bool) -> Vec<Action> {
        for p in self.view.slots.iter_mut().flatten().filter(|p| p.id == id) {
            p.state = state;
            p.applied = applied;
            p.source = source;
            p.failed_add = state == PlateState::Failed;
        }
        let event = self.view_event();
        self.feed(event)
    }

    /// The player removed unit `id`.
    fn remove_plate(&mut self, id: UnitId) -> Vec<Action> {
        for plates in &mut self.view.slots {
            plates.retain(|p| p.id != id);
        }
        let event = self.view_event();
        self.feed(event)
    }

    fn token(&self, id: UnitId) -> InstallToken {
        self.tokens[&id]
    }

    fn disposition(&self, id: UnitId) -> Option<&Disposition> {
        self.m.plan().dispositions.get(&id)
    }

    fn adds(&self) -> Vec<UnitId> {
        self.trace.iter().filter_map(|a| if let Action::Add { id, .. } = a { Some(*id) } else { None }).collect()
    }

    fn add_of(&self, id: UnitId) -> (usize, Place) {
        self.trace
            .iter()
            .find_map(|a| match a {
                Action::Add { slot, id: i, place, .. } if *i == id => Some((*slot, place.clone())),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no add of {id}: {:#?}", self.trace))
    }

    fn pos(&self, test: impl Fn(&Action) -> bool) -> Option<usize> {
        self.trace.iter().position(test)
    }

    fn pos_add(&self, id: UnitId) -> usize {
        self.pos(|a| matches!(a, Action::Add { id: i, .. } if *i == id)).unwrap_or_else(|| panic!("no add of {id}: {:#?}", self.trace))
    }

    fn done(&self) -> bool {
        self.trace.contains(&Action::Done)
    }

    fn logs(&self) -> usize {
        self.trace.iter().filter(|a| matches!(a, Action::Log(_))).count()
    }

    fn held(&self, id: UnitId) -> bool {
        matches!(self.disposition(id), Some(Disposition::Held { .. }))
    }
}

fn is_marker(ids: &[UnitId]) -> impl Fn(&Action) -> bool + '_ {
    move |a| matches!(a, Action::CommitMarker(i) if i == ids)
}

// ── the plan's tests ─────────────────────────────────────────────────────────────────────────────

#[test]
fn c1_marker_and_record_durable_before_any_add_levels_taken_before_adds() {
    let mut rig = Rig::new();
    rig.device = false;
    rig.open(record(slot(vec![fx(1, UnitKind::Ott), plug(2)], vec![]), slot(vec![], vec![fx(3, UnitKind::Ring)])), MarkerRead::None);
    // The levels and the send-off go in at once, before anything else.
    assert_eq!(rig.trace[0], Action::Seed { volumes: [0.8, 0.8], after: Duration::ZERO });
    // No device: nothing is added (slot B's placement may commit, its adds wait).
    assert!(rig.adds().is_empty(), "{:#?}", rig.trace);
    assert_eq!(rig.trace.last(), Some(&Action::WaitDevice));

    rig.device = true;
    rig.takes = false;
    rig.feed(Event::DeviceRuns);
    // The engine has not taken the seed yet: no add, whatever else is ready.
    assert!(rig.adds().is_empty(), "{:#?}", rig.trace);
    assert_eq!(rig.trace.last(), Some(&Action::WaitTaken));
    rig.takes = true;
    rig.feed(Event::Taken { gen: 1 });
    let taken = rig.pos(|a| *a == Action::WaitTaken).expect("the barrier");
    let first_marker = rig.pos(is_marker(&[])).expect("the marker naming nothing");
    let placement = rig.pos(|a| *a == Action::Commit).expect("slot B's placement");
    assert!(first_marker < placement, "the marker before the placement: {:#?}", rig.trace);
    assert!(taken < rig.pos_add(1) && first_marker < rig.pos_add(1));
    assert!(rig.pos(is_marker(&[2])).expect("the marker naming the plugin") < rig.pos_add(2));
    assert!(placement < rig.pos_add(3));
    assert_eq!(rig.adds(), [1, 2, 3]);
}

#[test]
fn c2_present_adds_missing_units_under_their_ids_in_order() {
    let mut rig = Rig::new();
    let a = vec![fx(5, UnitKind::Synth), fx(6, UnitKind::Echo)];
    let b = vec![plug(7), fx(8, UnitKind::Reverb)];
    rig.open(record(slot(a.clone(), vec![]), slot(b.clone(), vec![])), MarkerRead::None);
    assert_eq!(rig.adds(), [5, 6, 7, 8]);
    assert_eq!(rig.add_of(5), (0, Place::After(vec![])));
    assert_eq!(rig.add_of(6), (0, Place::After(vec![5])));
    assert_eq!(rig.add_of(7), (1, Place::After(vec![])));
    assert_eq!(rig.add_of(8), (1, Place::After(vec![7])));
    let spec = rig.trace.iter().find_map(|a| if let Action::Add { id: 7, spec, .. } = a { Some(spec.clone()) } else { None });
    assert_eq!(spec, Some(b[0].clone()), "the record's unit, under its id");
    for id in [5, 6, 7, 8] {
        assert_eq!(rig.disposition(id), Some(&Disposition::Restored { token: rig.token(id) }));
    }
    // The plugin settles, then the marker goes and the restore is done.
    assert!(!rig.done());
    rig.feed(Event::SettleElapsed { token: rig.token(7) });
    assert_eq!(rig.disk, None);
    assert!(rig.done());
}

#[test]
fn c3_c4_second_open_starts_nothing_and_settle_keeps_marker() {
    let mut rig = Rig::new();
    let rec = record(slot(vec![plug(7)], vec![]), slot(vec![], vec![]));
    rig.open(rec.clone(), MarkerRead::None);
    let token = rig.token(7);
    assert!(rig.trace.contains(&Action::Settle { token }));
    // A reload during the settle: nothing starts, and the marker still names the unit.
    assert!(rig.feed(Event::Opened { record: rec, marker: MarkerRead::Names(vec![7]) }).is_empty());
    assert_eq!(rig.disk, Some(vec![7]));
    assert!(!rig.trace.contains(&Action::RemoveMarker));
    let after = rig.feed(Event::SettleElapsed { token });
    assert_eq!(after, [Action::RemoveMarker, Action::Done]);
    assert_eq!(rig.adds(), [7], "nothing added twice");
}

#[test]
fn c5_leftover_marker_forgets_only_named_units_notice_until_seen() {
    let mut rig = Rig::new();
    let rec = record(slot(vec![plug(1), plug(2)], vec![]), slot(vec![plug(3)], vec![]));
    rig.open(rec.clone(), MarkerRead::Names(vec![2, 99]));
    let commit = rig.pos(|a| *a == Action::Commit).expect("the record written first");
    let forget = rig.pos(|a| *a == Action::Forget { id: 2 }).expect("its tone files go");
    let removed = rig.pos(|a| *a == Action::RemoveMarker).expect("the marker goes");
    assert!(commit < forget && forget < removed && removed < rig.pos_add(1));
    assert_eq!(rig.disposition(2), Some(&Disposition::Forgotten));
    assert_eq!(rig.adds(), [1, 3], "only the named unit is forgotten");
    assert!(!rig.trace.contains(&Action::Forget { id: 1 }) && !rig.trace.contains(&Action::Forget { id: 3 }));

    let notice = rig.m.notice().cloned().expect("a notice");
    assert_eq!(notice.forgotten, ["Plug 2"]);
    assert!(!notice.plugins_held);
    // A reload before the page confirmed it: answered again, and nothing starts.
    assert!(rig.feed(Event::Opened { record: rec, marker: MarkerRead::None }).is_empty());
    assert_eq!(rig.m.notice(), Some(&notice));
    rig.feed(Event::Seen(notice.id + 1));
    assert_eq!(rig.m.notice(), Some(&notice), "another notice's id clears nothing");
    rig.feed(Event::Seen(notice.id));
    assert_eq!(rig.m.notice(), None);
}

#[test]
fn c6_missing_from_scan_held_with_pending() {
    let mut rig = Rig::new();
    rig.found = Some(vec![]);
    rig.open(record(slot(vec![plug(1), fx(2, UnitKind::Echo)], vec![fx(3, UnitKind::Ring)]), slot(vec![], vec![])), MarkerRead::None);
    assert!(rig.trace.contains(&Action::Scan(vec![plugin_ref(1).path])));
    assert!(rig.held(1));
    assert_eq!(rig.adds(), [2], "the units after it still go in");
    assert_eq!(rig.add_of(2).1, Place::After(vec![1]));
    assert!(!rig.trace.contains(&Action::Commit), "no placement under a held head");
    assert_eq!(rig.m.plan().pending[0], [fx(3, UnitKind::Ring)]);
    assert!(rig.done());
}

#[test]
fn c7_head_role_from_applied_source_not_scan() {
    let mut rig = Rig::new().answer(1, &[Ans::Waiting]);
    rig.open(record(slot(vec![plug(1)], vec![fx(2, UnitKind::Ott), fx(3, UnitKind::Ring)]), slot(vec![], vec![])), MarkerRead::None);
    assert!(!rig.trace.contains(&Action::Commit), "no role before the head applies");
    // A plate not yet applied says nothing, whatever its `source` reads.
    rig.set_plate(1, PlateState::Waiting, false, true);
    assert!(!rig.trace.contains(&Action::Commit));
    rig.set_plate(1, PlateState::Seated, true, true);
    assert!(rig.trace.contains(&Action::Commit));
    assert_eq!(rig.adds(), [1, 2], "a source head takes the OTT alone");
    assert_eq!(rig.add_of(2).1, Place::End);
    assert_eq!(rig.m.plan().units[0].iter().map(|u| u.id).collect::<Vec<_>>(), [1, 2]);
    assert!(rig.m.plan().pending[0].is_empty());

    // A head plate of the same unit still loading is no match (CP3a r1): the head is held, so `pending`
    // waits; once it applies the player's unit wins the id, and its `source` decides the role.
    let mut rig = Rig::new();
    let p1 = plug(1);
    rig.view.slots[0] = vec![plate(&p1, 500, PlateState::Loading, false, true)];
    rig.view.rev = 1;
    let event = Event::ViewChanged(rig.view.clone());
    rig.feed(event);
    rig.open(record(slot(vec![p1], vec![fx(2, UnitKind::Ott), fx(3, UnitKind::Ring)]), slot(vec![], vec![])), MarkerRead::None);
    assert!(rig.held(1));
    assert!(!rig.trace.contains(&Action::Commit), "no role before the head applies");
    rig.set_plate(1, PlateState::Seated, true, false);
    assert_eq!(rig.disposition(1), None, "the player's unit won the id");
    assert_eq!(rig.adds(), [2, 3], "an effect: every send");
}

#[test]
fn c8_ott_on_placement_order() {
    let mut rig = Rig::new();
    let sends = vec![fx(4, UnitKind::Ott), fx(5, UnitKind::Ring), fx(6, UnitKind::Echo), fx(7, UnitKind::Reverb)];
    rig.open(
        record(slot(vec![fx(1, UnitKind::Synth)], vec![bypassed(fx(2, UnitKind::Ott)), fx(3, UnitKind::Ring)]), slot(vec![], sends)),
        MarkerRead::None,
    );
    // Slot A: a SYNTH head takes only an OTT that is on, and this one is off: nothing placed, pending cleared.
    // Slot B: input-fed, every send in its order, at the chain's end.
    assert_eq!(rig.adds(), [1, 4, 5, 6, 7]);
    for id in [4, 5, 6, 7] {
        assert_eq!(rig.add_of(id), (1, Place::End));
    }
    assert!(rig.m.plan().pending.iter().all(Vec::is_empty));
    assert!(rig.disposition(2).is_none() && rig.disposition(3).is_none());

    // A SYNTH head with its OTT on: the OTT alone goes in, at the chain's end.
    let mut rig = Rig::new();
    rig.open(record(slot(vec![fx(1, UnitKind::Synth)], vec![fx(2, UnitKind::Ott), fx(3, UnitKind::Ring)]), slot(vec![], vec![])), MarkerRead::None);
    assert_eq!(rig.adds(), [1, 2]);
    assert_eq!(rig.add_of(2), (0, Place::End));
    assert_eq!(rig.m.plan().units[0].iter().map(|u| u.id).collect::<Vec<_>>(), [1, 2]);
    assert!(rig.disposition(3).is_none() && rig.m.plan().pending[0].is_empty());
}

#[test]
fn c9_off_slot_places_with_no_head() {
    let mut rig = Rig::new();
    rig.open(record(slot(vec![], vec![fx(1, UnitKind::Ring), fx(2, UnitKind::Echo)]), slot(vec![], vec![])), MarkerRead::None);
    assert_eq!(rig.adds(), [1, 2]);
    assert_eq!(rig.add_of(1), (0, Place::End));
    assert!(rig.done());
}

#[test]
fn c10_placement_committed_before_its_adds() {
    let rec = record(slot(vec![], vec![fx(1, UnitKind::Ring)]), slot(vec![], vec![]));
    let mut rig = Rig::new();
    rig.open(rec.clone(), MarkerRead::None);
    assert!(rig.pos(|a| *a == Action::Commit).expect("the placement") < rig.pos_add(1));

    // The placement's write fails (write 2: the first is the marker): nothing added, pending kept.
    let mut rig = Rig::new();
    rig.fail_writes = vec![2];
    rig.open(rec, MarkerRead::None);
    assert!(rig.trace.contains(&Action::Commit));
    assert!(rig.adds().is_empty());
    assert_eq!(rig.m.plan().pending[0], [fx(1, UnitKind::Ring)]);
    assert!(rig.m.plan().units[0].is_empty() && rig.disposition(1).is_none());
    assert!(rig.done());
}

#[test]
fn c12_plugin_failure_forgotten_next_unit_heads() {
    let mut rig = Rig::new().answer(1, &[Ans::Plugin]);
    rig.open(record(slot(vec![plug(1), fx(2, UnitKind::Synth)], vec![fx(3, UnitKind::Ott), fx(4, UnitKind::Ring)]), slot(vec![], vec![])), MarkerRead::None);
    assert_eq!(rig.disposition(1), Some(&Disposition::Forgotten));
    assert!(rig.trace.contains(&Action::Forget { id: 1 }));
    assert_eq!(rig.adds(), [1, 2, 3], "the SYNTH heads now: the OTT alone");
    assert_eq!(rig.add_of(2).1, Place::After(vec![]), "a forgotten unit is no anchor");
    assert_eq!(rig.disk, None);
    assert!(rig.done());
}

#[test]
fn c12_host_failure_held_device_retry_same_id() {
    let mut rig = Rig::new().answer(1, &[Ans::Host(Retry::Device)]);
    rig.open(record(slot(vec![fx(1, UnitKind::Echo), fx(2, UnitKind::Reverb)], vec![fx(3, UnitKind::Ring)]), slot(vec![], vec![])), MarkerRead::None);
    assert!(rig.held(1));
    assert_eq!(rig.adds(), [1], "no add while no device runs");
    assert_eq!(rig.trace.last(), Some(&Action::WaitDevice));
    rig.device = true;
    let after = rig.feed(Event::DeviceRuns);
    assert_eq!(after[0], Action::WaitTaken, "the new engine takes the seed first");
    assert_eq!(rig.adds(), [1, 1, 2, 3], "the same id again, then the rest");
    assert_eq!(rig.disposition(1), Some(&Disposition::Restored { token: rig.token(1) }));
    assert!(rig.done());
}

#[test]
fn c13_matched_units_skipped() {
    let mut rig = Rig::new();
    let (p1, ring) = (plug(1), fx(3, UnitKind::Ring));
    rig.view.slots[0] = vec![plate(&p1, 500, PlateState::Seated, true, false), plate(&ring, 501, PlateState::Seated, true, false)];
    rig.view.rev = 1;
    let event = Event::ViewChanged(rig.view.clone());
    rig.feed(event);
    rig.open(record(slot(vec![p1, fx(2, UnitKind::Echo)], vec![]), slot(vec![ring], vec![])), MarkerRead::None);
    assert_eq!(rig.disposition(1), Some(&Disposition::Matched { token: 500 }));
    assert!(!rig.trace.iter().any(|a| matches!(a, Action::Scan(_))), "a matched plugin is not scanned");
    assert_eq!(rig.disposition(3), None, "an applied plate in the other slot holds its id: the player's unit wins it");
    assert!(rig.m.plan().units[1].is_empty());
    assert_eq!(rig.adds(), [2]);
    assert_eq!(rig.add_of(2).1, Place::After(vec![1]));
}

#[test]
fn c18_c27_refused_or_partial_seed_retries_no_add_meanwhile_one_log_at_10s() {
    let mut rig = Rig::new();
    rig.refusals = 40;
    rig.takes = false;
    rig.open(record(slot(vec![fx(1, UnitKind::Echo)], vec![]), slot(vec![], vec![])), MarkerRead::None);
    // Seeded at last, but not yet taken by the engine: still no add.
    assert!(rig.adds().is_empty(), "{:#?}", rig.trace);
    assert_eq!(rig.trace.last(), Some(&Action::WaitTaken));
    rig.takes = true;
    rig.feed(Event::Taken { gen: 1 });
    let seeds: Vec<(usize, Duration)> =
        rig.trace.iter().enumerate().filter_map(|(i, a)| if let Action::Seed { after, .. } = a { Some((i, *after)) } else { None }).collect();
    assert_eq!(seeds.len(), 41);
    let afters: Vec<Duration> = seeds.iter().map(|(_, d)| *d).collect();
    assert_eq!(afters[..4], [Duration::ZERO, Duration::from_millis(20), Duration::from_millis(40), Duration::from_millis(80)]);
    assert!(afters.iter().all(|d| *d <= SEED_BACKOFF_MAX) && afters[40] == SEED_BACKOFF_MAX);
    assert_eq!(rig.logs(), 1);
    let log = rig.pos(|a| matches!(a, Action::Log(_))).expect("one log");
    let waited: Duration = seeds.iter().filter(|(i, _)| *i < log).map(|(_, d)| *d).sum();
    assert!(waited >= SEED_LOG_AFTER && waited - SEED_BACKOFF_MAX < SEED_LOG_AFTER, "logged at 10 s: {waited:?}");
    assert!(seeds[40].0 < rig.pos_add(1), "no add before the seed went in");
}

#[test]
fn c21_confirmed_pre_load_refusal_later_units_go_on_after_a_present_anchor() {
    let mut rig = Rig::new().answer(2, &[Ans::Host(Retry::Launch)]);
    rig.open(record(slot(vec![fx(1, UnitKind::Echo), plug(2), fx(3, UnitKind::Reverb)], vec![]), slot(vec![], vec![])), MarkerRead::None);
    assert!(rig.held(2));
    assert_eq!(rig.adds(), [1, 2, 3]);
    assert_eq!(rig.add_of(3).1, Place::After(vec![1, 2]), "the app anchors after the nearest present: 1");
    assert_eq!(rig.disk, None, "no load ran: the marker goes");
    assert!(rig.done());
}

#[test]
fn c22_unresolved_plugin_and_builtin_timeout_hold_marker_and_place() {
    let mut rig = Rig::new().answer(1, &[Ans::Unresolved { exited: false }]).answer(3, &[Ans::Unresolved { exited: false }]);
    rig.open(record(slot(vec![plug(1), fx(2, UnitKind::Echo)], vec![]), slot(vec![fx(3, UnitKind::Ring)], vec![])), MarkerRead::None);
    assert_eq!(rig.adds(), [1, 2, 3], "the units after it go on");
    assert_eq!(rig.add_of(1), (0, Place::After(vec![])));
    assert_eq!(rig.add_of(2), (0, Place::After(vec![1])), "after the unresolved unit, which keeps its place");
    assert_eq!(rig.add_of(3), (1, Place::After(vec![])));
    assert_eq!(rig.disposition(1), Some(&Disposition::Issued { token: Some(rig.token(1)) }));
    assert_eq!(rig.disposition(3), Some(&Disposition::Issued { token: Some(rig.token(3)) }));
    assert_eq!(rig.disk, Some(vec![1]), "the marker keeps naming it");
    assert!(!rig.done());
    // Closing: a load not terminal keeps the marker.
    assert_eq!(rig.feed(Event::Closing), [Action::Done]);
    assert_eq!(rig.disk, Some(vec![1]));
}

#[test]
fn c23_builtin_hang_marker_names_no_plugin() {
    let mut rig = Rig::new().answer(1, &[Ans::Hang]);
    rig.open(record(slot(vec![fx(1, UnitKind::Echo)], vec![]), slot(vec![plug(2)], vec![])), MarkerRead::None);
    assert_eq!(rig.adds(), [1]);
    assert_eq!(rig.disk, Some(vec![]), "written, naming no plugin: a crash here forgets nothing");
    assert!(rig.pos(is_marker(&[])).expect("the first write") < rig.pos_add(1));
}

#[test]
fn c25_first_marker_write_fails_before_any_builtin_add() {
    let mut rig = Rig::new();
    rig.fail_writes = vec![1];
    rig.open(record(slot(vec![fx(1, UnitKind::Echo), plug(2)], vec![fx(3, UnitKind::Ring)]), slot(vec![], vec![])), MarkerRead::None);
    assert!(rig.trace.contains(&Action::CommitMarker(vec![])));
    assert!(rig.adds().is_empty());
    assert!(!rig.trace.contains(&Action::Commit), "nothing placed");
    assert!(rig.held(1) && rig.held(2));
    assert_eq!(rig.m.plan().pending[0].len(), 1);
    assert!(rig.done());
}

#[test]
fn c26_refusal_in_a_leaves_marker_before_b_loads() {
    let mut rig = Rig::new().answer(1, &[Ans::HostAfterMint]).answer(2, &[Ans::Hang]);
    rig.open(record(slot(vec![plug(1)], vec![]), slot(vec![plug(2)], vec![])), MarkerRead::None);
    assert!(rig.trace.contains(&Action::CommitMarker(vec![1])));
    assert_eq!(rig.disposition(1), Some(&Disposition::Held { why: "owner spawn".into(), token: Some(rig.token(1)) }));
    assert!(rig.pos(is_marker(&[2])).expect("b's write names b alone") < rig.pos_add(2));
    assert_eq!(rig.disk, Some(vec![2]), "a crash in b's load forgets b only");
}

#[test]
fn waiting_is_not_terminal() {
    let mut rig = Rig::new().answer(1, &[Ans::Waiting]);
    rig.open(record(slot(vec![plug(1)], vec![]), slot(vec![], vec![])), MarkerRead::None);
    let token = rig.token(1);
    assert!(!rig.trace.contains(&Action::Settle { token }));
    assert_eq!(rig.disposition(1), Some(&Disposition::Issued { token: Some(token) }));
    assert_eq!(rig.disk, Some(vec![1]));
    assert!(rig.set_plate(1, PlateState::Seated, true, false).contains(&Action::Settle { token }));
    assert_eq!(rig.disposition(1), Some(&Disposition::Restored { token }));
    assert_eq!(rig.disk, Some(vec![1]));
    assert_eq!(rig.feed(Event::SettleElapsed { token }), [Action::RemoveMarker, Action::Done]);
}

#[test]
fn setup_timeout_terminal_only_on_latch() {
    let mut rig = Rig::new().answer(1, &[Ans::Unresolved { exited: false }]);
    rig.open(record(slot(vec![plug(1)], vec![]), slot(vec![], vec![])), MarkerRead::None);
    let token = rig.token(1);
    assert!(rig.set_plate(1, PlateState::Failed, false, false).is_empty(), "failed, but its owner may be in plugin code");
    assert_eq!(rig.disposition(1), Some(&Disposition::Issued { token: Some(token) }));
    assert_eq!(rig.disk, Some(vec![1]));
    assert_eq!(rig.feed(Event::LatchSet { token }), [Action::RemoveMarker, Action::Done]);
    assert!(matches!(rig.disposition(1), Some(Disposition::Held { token: Some(t), .. }) if *t == token));
}

#[test]
fn terminal_load_leaves_marker_after_settle() {
    let mut rig = Rig::new();
    rig.open(record(slot(vec![plug(1), fx(2, UnitKind::Echo)], vec![]), slot(vec![plug(3)], vec![])), MarkerRead::None);
    let (t1, t3) = (rig.token(1), rig.token(3));
    assert_eq!(rig.disk, Some(vec![1, 3]), "both inside their settle");
    assert_eq!(rig.feed(Event::SettleElapsed { token: t1 }), [Action::CommitMarker(vec![3])]);
    assert_eq!(rig.feed(Event::SettleElapsed { token: t3 }), [Action::RemoveMarker, Action::Done]);
    assert_eq!(rig.disk, None);
}

#[test]
fn waiting_head_reevaluated_on_apply_and_on_removal() {
    // On apply: an effect head places every send.
    let mut rig = Rig::new().answer(1, &[Ans::Waiting]);
    rig.open(record(slot(vec![plug(1)], vec![fx(2, UnitKind::Ring)]), slot(vec![], vec![])), MarkerRead::None);
    assert!(!rig.trace.contains(&Action::Commit));
    rig.set_plate(1, PlateState::Seated, true, false);
    assert_eq!(rig.adds(), [1, 2]);

    // On removal: the next unit decides (a SYNTH: the OTT alone).
    let mut rig = Rig::new().answer(1, &[Ans::Waiting]);
    rig.open(record(slot(vec![plug(1), fx(2, UnitKind::Synth)], vec![fx(3, UnitKind::Ott), fx(4, UnitKind::Ring)]), slot(vec![], vec![])), MarkerRead::None);
    assert_eq!(rig.adds(), [1, 2]);
    assert!(!rig.trace.contains(&Action::Commit));
    rig.remove_plate(1);
    assert!(rig.disposition(1).is_none(), "a confirmed removal ends it");
    assert_eq!(rig.adds(), [1, 2, 3]);
}

#[test]
fn full_chain_keeps_unused_ids_queued() {
    // The chain is full: every placed unit is refused its seat in turn, and each is held under its
    // id, once, for the next launch.
    let mut rig = Rig::new().answer(2, &[Ans::Seat]).answer(3, &[Ans::Seat]).answer(4, &[Ans::Seat]);
    rig.open(
        record(slot(vec![fx(1, UnitKind::Echo)], vec![fx(2, UnitKind::Ott), fx(3, UnitKind::Ring), fx(4, UnitKind::Reverb)]), slot(vec![], vec![])),
        MarkerRead::None,
    );
    assert_eq!(rig.adds(), [1, 2, 3, 4]);
    let plan = rig.m.plan();
    assert_eq!(plan.units[0].iter().map(|u| u.id).collect::<Vec<_>>(), [1, 2, 3, 4], "each id once");
    assert!(plan.pending[0].is_empty());
    for id in [2, 3, 4] {
        assert_eq!(plan.dispositions[&id], Disposition::Held { why: "port full".into(), token: Some(rig.token(id)) });
    }
    assert!(rig.done());

    // A failure midway (an add that never answers, then close): the rest stay queued.
    let mut rig = Rig::new().answer(2, &[Ans::Seat]).answer(3, &[Ans::Hang]);
    rig.open(
        record(slot(vec![fx(1, UnitKind::Echo)], vec![fx(2, UnitKind::Ott), fx(3, UnitKind::Ring), fx(4, UnitKind::Reverb)]), slot(vec![], vec![])),
        MarkerRead::None,
    );
    rig.feed(Event::Closing);
    let plan = rig.m.plan();
    assert_eq!(plan.units[0].iter().map(|u| u.id).collect::<Vec<_>>(), [1, 2, 3, 4], "each id once");
    assert!(plan.pending[0].is_empty());
    assert!(matches!(plan.dispositions[&2], Disposition::Held { token: Some(_), .. }));
    assert_eq!(plan.dispositions[&3], Disposition::Issued { token: None });
    assert_eq!(plan.dispositions[&4], Disposition::Queued);
}

#[test]
fn unreadable_marker_restores_no_plugin_and_tells_once() {
    let mut rig = Rig::new();
    let rec = record(slot(vec![plug(1), fx(2, UnitKind::Echo)], vec![]), slot(vec![], vec![]));
    rig.open(rec.clone(), MarkerRead::Unreadable);
    assert!(rig.held(1));
    assert!(!rig.trace.iter().any(|a| matches!(a, Action::Scan(_) | Action::Commit)), "no scan, the record untouched");
    assert!(rig.pos(|a| *a == Action::RemoveMarker).expect("the marker goes") < rig.pos_add(2));
    assert_eq!(rig.adds(), [2], "built-ins are restored");
    let notice = rig.m.notice().cloned().expect("told");
    assert!(notice.plugins_held && notice.forgotten.is_empty());
    rig.feed(Event::Opened { record: rec, marker: MarkerRead::Unreadable });
    assert_eq!(rig.m.notice(), Some(&notice), "the same notice, told once");
    assert_eq!(rig.logs(), 1);
}

#[test]
fn closing_stops_every_action() {
    let mut rig = Rig::new().answer(2, &[Ans::Hang]);
    rig.open(record(slot(vec![plug(1), plug(2), fx(3, UnitKind::Echo)], vec![]), slot(vec![], vec![])), MarkerRead::None);
    let t1 = rig.token(1);
    assert_eq!(rig.disk, Some(vec![1, 2]));
    // SEAT-C: 1 is inside its settle and goes; 2 is still loading and stays.
    assert_eq!(rig.feed(Event::Closing), [Action::CommitMarker(vec![2]), Action::Done]);
    let late = [
        Event::AddAnswered(Ok(AddOk { state: AddState::Seated, token: rig.token(2) })),
        Event::SettleElapsed { token: t1 },
        Event::DeviceRuns,
        Event::GenMoved { gen: 2 },
        Event::Taken { gen: 2 },
        Event::Committed,
        Event::LatchSet { token: t1 },
        Event::ViewChanged(RigView { rev: 99, ..RigView::default() }),
        Event::Opened { record: record(slot(vec![], vec![]), slot(vec![], vec![])), marker: MarkerRead::None },
        Event::Closing,
    ];
    for event in late {
        assert!(rig.feed(event.clone()).is_empty(), "{event:?} after close");
    }
    assert_eq!(rig.adds(), [1, 2]);
}

// ── beside the plan's ────────────────────────────────────────────────────────────────────────────

#[test]
fn leftover_marker_commit_failure_restores_nothing_and_keeps_marker() {
    let mut rig = Rig::new();
    rig.fail_writes = vec![1];
    rig.open(record(slot(vec![plug(1), fx(2, UnitKind::Echo)], vec![]), slot(vec![], vec![])), MarkerRead::Names(vec![1]));
    assert_eq!(rig.trace.iter().filter(|a| **a == Action::Commit).count(), 1);
    assert!(rig.adds().is_empty());
    assert!(!rig.trace.iter().any(|a| matches!(a, Action::Forget { .. } | Action::RemoveMarker)));
    assert!(rig.held(1) && rig.held(2));
    assert_eq!(rig.disk, Some(vec![1]), "kept: the next launch forgets it");
    assert!(rig.m.notice().is_none());
}

#[test]
fn gen_moved_waits_for_the_new_engine_before_the_next_add() {
    let mut rig = Rig::new().answer(1, &[Ans::Hang]);
    rig.open(record(slot(vec![fx(1, UnitKind::Echo), fx(2, UnitKind::Reverb)], vec![]), slot(vec![], vec![])), MarkerRead::None);
    assert!(rig.feed(Event::GenMoved { gen: 2 }).is_empty());
    rig.gen = 2;
    let after = rig.feed(Event::AddAnswered(Ok(AddOk { state: AddState::Seated, token: rig.token(1) })));
    assert_eq!(after[0], Action::WaitTaken, "{after:#?}");
    assert_eq!(rig.adds(), [1, 2]);

    // A take by the old generation does not count.
    let mut rig = Rig::new();
    rig.takes = false;
    rig.feed(Event::GenMoved { gen: 3 });
    rig.open(record(slot(vec![fx(1, UnitKind::Echo)], vec![]), slot(vec![], vec![])), MarkerRead::None);
    assert_eq!(rig.trace.last(), Some(&Action::WaitTaken));
    assert_eq!(rig.feed(Event::Taken { gen: 1 }), [Action::WaitTaken], "taken by generation 1 only");
    rig.feed(Event::Taken { gen: 3 });
    assert_eq!(rig.adds(), [1]);
}

#[test]
fn yielded_source_and_player_removal_of_a_held_plate() {
    let mut rig = Rig::new().answer(1, &[Ans::SourceTaken]).answer(2, &[Ans::Seat]);
    rig.open(record(slot(vec![plug(1)], vec![]), slot(vec![fx(2, UnitKind::Echo)], vec![])), MarkerRead::None);
    assert_eq!(rig.disposition(1), Some(&Disposition::Yielded));
    assert!(!rig.trace.contains(&Action::Forget { id: 1 }), "a yielded unit keeps its tone");
    assert!(rig.held(2));
    rig.remove_plate(2);
    assert!(rig.disposition(2).is_none() && rig.m.plan().units[1].is_empty(), "the player removed its failed plate");
}

// ── review round 1 ───────────────────────────────────────────────────────────────────────────────

#[test]
fn id_reuse_a_plate_not_applied_keeps_the_hold_an_applied_one_wins_the_id() {
    // A held unit's failed plate goes, and a plate of another token loads under its id: the hold stays.
    let mut rig = Rig::new().answer(2, &[Ans::Seat]);
    rig.open(record(slot(vec![fx(1, UnitKind::Echo), plug(2)], vec![]), slot(vec![], vec![])), MarkerRead::None);
    let t2 = rig.token(2);
    assert!(rig.held(2));
    rig.view.slots[0].retain(|p| p.id != 2);
    rig.view.slots[0].push(plate(&plug(2), 900, PlateState::Loading, false, false));
    let event = rig.view_event();
    rig.feed(event);
    assert_eq!(rig.disposition(2), Some(&Disposition::Held { why: "port full".into(), token: Some(t2) }), "a loading plate ends nothing");
    assert_eq!(rig.m.plan().units[0].iter().map(|u| u.id).collect::<Vec<_>>(), [1, 2]);
    // It applies: the player's unit wins the id, and the restore adds nothing under it.
    rig.set_plate(2, PlateState::Seated, true, false);
    assert_eq!(rig.disposition(2), None);
    assert_eq!(rig.m.plan().units[0].iter().map(|u| u.id).collect::<Vec<_>>(), [1]);
    assert_eq!(rig.adds(), [1, 2]);

    // A unit still queued (no device yet): an applied plate of another unit under its id wins it.
    let mut rig = Rig::new();
    rig.device = false;
    rig.open(record(slot(vec![fx(1, UnitKind::Echo), fx(2, UnitKind::Ring)], vec![]), slot(vec![], vec![])), MarkerRead::None);
    rig.view.slots[1].push(plate(&fx(1, UnitKind::Reverb), 900, PlateState::Seated, true, false));
    let event = rig.view_event();
    rig.feed(event);
    assert_eq!(rig.disposition(1), None, "the player's unit holds the id");
    rig.device = true;
    rig.feed(Event::DeviceRuns);
    assert_eq!(rig.adds(), [2], "nothing added under the id");
    assert_eq!(rig.add_of(2).1, Place::After(vec![]));
}

#[test]
fn marker_rewrite_does_not_wait_for_a_blocked_add() {
    let mut rig = Rig::new().answer(2, &[Ans::Hang]);
    rig.open(record(slot(vec![plug(1), fx(2, UnitKind::Echo)], vec![]), slot(vec![], vec![])), MarkerRead::None);
    assert_eq!(rig.adds(), [1, 2], "add 2 is out and does not answer");
    assert_eq!(rig.disk, Some(vec![1]));
    assert_eq!(rig.feed(Event::SettleElapsed { token: rig.token(1) }), [Action::CommitMarker(vec![])]);
    assert_eq!(rig.disk, Some(vec![]));
}

#[test]
fn failed_marker_rewrite_retries_with_backoff() {
    // Writes: 1 the first marker, 2 plugin 1's load, 3 and 4 its rewrite (both fail), 5 the rewrite again.
    let mut rig = Rig::new().answer(2, &[Ans::Hang]);
    rig.fail_writes = vec![3, 4];
    rig.open(record(slot(vec![plug(1), fx(2, UnitKind::Echo)], vec![]), slot(vec![], vec![])), MarkerRead::None);
    let after = rig.feed(Event::SettleElapsed { token: rig.token(1) });
    assert_eq!(after[0], Action::CommitMarker(vec![]));
    assert!(matches!(after[1], Action::Log(_)));
    assert_eq!(after[2..], [Action::Backoff { after: Duration::from_millis(20) }]);
    assert_eq!(rig.feed(Event::BackoffElapsed), [Action::CommitMarker(vec![]), Action::Backoff { after: Duration::from_millis(40) }], "one log per run");
    assert_eq!(rig.disk, Some(vec![1]));
    assert_eq!(rig.feed(Event::BackoffElapsed), [Action::CommitMarker(vec![])]);
    assert_eq!(rig.disk, Some(vec![]));
}

#[test]
fn latch_after_a_plate_unseen_by_any_view_is_terminal() {
    let mut rig = Rig::new().answer(1, &[Ans::UnresolvedUnseen { exited: false }]);
    rig.open(record(slot(vec![plug(1)], vec![]), slot(vec![], vec![])), MarkerRead::None);
    let token = rig.token(1);
    assert_eq!(rig.disk, Some(vec![1]));
    assert_eq!(rig.feed(Event::LatchSet { token }), [Action::RemoveMarker, Action::Done]);
    assert_eq!(rig.disposition(1), Some(&Disposition::Held { why: "its add failed with no answer to trust".into(), token: Some(token) }));
}

#[test]
fn failed_then_applied_then_latch_keeps_the_settle() {
    let mut rig = Rig::new().answer(1, &[Ans::Unresolved { exited: false }]);
    rig.open(record(slot(vec![plug(1)], vec![]), slot(vec![], vec![])), MarkerRead::None);
    let token = rig.token(1);
    rig.set_plate(1, PlateState::Failed, false, false);
    assert!(rig.set_plate(1, PlateState::Seated, true, false).contains(&Action::Settle { token }));
    assert!(rig.feed(Event::LatchSet { token }).is_empty(), "applied: only its settle ends it");
    assert_eq!(rig.disk, Some(vec![1]));
    assert_eq!(rig.feed(Event::SettleElapsed { token }), [Action::RemoveMarker, Action::Done]);
}

#[test]
fn forget_is_durable_before_its_tone_and_marker() {
    let rec = record(slot(vec![plug(1), fx(2, UnitKind::Echo)], vec![]), slot(vec![], vec![]));
    let mut rig = Rig::new().answer(1, &[Ans::Plugin]);
    rig.open(rec.clone(), MarkerRead::None);
    let commit = rig.pos(|a| *a == Action::Commit).expect("the record without the unit");
    let forget = rig.pos(|a| *a == Action::Forget { id: 1 }).expect("its tone files go");
    let removed = rig.pos(|a| *a == Action::RemoveMarker).expect("the marker goes");
    assert!(commit < forget && forget < removed, "{:#?}", rig.trace);
    assert!(rig.done());

    // The record write fails: its tone stays and the marker keeps naming it, until a retry commits.
    // Writes: 1 the first marker, 2 plugin 1's load, 3 the forget's record (fails), 4 the retry.
    let mut rig = Rig::new().answer(1, &[Ans::Plugin]);
    rig.fail_writes = vec![3];
    rig.open(rec, MarkerRead::None);
    assert!(rig.trace.contains(&Action::Commit));
    assert!(!rig.trace.contains(&Action::Forget { id: 1 }));
    assert_eq!(rig.disk, Some(vec![1]), "a crash now forgets it at the next launch");
    assert_eq!(rig.trace.last(), Some(&Action::Backoff { after: Duration::from_millis(20) }));
    assert!(!rig.done());
    let after = rig.feed(Event::BackoffElapsed);
    assert_eq!(after[..2], [Action::Commit, Action::Forget { id: 1 }]);
    assert_eq!(rig.disk, None);
    assert!(rig.done());
}

#[test]
fn close_between_a_marker_write_and_its_ack() {
    // Writes: 1 the first marker, 2 plugin 1's load, 3 plugin 2's load: out when close comes.
    let mut rig = Rig::new().answer(1, &[Ans::Unresolved { exited: false }]);
    rig.hold_writes_from = 3;
    rig.open(record(slot(vec![plug(1)], vec![]), slot(vec![plug(2)], vec![])), MarkerRead::None);
    assert_eq!(rig.trace.last(), Some(&Action::CommitMarker(vec![1, 2])));
    assert_eq!(rig.adds(), [1]);
    // 2's add never started: close keeps 1 alone, written after the write out.
    assert_eq!(rig.feed(Event::Closing), [Action::CommitMarker(vec![1]), Action::Done]);
}

#[test]
fn close_reads_an_applied_plate_before_its_add_answers() {
    let mut rig = Rig::new().answer(1, &[Ans::Hang]);
    rig.open(record(slot(vec![plug(1)], vec![]), slot(vec![], vec![])), MarkerRead::None);
    assert_eq!(rig.disk, Some(vec![1]));
    rig.set_plate(1, PlateState::Seated, true, false);
    // SEAT-C: applied, so terminal at a clean close, though its answer never came.
    assert_eq!(rig.feed(Event::Closing), [Action::RemoveMarker, Action::Done]);
}

#[test]
fn host_matched_answer_is_matched_and_leaves_the_marker() {
    let mut rig = Rig::new().answer(1, &[Ans::Matched]);
    rig.open(record(slot(vec![plug(1), fx(2, UnitKind::Echo)], vec![]), slot(vec![], vec![])), MarkerRead::None);
    let token = rig.token(1);
    assert_eq!(rig.disposition(1), Some(&Disposition::Matched { token }));
    assert!(!rig.trace.contains(&Action::Settle { token }), "no plugin code ran for the attempt");
    assert_eq!(rig.adds(), [1, 2]);
    assert_eq!(rig.disk, None);
    assert!(rig.done());
}

// ── review round 2 ───────────────────────────────────────────────────────────────────────────────

/// The record unit ids a projection writes, units and pending, both slots, with their row counts.
fn rows(r: &RigRecord) -> BTreeMap<UnitId, usize> {
    let mut rows = BTreeMap::new();
    for unit in r.slots.iter().flat_map(|s| s.units.iter().chain(&s.pending)) {
        *rows.entry(unit.id).or_insert(0) += 1;
    }
    rows
}

impl Rig {
    /// The record the projection writes now, from the restore's plan and the fake's model.
    fn projected(&self, rec: &RigRecord) -> RigRecord {
        project(rec, self.m.plan(), &self.view, [None, None])
    }

    /// Put a plate the player added under `unit`'s id in slot `slot` (applied or not), and feed the view.
    fn player_plate(&mut self, slot: usize, unit: &UnitRecord, token: InstallToken, applied: bool) -> Vec<Action> {
        let state = if applied { PlateState::Seated } else { PlateState::Loading };
        self.view.slots[slot].push(plate(unit, token, state, applied, false));
        let event = self.view_event();
        self.feed(event)
    }
}

#[test]
fn latch_on_a_plate_no_view_showed_keeps_the_unit_in_the_projection() {
    let rec = record(slot(vec![plug(1), fx(2, UnitKind::Echo)], vec![]), slot(vec![], vec![]));
    let mut rig = Rig::new().answer(1, &[Ans::UnresolvedUnseen { exited: false }]);
    rig.open(rec.clone(), MarkerRead::None);
    let token = rig.token(1);
    rig.feed(Event::LatchSet { token });
    assert_eq!(rig.disposition(1), Some(&Disposition::Held { why: "its add failed with no answer to trust".into(), token: Some(token) }));
    let out = rig.projected(&rec);
    assert_eq!(out.slots[0].units.iter().map(|u| u.id).collect::<Vec<_>>(), [1, 2], "held: kept at its place for the next launch");
    // A later view that still never showed it ends nothing.
    rig.remove_plate(2);
    assert!(rig.held(1));
    assert_eq!(rig.projected(&rec).slots[0].units.iter().map(|u| u.id).collect::<Vec<_>>(), [1]);
}

#[test]
fn a_pending_id_the_player_won_is_not_placed_after_his_unit_leaves() {
    // Slot A waits on its plugin head with id 5 pending; the player puts a unit 5 in slot B.
    let rec = record(slot(vec![plug(1)], vec![fx(5, UnitKind::Ring)]), slot(vec![], vec![]));
    let mut rig = Rig::new().answer(1, &[Ans::Waiting]);
    rig.open(rec.clone(), MarkerRead::None);
    rig.player_plate(1, &fx(5, UnitKind::Echo), 900, true);
    assert!(rig.m.plan().pending[0].is_empty(), "the player's unit won the id");
    assert_eq!(rows(&rig.projected(&rec)), BTreeMap::from([(1, 1), (5, 1)]));
    assert_eq!(rig.projected(&rec).slots[1].units[0].body, fx(5, UnitKind::Echo).body);
    // He removes it, and A's head applies: nothing comes back under 5.
    rig.remove_plate(5);
    rig.set_plate(1, PlateState::Seated, true, false);
    assert_eq!(rig.adds(), [1]);
    assert!(!rig.m.plan().ids().contains(&5));
    assert_eq!(rows(&rig.projected(&rec)), BTreeMap::from([(1, 1)]));
}

#[test]
fn a_won_id_during_its_marker_write_leaves_the_marker() {
    // Writes: 1 the first marker, 2 plugin 1's load (out when the player's unit wins the id).
    let mut rig = Rig::new();
    rig.hold_writes_from = 2;
    rig.open(record(slot(vec![plug(1)], vec![]), slot(vec![], vec![])), MarkerRead::None);
    assert_eq!(rig.trace.last(), Some(&Action::CommitMarker(vec![1])));
    rig.player_plate(0, &fx(1, UnitKind::Echo), 900, true);
    assert_eq!(rig.disposition(1), None);
    // The write lands: no add follows, and its load leaves the marker.
    rig.hold_writes_from = usize::MAX;
    rig.disk = Some(vec![1]);
    rig.feed(Event::Committed);
    assert!(rig.adds().is_empty());
    assert_eq!(rig.disk, None, "{:#?}", rig.trace);
    assert!(rig.done());
}

#[test]
fn unresolved_with_its_latch_set_before_any_view_is_terminal() {
    let mut rig = Rig::new().answer(1, &[Ans::UnresolvedUnseen { exited: true }]);
    rig.open(record(slot(vec![plug(1)], vec![]), slot(vec![], vec![])), MarkerRead::None);
    let token = rig.token(1);
    assert_eq!(rig.disposition(1), Some(&Disposition::Held { why: "its add failed with no answer to trust".into(), token: Some(token) }));
    assert_eq!(rig.disk, None, "failed with its owner gone: the load left the marker");
    assert!(rig.done());
}

#[test]
fn failed_rewrite_keeps_its_backoff_when_the_add_answers_and_the_removal_retries() {
    // Writes: 1 the first marker, 2 plugin 1's load, 3 its rewrite (fails), 4 the removal (fails), 5 it again.
    let mut rig = Rig::new().answer(2, &[Ans::Hang]);
    rig.fail_writes = vec![3, 4];
    rig.open(record(slot(vec![plug(1), fx(2, UnitKind::Echo)], vec![]), slot(vec![], vec![])), MarkerRead::None);
    let after = rig.feed(Event::SettleElapsed { token: rig.token(1) });
    assert_eq!(after.last(), Some(&Action::Backoff { after: Duration::from_millis(20) }));
    // The add answers before the timer: nothing is written meanwhile.
    let after = rig.feed(Event::AddAnswered(Ok(AddOk { state: AddState::Seated, token: rig.token(2) })));
    assert!(after.is_empty(), "{after:#?}");
    assert_eq!(rig.feed(Event::BackoffElapsed), [Action::RemoveMarker, Action::Backoff { after: Duration::from_millis(40) }], "one log per run");
    assert_eq!(rig.disk, Some(vec![1]));
    assert!(!rig.done(), "never done with the marker on disk");
    assert_eq!(rig.feed(Event::BackoffElapsed), [Action::RemoveMarker, Action::Done]);
    assert_eq!(rig.disk, None);
}

#[test]
fn close_after_a_gen_move_drops_a_load_never_issued() {
    // Writes: 1 the first marker, 2 plugin 1's load (out while the engine generation moves).
    let mut rig = Rig::new();
    rig.hold_writes_from = 2;
    rig.open(record(slot(vec![plug(1)], vec![]), slot(vec![], vec![])), MarkerRead::None);
    rig.feed(Event::GenMoved { gen: 2 });
    rig.takes = false;
    rig.hold_writes_from = usize::MAX;
    rig.disk = Some(vec![1]);
    assert_eq!(rig.feed(Event::Committed), [Action::WaitTaken]);
    // Its add never started: close leaves nothing for the next launch to forget.
    assert_eq!(rig.feed(Event::Closing), [Action::RemoveMarker, Action::Done]);
    assert_eq!(rig.disk, None);
}

#[test]
fn forget_keeps_the_tone_files_of_a_player_unit_under_the_id() {
    // Writes: 1 the first marker, 2 plugin 1's load, 3 the forget's record (out when the player's unit
    // takes the id).
    let rec = record(slot(vec![plug(1)], vec![]), slot(vec![], vec![]));
    let mut rig = Rig::new().answer(1, &[Ans::Plugin]);
    rig.hold_writes_from = 3;
    rig.open(rec.clone(), MarkerRead::None);
    assert_eq!(rig.trace.last(), Some(&Action::Commit));
    rig.view.slots[0].retain(|p| p.id != 1);
    rig.player_plate(0, &plug(1), 900, true);
    assert_eq!(rows(&rig.projected(&rec)), BTreeMap::from([(1, 1)]), "the commit keeps the player's unit");
    rig.hold_writes_from = usize::MAX;
    rig.feed(Event::Committed);
    assert!(!rig.trace.contains(&Action::Forget { id: 1 }), "its tone files are the player's unit's");
    assert_eq!(rig.disk, None);
    assert!(rig.done());
}

#[test]
fn a_won_id_leaves_the_marker_while_the_restore_load_runs() {
    let mut rig = Rig::new().answer(1, &[Ans::Unresolved { exited: false }]);
    rig.open(record(slot(vec![plug(1)], vec![]), slot(vec![], vec![])), MarkerRead::None);
    // Its plate leaves while its owner may still be in plugin code: the marker keeps it.
    rig.remove_plate(1);
    assert_eq!(rig.disposition(1), None);
    assert_eq!(rig.disk, Some(vec![1]));
    // The player's unit under the id applies: an entry under it could only forget his unit.
    rig.player_plate(0, &plug(1), 900, true);
    assert_eq!(rig.disk, None, "{:#?}", rig.trace);
}

/// Each id a projection writes: its slot, whether it is `pending`, and the unit as written.
fn cells(r: &RigRecord) -> BTreeMap<UnitId, Vec<(usize, bool, UnitRecord)>> {
    let mut cells: BTreeMap<UnitId, Vec<_>> = BTreeMap::new();
    for (s, slot) in r.slots.iter().enumerate() {
        let all = slot.units.iter().map(|u| (false, u)).chain(slot.pending.iter().map(|u| (true, u)));
        for (pending, unit) in all {
            cells.entry(unit.id).or_default().push((s, pending, unit.clone()));
        }
    }
    cells
}

/// Feed a view of the fake's model, and check the restore and the projection judge every id alike:
/// what the plan keeps after the view is what the projection writes at it (the plan's own unit in its
/// own slot for a unit at its place, the plate's slot for one its plate stands for), and the
/// projection writes the same units, in the same slots, with the plan before the view as after it (one
/// rule, either side of the view).
fn view_agrees(rig: &mut Rig, rec: &RigRecord) {
    let before = rig.m.plan().clone();
    let event = rig.view_event();
    let Event::ViewChanged(view) = &event else { unreachable!() };
    let view = view.clone();
    rig.feed(event);
    let after = rig.m.plan();
    let (old, new) = (project(rec, &before, &view, [None, None]), project(rec, after, &view, [None, None]));
    assert_eq!(cells(&old), cells(&new), "the plan before the view and after it");
    assert!(rows(&new).values().all(|&n| n == 1), "each id once: {:?}", rows(&new));
    let written = cells(&new);
    for id in after.ids() {
        let cell = written.get(&id).map(|c| c[0].clone());
        match after.fate(id, &view) {
            Some(Fate::AtPlace) => {
                let s = (0..2).find(|s| after.units[*s].iter().chain(&after.pending[*s]).any(|u| u.id == id)).expect("the plan holds it");
                let pending = after.pending[s].iter().any(|u| u.id == id);
                let unit = after.units[s].iter().chain(&after.pending[s]).find(|u| u.id == id).cloned().expect("the plan holds it");
                assert_eq!(cell, Some((s, pending, unit)), "{id} at its place: the plan's unit in the plan's slot");
            }
            Some(Fate::Plate(t)) => {
                let s = (0..2).find(|s| view.slots[*s].iter().any(|p| p.added_by == t)).expect("its plate is in the view");
                assert_eq!(cell.map(|(slot, pending, _)| (slot, pending)), Some((s, false)), "{id} from its plate, in the plate's slot");
            }
            Some(Fate::Ended) => panic!("{id} ended but the restore kept it"),
            Some(Fate::Dropped) | None => {}
        }
    }
    for s in 0..2 {
        let pending: Vec<UnitId> = new.slots[s].pending.iter().map(|u| u.id).collect();
        assert_eq!(pending, after.pending[s].iter().map(|u| u.id).collect::<Vec<_>>(), "slot {s}'s pending");
    }
    let applied = |id: UnitId| view.slots.iter().flatten().any(|p| p.id == id && p.applied);
    let kept = |id: UnitId| matches!(after.dispositions.get(&id), Some(d) if !matches!(d, Disposition::Forgotten | Disposition::Yielded));
    let units: Vec<UnitId> = new.slots.iter().flat_map(|s| s.units.iter().map(|u| u.id)).collect();
    for id in after.dispositions.keys().filter(|id| kept(**id)) {
        assert!(units.contains(id), "the restore keeps {id}; the projection must write it: {units:?}");
    }
    for id in units {
        assert!(kept(id) || applied(id), "{id} written: neither kept by the restore nor the player's applied unit");
    }
}

#[test]
fn projection_and_restore_agree_on_every_id() {
    let rec = record(
        slot(vec![plug(1), fx(2, UnitKind::Echo), fx(3, UnitKind::Ring)], vec![fx(5, UnitKind::Reverb)]),
        slot(vec![fx(4, UnitKind::Echo), fx(7, UnitKind::Ring)], vec![fx(6, UnitKind::Ott)]),
    );
    let mut rig = Rig::new().answer(1, &[Ans::UnresolvedUnseen { exited: false }]).answer(3, &[Ans::Hang]);
    rig.open(rec.clone(), MarkerRead::None);
    // 1 unresolved, never shown; 2 seated; 3's add out; 4 queued behind it; B placed 6 behind 4.
    assert_eq!(rig.adds(), [1, 2, 3]);
    rig.feed(Event::LatchSet { token: rig.token(1) });
    assert!(rig.held(1));
    view_agrees(&mut rig, &rec);
    // The player's unit under A's pending id 5, in B.
    rig.view.slots[1].push(plate(&fx(5, UnitKind::Echo), 900, PlateState::Seated, true, false));
    view_agrees(&mut rig, &rec);
    // His unit under queued 4, in A; his loading plate under 6.
    rig.view.slots[0].push(plate(&fx(4, UnitKind::Reverb), 901, PlateState::Seated, true, false));
    rig.view.slots[1].push(plate(&fx(6, UnitKind::Ring), 902, PlateState::Loading, false, false));
    view_agrees(&mut rig, &rec);
    // He removes restored 2 and his 5.
    rig.view.slots[0].retain(|p| p.id != 2);
    rig.view.slots[1].retain(|p| p.id != 5);
    view_agrees(&mut rig, &rec);
    // While 3's add blocks, he applies an exact match for queued 7, then removes it.
    rig.view.slots[1].push(plate(&fx(7, UnitKind::Ring), 903, PlateState::Seated, true, false));
    view_agrees(&mut rig, &rec);
    assert_eq!(rig.disposition(7), Some(&Disposition::Matched { token: 903 }));
    rig.view.slots[1].retain(|p| p.id != 7);
    view_agrees(&mut rig, &rec);
    let new = rig.projected(&rec);
    assert_eq!(rows(&new).keys().copied().collect::<Vec<_>>(), [1, 3, 4, 6]);
    // 3's add answers: nothing comes back under 7.
    rig.set_plate(3, PlateState::Seated, true, false);
    rig.feed(Event::AddAnswered(Ok(AddOk { state: AddState::Seated, token: rig.token(3) })));
    assert!(!rig.adds().contains(&7), "{:?}", rig.adds());
    assert_eq!(rows(&rig.projected(&rec)).keys().copied().collect::<Vec<_>>(), [1, 3, 4, 6]);
}

// ── review round 3 ───────────────────────────────────────────────────────────────────────────────

#[test]
fn a_unit_removed_before_any_view_showed_its_seated_or_waiting_add_stays_removed() {
    // 1's plate seats and the player removes it before any view shows it; then its add answers.
    let rec = record(slot(vec![fx(1, UnitKind::Echo), fx(2, UnitKind::Ring)], vec![]), slot(vec![], vec![]));
    let mut rig = Rig::new().answer(1, &[Ans::Hang]);
    rig.open(rec.clone(), MarkerRead::None);
    let token = rig.token(1);
    rig.view.slots[0].retain(|p| p.id != 1);
    rig.feed(Event::AddAnswered(Ok(AddOk { state: AddState::Seated, token })));
    assert_eq!(rig.adds(), [1, 2]);
    assert_eq!(rig.disposition(1), None, "the answer saw its plate: the next view's lack of it is a removal");
    assert_eq!(rows(&rig.projected(&rec)), BTreeMap::from([(2, 1)]));

    // The same for a held install (`Waiting`) of a plugin.
    let rec = record(slot(vec![plug(1), fx(2, UnitKind::Ring)], vec![]), slot(vec![], vec![]));
    let mut rig = Rig::new().answer(1, &[Ans::Hang]);
    rig.open(rec.clone(), MarkerRead::None);
    let token = rig.token(1);
    rig.view.slots[0].retain(|p| p.id != 1);
    rig.feed(Event::AddAnswered(Ok(AddOk { state: AddState::Waiting, token })));
    assert_eq!(rig.adds(), [1, 2]);
    assert_eq!(rig.disposition(1), None);
    assert_eq!(rows(&rig.projected(&rec)), BTreeMap::from([(2, 1)]));
}

#[test]
fn a_match_removed_while_another_add_blocks_is_not_added() {
    let rec = record(slot(vec![plug(1)], vec![]), slot(vec![fx(2, UnitKind::Echo)], vec![]));
    let mut rig = Rig::new().answer(1, &[Ans::Hang]);
    rig.open(rec.clone(), MarkerRead::None);
    assert_eq!(rig.adds(), [1]);
    // While 1's add blocks, the player applies an exact match for queued 2: matched at that view.
    rig.player_plate(1, &fx(2, UnitKind::Echo), 900, true);
    assert_eq!(rig.disposition(2), Some(&Disposition::Matched { token: 900 }));
    // He removes it, and 1's add answers: nothing comes back under 2.
    rig.remove_plate(2);
    assert_eq!(rig.disposition(2), None);
    rig.set_plate(1, PlateState::Seated, true, false);
    rig.feed(Event::AddAnswered(Ok(AddOk { state: AddState::Seated, token: rig.token(1) })));
    assert_eq!(rig.adds(), [1]);
    assert_eq!(rows(&rig.projected(&rec)), BTreeMap::from([(1, 1)]));
}

#[test]
fn a_failed_placement_commit_keeps_out_an_id_that_ended_meanwhile() {
    // Writes: 1 the first marker, 2 slot A's placement (out while the player's unit wins 5 and leaves).
    let rec = record(slot(vec![fx(1, UnitKind::Echo)], vec![fx(5, UnitKind::Ring), fx(6, UnitKind::Reverb)]), slot(vec![], vec![]));
    let mut rig = Rig::new();
    rig.hold_writes_from = 2;
    rig.open(rec.clone(), MarkerRead::None);
    assert_eq!(rig.trace.last(), Some(&Action::Commit));
    assert_eq!(rig.m.plan().units[0].iter().map(|u| u.id).collect::<Vec<_>>(), [1, 5, 6]);
    rig.player_plate(1, &fx(5, UnitKind::Echo), 900, true);
    assert!(!rig.m.plan().ids().contains(&5));
    rig.remove_plate(5);
    rig.hold_writes_from = usize::MAX;
    rig.feed(Event::CommitFailed);
    // The rollback keeps `pending` as the record holds it, less 5.
    assert_eq!(rig.m.plan().pending[0], [fx(6, UnitKind::Reverb)]);
    assert_eq!(rows(&rig.projected(&rec)), BTreeMap::from([(1, 1), (6, 1)]));
    assert_eq!(rig.adds(), [1]);
}

#[test]
fn a_matched_load_leaves_a_marker_another_load_still_holds() {
    // Writes: 1 the first marker, 2 plugin 1's load, 3 plugin 2's load (out while the player's exact
    // match for 2 applies). 1 stays unresolved.
    let rec = record(slot(vec![plug(1)], vec![]), slot(vec![plug(2)], vec![]));
    let mut rig = Rig::new().answer(1, &[Ans::Unresolved { exited: false }]);
    rig.hold_writes_from = 3;
    rig.open(rec.clone(), MarkerRead::None);
    assert_eq!(rig.trace.last(), Some(&Action::CommitMarker(vec![1, 2])));
    rig.player_plate(1, &plug(2), 900, true);
    rig.hold_writes_from = usize::MAX;
    rig.disk = Some(vec![1, 2]);
    rig.feed(Event::Committed);
    assert_eq!(rig.disk, Some(vec![1]), "{:#?}", rig.trace);
    assert_eq!(rig.adds(), [1]);

    // The host finds 2 under its slot lock (a plate after the machine's last view): the same.
    let mut rig = Rig::new().answer(1, &[Ans::Unresolved { exited: false }]).answer(2, &[Ans::Matched]);
    rig.open(rec, MarkerRead::None);
    assert!(rig.trace.contains(&Action::CommitMarker(vec![1, 2])));
    assert_eq!(rig.adds(), [1, 2]);
    assert_eq!(rig.disk, Some(vec![1]), "{:#?}", rig.trace);
}

#[test]
fn a_failed_leftover_removal_retries_and_is_never_done_with_it_on_disk() {
    // Write 1, the leftover marker's removal, fails; 2 removes it.
    let rec = record(slot(vec![], vec![]), slot(vec![], vec![]));
    let mut rig = Rig::new();
    rig.fail_writes = vec![1];
    rig.open(rec.clone(), MarkerRead::Names(vec![9]));
    assert_eq!(rig.trace.last(), Some(&Action::Backoff { after: Duration::from_millis(20) }));
    assert_eq!(rig.disk, Some(vec![9]));
    assert!(!rig.done(), "never done with the marker on disk");
    assert_eq!(rig.feed(Event::BackoffElapsed), [Action::RemoveMarker, Action::Done]);
    assert_eq!(rig.disk, None);

    // Close before the retry removes it too.
    let mut rig = Rig::new();
    rig.fail_writes = vec![1];
    rig.open(rec, MarkerRead::Names(vec![9]));
    assert_eq!(rig.feed(Event::Closing), [Action::RemoveMarker, Action::Done]);
    assert_eq!(rig.disk, None);
}

#[test]
fn a_forget_whose_id_a_plate_holds_waits_for_it_to_leave() {
    // Writes: 1 the first marker, 2 plugin 1's load, 3 the forget's record (out when the player's
    // effect takes the id).
    let rec = record(slot(vec![plug(1)], vec![]), slot(vec![], vec![]));
    let start = || {
        let mut rig = Rig::new().answer(1, &[Ans::Plugin]);
        rig.hold_writes_from = 3;
        rig.open(rec.clone(), MarkerRead::None);
        assert_eq!(rig.trace.last(), Some(&Action::Commit));
        rig.view.slots[0].retain(|p| p.id != 1);
        rig.player_plate(0, &fx(1, UnitKind::Echo), 900, true);
        rig.hold_writes_from = usize::MAX;
        rig.feed(Event::Committed);
        assert!(!rig.trace.contains(&Action::Forget { id: 1 }), "the id is his effect's now");
        assert!(rig.done());
        rig
    };
    // He removes his effect: the forgotten plugin's tone files go at that view.
    let mut rig = start();
    assert_eq!(rig.remove_plate(1), [Action::Forget { id: 1 }]);
    // He keeps it until close: the files stay, told in the log.
    let mut rig = start();
    assert!(matches!(rig.feed(Event::Closing)[..], [Action::Log(_)]));
    assert!(!rig.trace.contains(&Action::Forget { id: 1 }));
}

// ── CP3a part 2, review round 1 ──────────────────────────────────────────────────────────────────

/// D9's match needs an applied plate (CP3a r1). The machine as the app's `Rig::new` runs it (fed the
/// record and every view, never seeded): a queued unit whose same-unit plate the player put in is shown
/// loading, then gone before it applied, stays in the plan at its place.
#[test]
fn a_same_unit_plate_gone_before_it_applied_leaves_the_unit_at_its_place() {
    let p = plug(1);
    let rec = record(slot(vec![p.clone()], vec![]), slot(vec![], vec![]));
    for state in [PlateState::Loading, PlateState::Waiting] {
        let mut m = Restore::new();
        assert!(matches!(m.handle(Event::Opened { record: rec.clone(), marker: MarkerRead::None })[..], [Action::Seed { .. }]));
        let mut view = RigView::default();
        view.rev = 1;
        view.slots[0].push(plate(&p, 900, state, false, false));
        m.handle(Event::ViewChanged(view.clone()));
        assert_eq!(m.plan().dispositions.get(&1), Some(&Disposition::Queued), "{state:?}: not the unit before it applies");
        view.rev = 2;
        view.slots[0].clear();
        m.handle(Event::ViewChanged(view.clone()));
        assert_eq!(m.plan().dispositions.get(&1), Some(&Disposition::Queued), "{state:?}: kept");
        assert_eq!(m.plan().fate(1, &view), Some(Fate::AtPlace));
        assert_eq!(rows(&project(&rec, m.plan(), &view, [None, None])), BTreeMap::from([(1, 1)]), "{state:?}: written at its place");
    }
}

/// At an add, a same-unit plate that has not applied holds the id: the restore holds the unit (no
/// token) and adds nothing. The player's plate wins the id if it applies; if it leaves first, the unit
/// stays at its place.
#[test]
fn a_same_unit_plate_not_applied_at_the_add_holds_the_unit() {
    let p = plug(1);
    let rec = record(slot(vec![p.clone()], vec![]), slot(vec![], vec![]));
    let start = || {
        let mut rig = Rig::new();
        rig.player_plate(0, &p, 900, false);
        rig.open(rec.clone(), MarkerRead::None);
        assert_eq!(rig.adds(), Vec::<UnitId>::new(), "never added over the player's attempt");
        assert_eq!(rig.disposition(1), Some(&Disposition::Held { why: "another unit holds its id".into(), token: None }));
        rig
    };
    // It applies: the player's unit wins the id, recorded from its plate.
    let mut rig = start();
    rig.set_plate(1, PlateState::Seated, true, false);
    assert_eq!(rig.disposition(1), None, "the player's unit won the id");
    assert_eq!(rows(&rig.projected(&rec)), BTreeMap::from([(1, 1)]));
    // It leaves before it applies: the unit stays at its place.
    let mut rig = start();
    rig.remove_plate(1);
    assert!(rig.held(1));
    assert_eq!(rows(&rig.projected(&rec)), BTreeMap::from([(1, 1)]));
}
