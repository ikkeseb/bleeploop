//! OWNS: the input ports: one midir connection per present port, each port's [`PortIdentity`], which
//! port a stored binding answers to ([`resolve`]), and hot-plug: Windows' interface notifications wake
//! the port thread (`liveness`), and a poll of the port list every [`POLL`] is the backstop. Between
//! enumerations the same thread runs the host's timers (`Core::tick`: learn's release waits, the
//! queue's retry, the binding store's writes). The identities, the diff and the resolution are pure
//! functions; the port thread that applies them is the only code here that touches midir, and only real
//! hardware runs it.
//!
//! A port is its [`PortIdentity`]: midir's id (WinMM's device-interface path, which the ports of one
//! multi-port device share), its name, and its position among the present ports sharing that path and
//! name (0 but for two same-named ports of one device). The name is part of the identity so the ports of
//! one device that WinMM lists in another order after a restart or a replug keep their own identities.
//! It is computed when a connection opens and kept for the connection's lifetime, never recounted
//! for it later. A port whose path is empty gets a weak identity, which never matches a stored id
//! exactly. WinMM input ports are exclusive on the classic driver (under `wdmaud2`, unknown): a port
//! another program holds fails to open, and is retried by the arrival schedule and then every poll.
//!
//! The poll compares whole identities (path, index, name), so a port listed again with a new path or
//! position reopens. What it cannot see: a port unplugged and replugged into the same socket inside one
//! poll comes back with the same identity, and only the removal notification closes its dead
//! connection. Without notifications (registration failed, or the port's class is not watched) that
//! connection stays open and silent until the port goes for longer than a poll.
//!
//! Allocation per message: midir's WinMM handler copies each message into a `Vec` it clears and
//! reuses, so a short message allocates only while that buffer first grows (SysEx, which could grow it
//! again, is ignored here). This side allocates MIDI learn's key for the message (a copy of the port's
//! id), a note's owner list on each note-on, the sweep list on a pedal-up or a release, and UI events on
//! a learn; `EngineHost::send` pushes into the engine's ring without allocating. None of these threads
//! is an audio thread.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::liveness::{batch, same_path, Generation, Registration, Wake, Watch};
use super::{Core, PortEntry};

/// How often the port list is read when no notification wakes the port thread.
pub(crate) const POLL: Duration = Duration::from_secs(1);

/// A present port, as a binding names it (`docs/plans/native-midi.md` decision 8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PortIdentity {
    /// midir's id: the WinMM device-interface path; empty when the driver gave none (weak).
    pub(crate) path: String,
    /// Among the present ports sharing this path and this name (a weak one: among the weak ports with
    /// this name), which one, in WinMM's order, from 0.
    pub(crate) index: u32,
    pub(crate) name: String,
}

impl PortIdentity {
    /// No device-interface path: the identity is the name and an ordinal, which another device with
    /// that name can take over.
    pub(crate) fn weak(&self) -> bool {
        self.path.is_empty()
    }

    /// The canonical id a binding stores as its `port_id`: index, path, name. Never `input-<N>` (a legacy
    /// Web MIDI id); the path is lowercased, as Windows compares paths without case. Unambiguous while a
    /// device-interface path holds no `:` (inferred: none seen does).
    pub(crate) fn id(&self) -> String {
        if self.weak() {
            format!("winmm-weak:{}:{}", self.index, self.name)
        } else {
            format!("winmm:{}:{}:{}", self.index, self.path.to_ascii_lowercase(), self.name)
        }
    }
}

/// Each listed port's identity, from `(path, name)` in WinMM's order.
pub(crate) fn identities(listed: &[(&str, &str)]) -> Vec<PortIdentity> {
    let shares = |(path, name): (&str, &str), (p, n): (&str, &str)| n == name && if path.is_empty() { p.is_empty() } else { same_path(p, path) };
    listed
        .iter()
        .enumerate()
        .map(|(i, &(path, name))| PortIdentity {
            path: path.into(),
            index: listed[..i].iter().filter(|&&other| shares((path, name), other)).count() as u32,
            name: name.into(),
        })
        .collect()
}

