//! OWNS: raw MIDI bytes to the messages the play path and MIDI learn read, ported from
//! `src/audio/midi.ts` (`parseMidiMessage`). Only 3-byte channel messages count: note on and off,
//! control change and pitch bend. Everything else falls out here, as it does on the web: realtime and
//! other short messages (clock, program change, channel pressure; the audio clock is the only tempo
//! authority, invariant 1), poly aftertouch, and system messages including SysEx.

/// A channel message, channel 0..15.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Message {
    /// Velocity 1..127 (a note-on at velocity 0 is a [`Message::NoteOff`]).
    NoteOn { channel: u8, note: u8, velocity: u8 },
    NoteOff { channel: u8, note: u8 },
    Cc { channel: u8, controller: u8, value: u8 },
    /// The 14-bit wheel position, centre 8192.
    PitchBend { channel: u8, value: u16 },
}

impl Message {
    pub fn channel(&self) -> u8 {
        match *self {
            Message::NoteOn { channel, .. }
            | Message::NoteOff { channel, .. }
            | Message::Cc { channel, .. }
            | Message::PitchBend { channel, .. } => channel,
        }
    }
}

/// The message in `bytes` (one MIDI message, as midir and Web MIDI deliver them), or `None` for one
/// the play path ignores.
pub fn parse(bytes: &[u8]) -> Option<Message> {
    let [status, data1, data2, ..] = *bytes else { return None };
    // A data byte with its high bit set is malformed; Chromium's Web MIDI drops such a message before
    // the page sees it.
    if data1 >= 0x80 || data2 >= 0x80 {
        return None;
    }
    let channel = status & 0x0f;
    match status & 0xf0 {
        0x90 if data2 > 0 => Some(Message::NoteOn { channel, note: data1, velocity: data2 }),
        0x90 | 0x80 => Some(Message::NoteOff { channel, note: data1 }),
        0xb0 => Some(Message::Cc { channel, controller: data1, value: data2 }),
        // 14-bit little-endian: LSB then MSB.
        0xe0 => Some(Message::PitchBend { channel, value: (u16::from(data2) << 7) | u16::from(data1) }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // midi.ts parseMidiMessage: "Note-on; velocity 0 is treated as note-off per MIDI spec", and 0x80 is
    // a note-off whatever its velocity.
    #[test]
    fn a_note_on_at_velocity_zero_is_a_note_off() {
        assert_eq!(parse(&[0x93, 60, 100]), Some(Message::NoteOn { channel: 3, note: 60, velocity: 100 }));
        assert_eq!(parse(&[0x93, 60, 0]), Some(Message::NoteOff { channel: 3, note: 60 }));
        assert_eq!(parse(&[0x8f, 61, 64]), Some(Message::NoteOff { channel: 15, note: 61 }));
    }

    // midi.ts parseMidiMessage: the status byte's low nibble is the channel; CC keeps controller and value.
    #[test]
    fn a_control_change_keeps_its_channel_controller_and_value() {
        assert_eq!(parse(&[0xb5, 64, 127]), Some(Message::Cc { channel: 5, controller: 64, value: 127 }));
        assert_eq!(parse(&[0xb0, 1, 0]).map(|m| m.channel()), Some(0));
    }

    // midi.ts parseMidiMessage: "Pitch bend: 14-bit little-endian (LSB then MSB), center 8192".
    #[test]
    fn pitch_bend_is_fourteen_bits_lsb_first() {
        assert_eq!(parse(&[0xe0, 0x00, 0x40]), Some(Message::PitchBend { channel: 0, value: 8192 }));
        assert_eq!(parse(&[0xe1, 0x7f, 0x7f]), Some(Message::PitchBend { channel: 1, value: 16383 }));
        assert_eq!(parse(&[0xe2, 0x01, 0x00]), Some(Message::PitchBend { channel: 2, value: 1 }));
    }

    // midi.ts parseMidiMessage: "if (data.length < 3) return" drops realtime (0xF8 clock, start/stop,
    // sensing) and the 2-byte program change and channel pressure; a 3-byte message of any other type
    // (poly aftertouch, song position, SysEx) matches no branch.
    #[test]
    fn short_system_and_unhandled_messages_are_ignored() {
        for bytes in [
            &[0xf8][..],
            &[0xfa],
            &[0xfe],
            &[0xc0, 5],
            &[0xd0, 90],
            &[0xa0, 60, 40],
            &[0xf2, 0, 0],
            &[0xf0, 0x7e, 0x7f, 0x06, 0x01, 0xf7],
            &[],
        ] {
            assert_eq!(parse(bytes), None, "{bytes:02x?}");
        }
    }

    // Chromium's Web MIDI drops a message with a data byte ≥ 0x80 before midi.ts sees it.
    #[test]
    fn a_malformed_data_byte_drops_the_message() {
        assert_eq!(parse(&[0x90, 0x80, 100]), None);
        assert_eq!(parse(&[0xb0, 7, 0xff]), None);
    }
}
