//! OWNS: the input ports: one midir connection per present port, and hot-plug by polling the port list
//! every [`POLL`] and diffing it against the open connections (midir has no port-change notification on
//! WinMM). The diff and the port keys are pure functions; the poller that applies them is the only code
//! here that touches midir, and only real hardware runs it.
//!
//! A port is followed by its [`Ident`], midir's id with the port's name: two ports with one name stay
//! apart, and so do the ports of one multi-port device, which share one id (WinMM's device interface
//! path). Bindings name a port by [`PortKey`], recomputed on every poll. WinMM input ports are exclusive: a
//! port another program (or the web app's Web MIDI) holds fails to open, and is retried every poll.
//!
//! Allocation per message: midir's WinMM handler copies each message into a `Vec` it clears and
//! reuses, so a short message allocates only while that buffer first grows (SysEx, which could grow it
//! again, is ignored here). This side allocates a note's owner list on each note-on, the sweep list on a
//! pedal-up or a release, and a UI event on a learn; the sink (`EngineHost::send`) pushes into the
//! engine's ring without allocating. None of these threads is an audio thread.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::bindings::PortKey;
use super::{Core, PortEntry};

/// How often the port list is read.
pub(crate) const POLL: Duration = Duration::from_secs(1);

/// Each name's occurrence among the ports listed before it (0 for the first port with that name).
pub(crate) fn port_keys(names: &[String]) -> Vec<PortKey> {
    names
        .iter()
        .enumerate()
        .map(|(i, name)| PortKey { name: name.clone(), occurrence: names[..i].iter().filter(|n| *n == name).count() as u32 })
        .collect()
}

/// How the poller follows a port from poll to poll: (midir's id, the port's name). WinMM's id is the
/// device interface path, which the ports of one multi-port USB device share; midir itself finds a
/// port again by both (`current_port_number`).
pub(crate) type Ident<'a> = (&'a str, &'a str);

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
pub(crate) fn conns(open: &[(Ident, u32)], present: &[Ident], opening: &[usize], next_conn: &mut u32) -> (Vec<Option<u32>>, Vec<(usize, u32)>) {
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

/// One open connection, owned by the poller.
struct Connection {
    id: String,
    name: String,
    conn: u32,
    input: midir::MidiInputConnection<()>,
}

impl Connection {
    fn ident(&self) -> Ident<'_> {
        (&self.id, &self.name)
    }
}

/// The poller thread: list, diff, close what went away (its notes released first), open what arrived,
/// publish the table, until `stop` hangs up. On the way out every port is released and closed.
pub(crate) fn run(core: Arc<Core>, stop: Receiver<()>) {
    let mut open: Vec<Connection> = Vec::new();
    let mut next_conn: u32 = 0;
    let mut failed: Vec<(String, String)> = Vec::new();
    loop {
        // `id()` allocates; the poll is not a hot path.
        let listed = list();
        let present: Vec<Ident> = listed.iter().map(|(p, _)| p.ident()).collect();
        let d = diff(&open.iter().map(Connection::ident).collect::<Vec<_>>(), &present);

        let mut gone = Vec::new();
        for &i in d.close.iter().rev() {
            let c = open.remove(i);
            // Release first: a message still in flight on the closing port then finds no owner.
            if let Some(name) = core.port_gone(c.conn) {
                log::error!("[midi] input disconnected: {name}");
                gone.push(name);
            }
            c.input.close();
        }

        let names: Vec<String> = listed.iter().map(|(p, _)| p.name.clone()).collect();
        let open_conns: Vec<(Ident, u32)> = open.iter().map(|c| (c.ident(), c.conn)).collect();
        let (port_conns, fresh) = conns(&open_conns, &present, &d.open, &mut next_conn);
        let entries: Vec<PortEntry> = port_keys(&names).into_iter().zip(port_conns).map(|(key, conn)| PortEntry { key, conn }).collect();
        // Registered before it connects, so the first message finds its port.
        core.set_ports(entries);

        for (i, conn) in fresh {
            let (p, port) = &listed[i];
            let ident = (p.id.clone(), p.name.clone());
            match connect(&core, port, conn) {
                Ok(input) => {
                    failed.retain(|f| *f != ident);
                    open.push(Connection { id: ident.0, name: ident.1, conn, input });
                }
                Err(e) => {
                    core.port_gone(conn);
                    if !failed.contains(&ident) {
                        log::warn!("[midi] could not open input {}: {e}", p.name);
                        failed.push(ident);
                    }
                }
            }
        }
        core.publish_ports(gone);

        match stop.recv_timeout(POLL) {
            Err(RecvTimeoutError::Timeout) => {}
            _ => break,
        }
    }
    for c in open {
        core.port_gone(c.conn);
        c.input.close();
    }
    core.set_ports(Vec::new());
}

struct Listed {
    id: String,
    name: String,
}

impl Listed {
    fn ident(&self) -> Ident<'_> {
        (&self.id, &self.name)
    }
}

/// The present input ports in the system's order (empty when midir cannot start).
fn list() -> Vec<(Listed, midir::MidiInputPort)> {
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
            Some((Listed { id: port.id(), name }, port))
        })
        .collect()
}

/// Open `port`; its callback stamps each message's arrival before anything else runs.
fn connect(core: &Arc<Core>, port: &midir::MidiInputPort, conn: u32) -> Result<midir::MidiInputConnection<()>, String> {
    let mut input = midir::MidiInput::new("BleepLoop").map_err(|e| e.to_string())?;
    // SysEx, clock and active sensing never reach the play path (`parse`): let midir drop them first.
    input.ignore(midir::Ignore::All);
    let core = core.clone();
    input
        .connect(port, "BleepLoop in", move |_, bytes, _| core.message(conn, Instant::now(), bytes), ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn ports_that_share_an_id_keep_their_own_connections_across_polls() {
        let present: [Ident; 3] = [("usb#pedal", "Pedal"), ("usb#pedal", "MIDIIN2 (Pedal)"), ("usb#keys", "Keys")];
        let mut next = 0;
        let d = diff(&[], &present);
        assert_eq!(d, Diff { open: vec![0, 1, 2], close: vec![] });
        let (first, fresh) = conns(&[], &present, &d.open, &mut next);
        assert_eq!(first, [Some(1), Some(2), Some(3)]);
        let open: Vec<(Ident, u32)> = fresh.iter().map(|&(i, conn)| (present[i], conn)).collect();
        // The next polls: nothing opens or closes, and every port keeps its own connection, whatever the
        // order the system lists them in.
        let opened: Vec<Ident> = open.iter().map(|(p, _)| *p).collect();
        for listed in [present, [present[1], present[2], present[0]]] {
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

    // The native port key (midi-actions.ts keyed by Web MIDI id): the n-th port with a name.
    #[test]
    fn ports_with_one_name_are_told_apart_by_occurrence() {
        let names: Vec<String> = ["Pedal", "Keys", "Pedal", "Pedal"].map(String::from).into();
        let keys: Vec<(String, u32)> = port_keys(&names).into_iter().map(|k| (k.name, k.occurrence)).collect();
        assert_eq!(keys, [("Pedal".into(), 0), ("Keys".into(), 0), ("Pedal".into(), 1), ("Pedal".into(), 2)]);
    }
}