/// Which present port a stored binding answers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Resolution {
    /// `present[port]`. `reanchor`: the stored id is not that port's [`PortIdentity::id`]; the caller
    /// stores the port's id in its place.
    Port { port: usize, reanchor: bool },
    /// No port: the binding stays listed and fires nothing.
    Unresolved(Unresolved),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unresolved {
    /// No present port has its id, and no unclaimed present port carries its name.
    NoPort,
    /// Several unclaimed present ports carry its name.
    SeveralPorts,
    /// Another stored id with no present port carries its name too.
    SeveralAbsent,
}

/// Resolve stored bindings' `(port_id, port_name)` (repeats allowed) against one enumeration snapshot,
/// one result per entry, all at once: the caller persists the whole result together (decisions 8, 9).
///
/// 1. A stored id equal to a present port's strong id answers to it, and claims that port.
/// 2. Otherwise the name decides, only when exactly one UNCLAIMED present port carries it and exactly
///    one stored id with no present port carries it (this one). A legacy Web MIDI id (`input-<N>`) and
///    a weak id never match exactly, so they always take this rule. A legacy id named no port at all
///    (a per-run ordinal: its name is its identity), so for it every present port with the name counts,
///    claimed or not: of two same-named controllers, the one another binding claims could be the one
///    it was learned on.
/// 3. Everything else stays unresolved: an ambiguous identity never fires another controller's action.
pub(crate) fn resolve(stored: &[(&str, &str)], present: &[PortIdentity]) -> Vec<Resolution> {
    let ids: Vec<String> = present.iter().map(PortIdentity::id).collect();
    let exact: Vec<Option<usize>> =
        stored.iter().map(|&(id, _)| (0..present.len()).find(|&i| !present[i].weak() && ids[i] == id)).collect();
    let claimed = |i: usize| exact.contains(&Some(i));
    stored
        .iter()
        .zip(&exact)
        .map(|(&(id, name), &hit)| {
            if let Some(port) = hit {
                return Resolution::Port { port, reanchor: false };
            }
            let named: Vec<usize> = (0..present.len()).filter(|&i| present[i].name == name).collect();
            if legacy(id) && named.len() > 1 {
                return Resolution::Unresolved(Unresolved::SeveralPorts);
            }
            let ports: Vec<usize> = named.into_iter().filter(|&i| !claimed(i)).collect();
            let mut absent: Vec<&str> =
                stored.iter().zip(&exact).filter(|((_, n), e)| e.is_none() && *n == name).map(|((i, _), _)| *i).collect();
            absent.sort_unstable();
            absent.dedup();
            match (ports.as_slice(), absent.len()) {
                ([], _) => Resolution::Unresolved(Unresolved::NoPort),
                (&[port], 1) => Resolution::Port { port, reanchor: ids[port] != id },
                ([_], _) => Resolution::Unresolved(Unresolved::SeveralAbsent),
                _ => Resolution::Unresolved(Unresolved::SeveralPorts),
            }
        })
        .collect()
}

/// A legacy Web MIDI id (`input-<N>`, or the one the glue passes for every ordinal record of a name).
fn legacy(id: &str) -> bool {
    id.starts_with("input-")
}

/// A poll's changes: indices into the present list to open, and into the open list to close.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Diff {
    pub(crate) open: Vec<usize>,
    pub(crate) close: Vec<usize>,
}

/// Compare the open connections' ports with the ports listed now.
pub(crate) fn diff<T: PartialEq>(open: &[T], present: &[T]) -> Diff {
    Diff {
        open: (0..present.len()).filter(|&i| !open.contains(&present[i])).collect(),
        close: (0..open.len()).filter(|&i| !present.contains(&open[i])).collect(),
    }
}

/// Each present port's connection: the open one that follows it, or a fresh number when the diff opens
/// it (`opening`, listed again in the second list with its index), or none.
pub(crate) fn conns<T: PartialEq>(open: &[(T, u32)], present: &[T], opening: &[usize], next_conn: &mut u32) -> (Vec<Option<u32>>, Vec<(usize, u32)>) {
    let mut fresh = Vec::new();
    let conns = present
        .iter()
        .enumerate()
        .map(|(i, p)| {
            open.iter().find(|(o, _)| o == p).map(|&(_, conn)| conn).or_else(|| {
                opening.contains(&i).then(|| {
                    *next_conn = next_conn.wrapping_add(1);
                    fresh.push((i, *next_conn));
                    *next_conn
                })
            })
        })
        .collect();
    (conns, fresh)
}

