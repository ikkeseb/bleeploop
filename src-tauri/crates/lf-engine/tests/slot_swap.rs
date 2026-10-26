//! The hot plugin swap (`src/slots.rs`, `SlotPort::replace`) driven through the engine. So far only its
//! guard on everything outside a swap: the slots render bit for bit as they did before it.

mod common;

use common::refs::sha256_f32;
use common::units::{Fake, Probe};
use common::{Delay, Opts, Rig};
use lf_engine::grid::Frame;
use lf_engine::{Command, NoteTarget};

/// Two triangle waves, exact in f32: slot 0's input and slot 1's.
fn input_a(f: Frame) -> f32 {
    (f.rem_euclid(200) as f32 - 100.0) / 200.0
}

fn input_b(f: Frame) -> f32 {
    f.rem_euclid(77) as f32 / 154.0
}

/// Installs, removals (one through each crossfade, two serviced at once with no device), notes, and live
/// toggles inside an install fade and a removal fade, on both slots, with no gain change (the glide's
/// coefficient calls the platform's `exp`; everything else here is plain arithmetic). The hash of the
/// monitor, the bus and the record tap.
fn plain_session(block: usize) -> String {
    let (a, b, c, d) = (Probe::new(), Probe::new(), Probe::new(), Probe::new());
    let mut rig = Rig::with(Opts { block, ..Default::default() });
    rig.set_inputs(input_a, input_b);
    let t = rig.frame;
    let script = [
        (0, Command::SelectInstrument(NoteTarget::Slot(1))),
        (1_100, Command::SetSlotLive(0, false)),
        (1_300, Command::SetSlotLive(0, true)),
        (3_001, Command::NoteOn(60, 1.0)),
        (7_777, Command::NoteOn(62, 0.5)),
        (9_000, Command::NoteOff(60)),
        (12_000, Command::SetSlotLive(1, true)),
        (20_200, Command::SetSlotLive(1, false)),
    ];
    for (at, command) in script {
        rig.send_at(t + at, command);
    }
    rig.keep_output();
    rig.advance_to(t + 1_000);
    rig.install(0, Box::new(Delay::new(57)));
    rig.install(1, Fake::instrument(0.3, 33, &a));
    rig.advance_to(t + 15_000);
    rig.remove(0);
    rig.advance_to(t + 20_000);
    rig.remove(1);
    rig.advance_to(t + 25_000);
    rig.install(0, Fake::effect(0.1, 0.5, &b));
    rig.advance_to(t + 30_000);
    rig.remove(0);
    rig.advance_to(t + 32_000);
    rig.install(0, Fake::effect(0.2, 0.25, &c));
    rig.install(1, Fake::effect(-0.1, 1.0, &d));
    rig.engine.service_slots_idle();
    rig.advance_to(t + 36_000);
    rig.remove(0);
    rig.remove(1);
    rig.engine.service_slots_idle();
    rig.advance_to(t + 40_000);
    let back = [0, 0, 0, 1, 1].into_iter().filter_map(|s| rig.returned(s)).count();
    assert_eq!(back, 5, "every unit came back");
    sha256_f32(&[&rig.monitor, &rig.bus, &rig.record])
}

/// [`plain_session`] at each block size, hashed on the engine before the hot swap (304842ba, Windows).
/// The record tap follows a latent effect's live toggle from the next block start, so each block size
/// has its own hash.
const BEFORE: [(usize, &str); 4] = [
    (128, "15d319118842bfc8d5d03ba9469f919770921a2a3716cfc90dbf7584998bb86b"),
    (1, "18ff445dde631b3c37d97e2c9592203634a38d88f3a2b46f80165525a0fe5b7b"),
    (32, "e02451c4cdf6721f5fa8ab9aa1ed6958e68b5949597927270ad72bfa24f6f1de"),
    (480, "64f9ed725629df78434c7e1802bcaddd336a8725d487af29cf3f76fa4b01e240"),
];

#[test]
fn outside_a_swap_the_slots_render_as_before_it() {
    for (block, hash) in BEFORE {
        assert_eq!(plain_session(block), hash, "block {block}");
    }
}