/// One open connection, owned by the port thread.
struct Connection {
    /// Fixed when it opened (hole 1).
    identity: PortIdentity,
    conn: u32,
    generation: Generation,
    input: midir::MidiInputConnection<()>,
}

/// A connection whose port went away: its callbacks stop, then its notes and HOLD are released, then
/// WinMM closes it.
fn disconnect(core: &Core, c: Connection, gone: &mut Vec<String>) {
    c.generation.invalidate();
    if let Some(name) = core.port_gone(c.conn) {
        log::error!("[midi] input disconnected: {name}");
        gone.push(name);
    }
    c.input.close();
}

/// The port thread: register for interface notifications, then list, diff, close what went away, open
/// what arrived, publish the table, and until the next enumeration run the host's timers
/// (`Core::tick`), waking for the earliest of them, a notification, the arrival schedule or the poll,
/// until [`Wake::Stop`]. `wake` is the sender the notification callbacks use. On the way out the
/// notifications are unregistered, every port is released and closed, and the timers run once more (a
/// store write still due).
pub(crate) fn run(core: Arc<Core>, wake: Sender<Wake>, woken: Receiver<Wake>) {
    // Before the first enumeration, so a port arriving between the two is not missed.
    let registration = Registration::new(wake);
    let mut open: Vec<Connection> = Vec::new();
    let mut next_conn: u32 = 0;
    let mut failed: Vec<PortIdentity> = Vec::new();
    let mut watch = Watch::default();
    let mut gone = Vec::new();
    'run: loop {
        // `id()` allocates; the poll is not a hot path.
        let listed = list();
        let present = identities(&listed.iter().map(|(path, name, _)| (path.as_str(), name.as_str())).collect::<Vec<_>>());
        watch.listed(&present.iter().map(|p| p.path.as_str()).collect::<Vec<_>>());
        let d = diff(&open.iter().map(|c| &c.identity).collect::<Vec<_>>(), &present.iter().collect::<Vec<_>>());
        for &i in d.close.iter().rev() {
            disconnect(&core, open.remove(i), &mut gone);
        }

        // A port a removal closed stays closed while WinMM still lists it (`Watch`).
        let opening: Vec<usize> = d.open.into_iter().filter(|&i| watch.may_open(&present[i].path)).collect();
        let (port_conns, fresh) = {
            let open_conns: Vec<(&PortIdentity, u32)> = open.iter().map(|c| (&c.identity, c.conn)).collect();
            conns(&open_conns, &present.iter().collect::<Vec<_>>(), &opening, &mut next_conn)
        };
        let entries: Vec<PortEntry> = present
            .iter()
            .zip(port_conns)
            .map(|(identity, conn)| PortEntry::new(identity.clone(), conn, conn.is_none() && failed.contains(identity)))
            .collect();
        // Registered before it connects, so the first message finds its port.
        core.set_ports(entries);

        for (i, conn) in fresh {
            let identity = &present[i];
            let generation = Generation::default();
            match connect(&core, &listed[i].2, conn, &generation) {
                Ok(input) => {
                    log::info!("[midi] opened input {} as {}", identity.name, identity.id());
                    failed.retain(|f| f != identity);
                    open.push(Connection { identity: identity.clone(), conn, generation, input });
                }
                Err(e) => {
                    core.open_failed(conn);
                    if !failed.contains(identity) {
                        log::warn!("[midi] could not open input {}: {e}", identity.name);
                        failed.push(identity.clone());
                    }
                }
            }
        }
        core.publish_ports(std::mem::take(&mut gone));

        let enumerate = Instant::now() + watch.next_wait();
        loop {
            let now = Instant::now();
            let timer = core.tick(now);
            if now >= enumerate {
                break;
            }
            let until = timer.map_or(enumerate, |t| t.min(enumerate));
            match woken.recv_timeout(until.saturating_duration_since(now)) {
                Err(RecvTimeoutError::Timeout) | Ok(Wake::Tick) => {}
                Err(RecvTimeoutError::Disconnected) | Ok(Wake::Stop) => break 'run,
                Ok(first) => {
                    // The notices queued so far, at most `MAX_BATCH`, then one enumeration.
                    let (notices, stop) = batch(first, &woken);
                    if stop {
                        break 'run;
                    }
                    for w in notices {
                        let path = match &w {
                            Wake::Stop | Wake::Tick => continue,
                            Wake::Arrived(path) | Wake::Removed(path) => path,
                        };
                        log::info!("[midi] interface {}: {path}", if matches!(w, Wake::Removed(_)) { "removed" } else { "arrived" });
                        let hit = watch.notice(&w, &open.iter().map(|c| c.identity.path.as_str()).collect::<Vec<_>>());
                        for &i in hit.iter().rev() {
                            disconnect(&core, open.remove(i), &mut gone);
                        }
                    }
                    break;
                }
            }
        }
    }
    // No callback runs past this, so none can wake a thread that is gone.
    drop(registration);
    for c in open {
        c.generation.invalidate();
        core.port_gone(c.conn);
        c.input.close();
    }
    core.set_ports(Vec::new());
    core.tick(Instant::now());
}

/// The present input ports in the system's order, as `(id, name, port)` (empty when midir cannot
/// start). midir skips a port whose path the driver will not give.
fn list() -> Vec<(String, String, midir::MidiInputPort)> {
    let input = match midir::MidiInput::new("BleepLoop") {
        Ok(input) => input,
        Err(e) => {
            log::warn!("[midi] no MIDI input: {e}");
            return Vec::new();
        }
    };
    input
        .ports()
        .into_iter()
        .filter_map(|port| {
            let name = input.port_name(&port).ok()?;
            Some((port.id(), name, port))
        })
        .collect()
}

/// Open `port` in `generation`'s current generation; its callback stamps each message's arrival before
/// anything else runs, and drops it once the generation moved on.
fn connect(core: &Arc<Core>, port: &midir::MidiInputPort, conn: u32, generation: &Generation) -> Result<midir::MidiInputConnection<()>, String> {
    let mut input = midir::MidiInput::new("BleepLoop").map_err(|e| e.to_string())?;
    // SysEx, clock and active sensing never reach the play path (`parse`): let midir drop them first.
    input.ignore(midir::Ignore::All);
    let core = core.clone();
    let generation = generation.clone();
    let ticket = generation.current();
    input
        .connect(
            port,
            "BleepLoop in",
            move |_, bytes, _| {
                let at = Instant::now();
                if generation.admits(ticket) {
                    core.message(conn, at, bytes);
                }
            },
            (),
        )
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use Resolution::{Port, Unresolved as Un};
    use Unresolved::*;

    fn port(path: &str, index: u32, name: &str) -> PortIdentity {
        PortIdentity { path: path.into(), index, name: name.into() }
    }

    // midi.ts attachInputs (hot-plug): a port that arrives opens, a port that vanished closes, and one
    // that stays is left alone, whatever the order the system lists them in.
    #[test]
    fn a_poll_opens_arrivals_and_closes_departures_only() {
        assert_eq!(diff(&[], &["a", "b"]), Diff { open: vec![0, 1], close: vec![] });
        assert_eq!(diff(&["a", "b"], &["b", "a"]), Diff { open: vec![], close: vec![] });
        assert_eq!(diff(&["a", "b", "c"], &["c", "d", "a"]), Diff { open: vec![1], close: vec![1] });
        assert_eq!(diff(&["a"], &[]), Diff { open: vec![], close: vec![0] });
    }

    // Two ports of one multi-port device share WinMM's interface path: each keeps its own connection
    // from poll to poll, so neither's messages reach the other's owner or get dropped.
    #[test]
    fn ports_that_share_a_path_keep_their_own_connections_across_polls() {
        let present = identities(&[("usb#pedal", "Pedal"), ("usb#pedal", "MIDIIN2 (Pedal)"), ("usb#keys", "Keys")]);
        let present: Vec<&PortIdentity> = present.iter().collect();
        let mut next = 0;
        let d = diff(&[], &present);
        assert_eq!(d, Diff { open: vec![0, 1, 2], close: vec![] });
        let (first, fresh) = conns(&[], &present, &d.open, &mut next);
        assert_eq!(first, [Some(1), Some(2), Some(3)]);
        let open: Vec<(&PortIdentity, u32)> = fresh.iter().map(|&(i, conn)| (present[i], conn)).collect();
        let opened: Vec<&PortIdentity> = open.iter().map(|(p, _)| *p).collect();
        for listed in [present.clone(), vec![present[1], present[2], present[0]]] {
            let d = diff(&opened, &listed);
            assert_eq!(d, Diff { open: vec![], close: vec![] });
            let (again, fresh) = conns(&open, &listed, &d.open, &mut next);
            let expect = if listed == present { [Some(1), Some(2), Some(3)] } else { [Some(2), Some(3), Some(1)] };
            assert_eq!(again, expect);
            assert!(fresh.is_empty());
        }
        // One of them unplugged: only it closes.
        assert_eq!(diff(&opened, &[present[0], present[2]]), Diff { open: vec![], close: vec![1] });
    }

    // Hole 2, poll backstop: a port listed again under a new path or position is a new port, so its
    // old connection closes and a fresh one opens, even with the same name.
    #[test]
    fn a_poll_compares_whole_identities() {
        let before = [port("usb#1", 0, "Pedal")];
        assert_eq!(diff(&before, &[port("usb#2", 0, "Pedal")]), Diff { open: vec![0], close: vec![0] });
        assert_eq!(diff(&before, &[port("usb#1", 1, "Pedal")]), Diff { open: vec![0], close: vec![0] });
        assert_eq!(diff(&before, &[port("usb#1", 0, "Pedal")]), Diff { open: vec![], close: vec![] });
    }

    // Index assignment: the position among the ports sharing a path (case-insensitive) and a name, in
    // WinMM's order; two identical controllers have two paths, so both are index 0; the two ports of a
    // multi-port device have two names, so both are index 0; a path-less port counts among the
    // path-less ports with its name.
    #[test]
    fn the_index_counts_the_ports_sharing_a_path_and_a_name() {
        let ids = identities(&[("USB#a", "X"), ("usb#b", "X"), ("usb#A", "MIDIIN2 (X)"), ("usb#a", "X"), ("", "Y"), ("", "Z"), ("", "Y")]);
        let got: Vec<(u32, bool)> = ids.iter().map(|p| (p.index, p.weak())).collect();
        assert_eq!(got, [(0, false), (0, false), (0, false), (1, false), (0, true), (0, true), (1, true)]);
        assert_eq!(ids[2].id(), "winmm:0:usb#a:MIDIIN2 (X)");
        assert_eq!(ids[3].id(), "winmm:1:usb#a:X");
        assert_eq!(ids[6].id(), "winmm-weak:1:Y");
        assert!(ids.iter().all(|p| !p.id().starts_with("input-")));
    }

    // The resolution table (decisions 8 and 9, the seat's rule): one row per case.
    #[test]
    fn resolution_table() {
        let a = port(r"\\?\usb#a", 0, "Pedal");
        let b = port(r"\\?\usb#b", 0, "Pedal");
        let c = port(r"\\?\usb#c", 0, "Pedal");
        let (ida, idb, idc) = (a.id(), b.id(), c.id());
        let keys = port(r"\\?\usb#k", 0, "Keys");
        let multi = identities(&[(r"\\?\usb#m", "MIDI X"), (r"\\?\usb#m", "MIDIIN2 (MIDI X)")]);
        let weak = port("", 0, "Old box");
        let (idk, idw, m0, m1) = (keys.id(), weak.id(), multi[0].id(), multi[1].id());

        struct Row<'a> {
            case: &'a str,
            stored: Vec<(&'a str, &'a str)>,
            present: Vec<PortIdentity>,
            expect: Vec<Resolution>,
        }
        let mut rows = vec![
            Row {
                case: "two identical controllers, both present: each by its own id",
                stored: vec![(&ida, "Pedal"), (&idb, "Pedal")],
                present: vec![b.clone(), a.clone()],
                expect: vec![Port { port: 1, reanchor: false }, Port { port: 0, reanchor: false }],
            },
            Row {
                case: "two identical controllers, A unplugged, B present by its id: A never takes B",
                stored: vec![(&ida, "Pedal"), (&idb, "Pedal")],
                present: vec![b.clone()],
                expect: vec![Un(NoPort), Port { port: 0, reanchor: false }],
            },
            Row {
                case: "two identical controllers, A present, B moved to another socket: B follows the one unclaimed Pedal",
                stored: vec![(&ida, "Pedal"), (&idb, "Pedal")],
                present: vec![a.clone(), c.clone()],
                expect: vec![Port { port: 0, reanchor: false }, Port { port: 1, reanchor: true }],
            },
            Row {
                case: "two identical controllers both absent, one Pedal present: neither re-anchors",
                stored: vec![(&ida, "Pedal"), (&idb, "Pedal")],
                present: vec![c.clone()],
                expect: vec![Un(SeveralAbsent), Un(SeveralAbsent)],
            },
            Row {
                case: "a pedal moved to another USB socket: new path, same unique name",
                stored: vec![(&ida, "Pedal"), (&ida, "Pedal")],
                present: vec![keys.clone(), c.clone()],
                expect: vec![Port { port: 1, reanchor: true }; 2],
            },
            Row {
                case: "an absent id whose name two unclaimed ports carry",
                stored: vec![(&ida, "Pedal")],
                present: vec![b.clone(), c.clone()],
                expect: vec![Un(SeveralPorts)],
            },
            Row {
                case: "a multi-port device: each port by its own id",
                stored: vec![(&m1, "MIDIIN2 (MIDI X)"), (&m0, "MIDI X")],
                present: multi.to_vec(),
                expect: vec![Port { port: 1, reanchor: false }, Port { port: 0, reanchor: false }],
            },
            Row {
                case: "a legacy Web MIDI id: by its name, re-anchored",
                stored: vec![("input-2", "Pedal"), ("input-2", "Pedal")],
                present: vec![keys.clone(), a.clone()],
                expect: vec![Port { port: 1, reanchor: true }; 2],
            },
            Row {
                case: "a legacy id whose name's one port another binding holds by id: unresolved",
                stored: vec![(&ida, "Pedal"), ("input-0", "Pedal")],
                present: vec![a.clone()],
                expect: vec![Port { port: 0, reanchor: false }, Un(NoPort)],
            },
            Row {
                case: "a legacy id whose name two present ports carry, one claimed by id: unresolved (review fix)",
                stored: vec![(&ida, "Pedal"), ("input-0", "Pedal")],
                present: vec![a.clone(), b.clone()],
                expect: vec![Port { port: 0, reanchor: false }, Un(SeveralPorts)],
            },
            Row {
                case: "two legacy ids with one name: neither",
                stored: vec![("input-0", "Pedal"), ("input-1", "Pedal")],
                present: vec![a.clone()],
                expect: vec![Un(SeveralAbsent), Un(SeveralAbsent)],
            },
            Row {
                case: "a name collision with one absent id: a present Keys and an absent Keys from elsewhere",
                stored: vec![(&idk, "Keys"), ("winmm:0:\\\\?\\usb#gone", "Keys")],
                present: vec![keys.clone()],
                expect: vec![Port { port: 0, reanchor: false }, Un(NoPort)],
            },
            Row {
                case: "a weak identity never matches by id, only by its unique name",
                stored: vec![(&idw, "Old box")],
                present: vec![weak.clone()],
                expect: vec![Port { port: 0, reanchor: false }],
            },
            Row {
                case: "two weak ports with one name: neither",
                stored: vec![(&idw, "Old box")],
                present: vec![weak.clone(), port("", 1, "Old box")],
                expect: vec![Un(SeveralPorts)],
            },
            Row {
                case: "nothing present",
                stored: vec![(&idc, "Pedal"), ("input-0", "Keys")],
                present: vec![],
                expect: vec![Un(NoPort), Un(NoPort)],
            },
        ];
        // The multi-port device moved: both ports re-anchor by name.
        let moved = identities(&[(r"\\?\usb#m2", "MIDI X"), (r"\\?\usb#m2", "MIDIIN2 (MIDI X)")]);
        rows.push(Row {
            case: "a multi-port device moved to another socket",
            stored: vec![(&m0, "MIDI X"), (&m1, "MIDIIN2 (MIDI X)")],
            present: moved.to_vec(),
            expect: vec![Port { port: 0, reanchor: true }, Port { port: 1, reanchor: true }],
        });
        // The same device after a restart, WinMM listing its ports the other way round: each keeps its
        // own exact identity, so neither fires the other's bindings.
        rows.push(Row {
            case: "a multi-port device enumerated in swapped order",
            stored: vec![(&m0, "MIDI X"), (&m1, "MIDIIN2 (MIDI X)")],
            present: identities(&[(r"\\?\usb#m", "MIDIIN2 (MIDI X)"), (r"\\?\usb#m", "MIDI X")]),
            expect: vec![Port { port: 1, reanchor: false }, Port { port: 0, reanchor: false }],
        });
        for row in rows {
            assert_eq!(resolve(&row.stored, &row.present), row.expect, "{}", row.case);
        }
    }
}
