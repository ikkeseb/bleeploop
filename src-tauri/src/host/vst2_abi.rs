//! The VST2 binary interface as a 64-bit Windows plugin lays it out: the `AEffect` a plugin's entry
//! returns, the structures that cross its dispatcher and its host callback, and the opcode numbers
//! this host sends and answers. Written from the layout itself (field order, widths and the
//! platform's natural alignment), not from any SDK header; the tests below state every offset as a
//! literal number, so a field that moves fails the build's tests instead of a plugin.
//!
//! All of it is `#[repr(C)]` without packing. Dispatcher and callback values and returns are
//! pointer-sized (`isize`), opcodes and indices 32-bit, and a function pointer the plugin may leave
//! out is an `Option`. x64 Windows has one calling convention, so `extern "C"` is the plugin's.

use std::ffi::c_void;

/// `AEffect::magic`: the four characters `VstP`.
pub(crate) const EFFECT_MAGIC: i32 = 0x5673_7450;

/// The plugin's dispatcher: `(effect, opcode, index, value, ptr, opt)`, an `EFF_*` opcode.
pub(crate) type DispatcherFn =
    unsafe extern "C" fn(*mut AEffect, i32, i32, isize, *mut c_void, f32) -> isize;
/// The host's callback, handed to the plugin's entry: the same shape, an `AUDIO_MASTER_*` opcode.
pub(crate) type HostCallback =
    unsafe extern "C" fn(*mut AEffect, i32, i32, isize, *mut c_void, f32) -> isize;
/// `process` (adds into its outputs) and `processReplacing` (overwrites them):
/// `(effect, inputs, outputs, frames)`, one pointer per channel.
pub(crate) type ProcessFn = unsafe extern "C" fn(*mut AEffect, *mut *mut f32, *mut *mut f32, i32);
pub(crate) type ProcessDoubleFn = unsafe extern "C" fn(*mut AEffect, *mut *mut f64, *mut *mut f64, i32);
pub(crate) type SetParameterFn = unsafe extern "C" fn(*mut AEffect, i32, f32);
pub(crate) type GetParameterFn = unsafe extern "C" fn(*mut AEffect, i32) -> f32;
/// A plugin DLL's entry (`VSTPluginMain`, or `main` in older ones): one new effect per call.
pub(crate) type EntryFn = unsafe extern "C" fn(HostCallback) -> *mut AEffect;

/// One plugin instance, owned by the plugin and freed by its `EFF_CLOSE`.
#[repr(C)]
pub(crate) struct AEffect {
    pub(crate) magic: i32,
    pub(crate) dispatcher: Option<DispatcherFn>,
    /// The accumulating process call: the only one a VST 1.0-era plugin has.
    pub(crate) process: Option<ProcessFn>,
    pub(crate) set_parameter: Option<SetParameterFn>,
    pub(crate) get_parameter: Option<GetParameterFn>,
    pub(crate) num_programs: i32,
    pub(crate) num_params: i32,
    pub(crate) num_inputs: i32,
    pub(crate) num_outputs: i32,
    pub(crate) flags: i32,
    /// Reserved for the host: this one keeps its `HostContext` pointer here (`vst2.rs`).
    pub(crate) resvd1: isize,
    pub(crate) resvd2: isize,
    /// The plugin's latency in frames.
    pub(crate) initial_delay: i32,
    pub(crate) real_qualities: i32,
    pub(crate) off_qualities: i32,
    pub(crate) io_ratio: f32,
    pub(crate) object: *mut c_void,
    pub(crate) user: *mut c_void,
    pub(crate) unique_id: i32,
    pub(crate) version: i32,
    pub(crate) process_replacing: Option<ProcessFn>,
    pub(crate) process_double_replacing: Option<ProcessDoubleFn>,
    pub(crate) future: [u8; 56],
}

/// `VstEvents` with room for `N` event pointers after its header. A plugin declares two; the host
/// passes its own fixed capacity and says how many are filled in `num_events`.
#[repr(C)]
pub(crate) struct VstEvents<const N: usize = 2> {
    pub(crate) num_events: i32,
    pub(crate) reserved: isize,
    pub(crate) events: [*mut VstMidiEvent; N],
}

/// The one event kind this host sends (`event_type` = `VST_MIDI_TYPE`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct VstMidiEvent {
    pub(crate) event_type: i32,
    /// The size of this structure.
    pub(crate) byte_size: i32,
    /// Frames into the block the event belongs to.
    pub(crate) delta_frames: i32,
    pub(crate) flags: i32,
    pub(crate) note_length: i32,
    pub(crate) note_offset: i32,
    pub(crate) midi_data: [u8; 4],
    pub(crate) detune: i8,
    pub(crate) note_off_velocity: i8,
    pub(crate) reserved1: i8,
    pub(crate) reserved2: i8,
}

/// What `AUDIO_MASTER_GET_TIME` answers with a pointer to. `flags` says which fields are valid.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct VstTimeInfo {
    pub(crate) sample_pos: f64,
    pub(crate) sample_rate: f64,
    pub(crate) nano_seconds: f64,
    pub(crate) ppq_pos: f64,
    pub(crate) tempo: f64,
    pub(crate) bar_start_pos: f64,
    pub(crate) cycle_start_pos: f64,
    pub(crate) cycle_end_pos: f64,
    pub(crate) time_sig_numerator: i32,
    pub(crate) time_sig_denominator: i32,
    pub(crate) smpte_offset: i32,
    pub(crate) smpte_frame_rate: i32,
    pub(crate) samples_to_next_clock: i32,
    pub(crate) flags: i32,
}

/// An editor's rectangle (`EFF_EDIT_GET_RECT` answers a pointer to one the plugin owns).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ERect {
    pub(crate) top: i16,
    pub(crate) left: i16,
    pub(crate) bottom: i16,
    pub(crate) right: i16,
}

// Dispatcher opcodes (host → plugin).
pub(crate) const EFF_OPEN: i32 = 0;
pub(crate) const EFF_CLOSE: i32 = 1;
pub(crate) const EFF_SET_PROGRAM: i32 = 2;
pub(crate) const EFF_GET_PROGRAM: i32 = 3;
pub(crate) const EFF_GET_PARAM_LABEL: i32 = 6;
pub(crate) const EFF_GET_PARAM_DISPLAY: i32 = 7;
pub(crate) const EFF_GET_PARAM_NAME: i32 = 8;
pub(crate) const EFF_SET_SAMPLE_RATE: i32 = 10;
pub(crate) const EFF_SET_BLOCK_SIZE: i32 = 11;
pub(crate) const EFF_MAINS_CHANGED: i32 = 12;
pub(crate) const EFF_EDIT_GET_RECT: i32 = 13;
pub(crate) const EFF_EDIT_OPEN: i32 = 14;
pub(crate) const EFF_EDIT_CLOSE: i32 = 15;
pub(crate) const EFF_EDIT_IDLE: i32 = 19;
pub(crate) const EFF_GET_CHUNK: i32 = 23;
pub(crate) const EFF_SET_CHUNK: i32 = 24;
pub(crate) const EFF_PROCESS_EVENTS: i32 = 25;
pub(crate) const EFF_GET_PLUG_CATEGORY: i32 = 35;
pub(crate) const EFF_GET_EFFECT_NAME: i32 = 45;
pub(crate) const EFF_GET_VENDOR_STRING: i32 = 47;
pub(crate) const EFF_GET_PRODUCT_STRING: i32 = 48;
pub(crate) const EFF_CAN_DO: i32 = 51;
pub(crate) const EFF_IDLE: i32 = 53;
pub(crate) const EFF_SHELL_GET_NEXT_PLUGIN: i32 = 70;
pub(crate) const EFF_START_PROCESS: i32 = 71;
pub(crate) const EFF_STOP_PROCESS: i32 = 72;

// Host callback opcodes (plugin → host).
pub(crate) const AUDIO_MASTER_AUTOMATE: i32 = 0;
pub(crate) const AUDIO_MASTER_VERSION: i32 = 1;
pub(crate) const AUDIO_MASTER_CURRENT_ID: i32 = 2;
pub(crate) const AUDIO_MASTER_IDLE: i32 = 3;
pub(crate) const AUDIO_MASTER_WANT_MIDI: i32 = 6;
pub(crate) const AUDIO_MASTER_GET_TIME: i32 = 7;
pub(crate) const AUDIO_MASTER_IO_CHANGED: i32 = 13;
pub(crate) const AUDIO_MASTER_NEED_IDLE: i32 = 14;
pub(crate) const AUDIO_MASTER_SIZE_WINDOW: i32 = 15;
pub(crate) const AUDIO_MASTER_GET_SAMPLE_RATE: i32 = 16;
pub(crate) const AUDIO_MASTER_GET_BLOCK_SIZE: i32 = 17;
pub(crate) const AUDIO_MASTER_GET_CURRENT_PROCESS_LEVEL: i32 = 23;
pub(crate) const AUDIO_MASTER_GET_VENDOR_STRING: i32 = 32;
pub(crate) const AUDIO_MASTER_GET_PRODUCT_STRING: i32 = 33;
pub(crate) const AUDIO_MASTER_GET_VENDOR_VERSION: i32 = 34;
pub(crate) const AUDIO_MASTER_CAN_DO: i32 = 37;
pub(crate) const AUDIO_MASTER_UPDATE_DISPLAY: i32 = 42;
pub(crate) const AUDIO_MASTER_BEGIN_EDIT: i32 = 43;
pub(crate) const AUDIO_MASTER_END_EDIT: i32 = 44;

// `AEffect::flags` bits.
pub(crate) const EFF_FLAGS_HAS_EDITOR: i32 = 1 << 0;
pub(crate) const EFF_FLAGS_CAN_REPLACING: i32 = 1 << 4;
pub(crate) const EFF_FLAGS_PROGRAM_CHUNKS: i32 = 1 << 5;
pub(crate) const EFF_FLAGS_IS_SYNTH: i32 = 1 << 8;

/// `EFF_GET_PLUG_CATEGORY`'s answer for a shell: one file that holds several plugins.
pub(crate) const PLUG_CATEGORY_SHELL: isize = 10;

/// `VstMidiEvent::event_type`.
pub(crate) const VST_MIDI_TYPE: i32 = 1;

/// `AUDIO_MASTER_GET_CURRENT_PROCESS_LEVEL`'s answers: a GUI or other non-audio thread, and the
/// audio thread inside a process call.
pub(crate) const PROCESS_LEVEL_USER: isize = 1;
pub(crate) const PROCESS_LEVEL_REALTIME: isize = 2;

/// What `AUDIO_MASTER_VERSION` answers: VST 2.4.
pub(crate) const VST_VERSION_2_4: isize = 2400;

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    // Every expected number below is the layout stated on its own; none is derived from the types.

    #[test]
    fn the_effect_has_its_64_bit_layout() {
        assert_eq!(size_of::<AEffect>(), 192);
        assert_eq!(align_of::<AEffect>(), 8);
        assert_eq!(offset_of!(AEffect, magic), 0);
        assert_eq!(offset_of!(AEffect, dispatcher), 8);
        assert_eq!(offset_of!(AEffect, process), 16);
        assert_eq!(offset_of!(AEffect, set_parameter), 24);
        assert_eq!(offset_of!(AEffect, get_parameter), 32);
        assert_eq!(offset_of!(AEffect, num_programs), 40);
        assert_eq!(offset_of!(AEffect, num_params), 44);
        assert_eq!(offset_of!(AEffect, num_inputs), 48);
        assert_eq!(offset_of!(AEffect, num_outputs), 52);
        assert_eq!(offset_of!(AEffect, flags), 56);
        assert_eq!(offset_of!(AEffect, resvd1), 64);
        assert_eq!(offset_of!(AEffect, resvd2), 72);
        assert_eq!(offset_of!(AEffect, initial_delay), 80);
        assert_eq!(offset_of!(AEffect, real_qualities), 84);
        assert_eq!(offset_of!(AEffect, off_qualities), 88);
        assert_eq!(offset_of!(AEffect, io_ratio), 92);
        assert_eq!(offset_of!(AEffect, object), 96);
        assert_eq!(offset_of!(AEffect, user), 104);
        assert_eq!(offset_of!(AEffect, unique_id), 112);
        assert_eq!(offset_of!(AEffect, version), 116);
        assert_eq!(offset_of!(AEffect, process_replacing), 120);
        assert_eq!(offset_of!(AEffect, process_double_replacing), 128);
        assert_eq!(offset_of!(AEffect, future), 136);
        assert_eq!(EFFECT_MAGIC.to_be_bytes(), *b"VstP");
    }

    #[test]
    fn a_missing_function_pointer_is_a_null_pointer() {
        assert_eq!(size_of::<Option<DispatcherFn>>(), 8);
        assert_eq!(size_of::<Option<ProcessFn>>(), 8);
        assert_eq!(size_of::<Option<ProcessDoubleFn>>(), 8);
        assert_eq!(size_of::<Option<SetParameterFn>>(), 8);
        assert_eq!(size_of::<Option<GetParameterFn>>(), 8);
        // SAFETY: an all-zero `AEffect` is valid: integers, null pointers and `None`s.
        let zeroed: AEffect = unsafe { std::mem::zeroed() };
        assert!(zeroed.dispatcher.is_none() && zeroed.process_replacing.is_none());
    }

    #[test]
    fn the_event_list_has_its_layout_at_any_capacity() {
        assert_eq!(size_of::<VstEvents>(), 32);
        assert_eq!(align_of::<VstEvents>(), 8);
        assert_eq!(offset_of!(VstEvents, num_events), 0);
        assert_eq!(offset_of!(VstEvents, reserved), 8);
        assert_eq!(offset_of!(VstEvents, events), 16);
        // The host's own capacity only lengthens the pointer array.
        assert_eq!(offset_of!(VstEvents<256>, num_events), 0);
        assert_eq!(offset_of!(VstEvents<256>, reserved), 8);
        assert_eq!(offset_of!(VstEvents<256>, events), 16);
        assert_eq!(size_of::<VstEvents<256>>(), 2064);
    }

    #[test]
    fn the_midi_event_has_its_layout() {
        assert_eq!(size_of::<VstMidiEvent>(), 32);
        assert_eq!(align_of::<VstMidiEvent>(), 4);
        assert_eq!(offset_of!(VstMidiEvent, event_type), 0);
        assert_eq!(offset_of!(VstMidiEvent, byte_size), 4);
        assert_eq!(offset_of!(VstMidiEvent, delta_frames), 8);
        assert_eq!(offset_of!(VstMidiEvent, flags), 12);
        assert_eq!(offset_of!(VstMidiEvent, note_length), 16);
        assert_eq!(offset_of!(VstMidiEvent, note_offset), 20);
        assert_eq!(offset_of!(VstMidiEvent, midi_data), 24);
        assert_eq!(offset_of!(VstMidiEvent, detune), 28);
        assert_eq!(offset_of!(VstMidiEvent, note_off_velocity), 29);
        assert_eq!(offset_of!(VstMidiEvent, reserved1), 30);
        assert_eq!(offset_of!(VstMidiEvent, reserved2), 31);
    }

    #[test]
    fn the_time_info_has_its_layout() {
        assert_eq!(size_of::<VstTimeInfo>(), 88);
        assert_eq!(align_of::<VstTimeInfo>(), 8);
        assert_eq!(offset_of!(VstTimeInfo, sample_pos), 0);
        assert_eq!(offset_of!(VstTimeInfo, sample_rate), 8);
        assert_eq!(offset_of!(VstTimeInfo, nano_seconds), 16);
        assert_eq!(offset_of!(VstTimeInfo, ppq_pos), 24);
        assert_eq!(offset_of!(VstTimeInfo, tempo), 32);
        assert_eq!(offset_of!(VstTimeInfo, bar_start_pos), 40);
        assert_eq!(offset_of!(VstTimeInfo, cycle_start_pos), 48);
        assert_eq!(offset_of!(VstTimeInfo, cycle_end_pos), 56);
        assert_eq!(offset_of!(VstTimeInfo, time_sig_numerator), 64);
        assert_eq!(offset_of!(VstTimeInfo, time_sig_denominator), 68);
        assert_eq!(offset_of!(VstTimeInfo, smpte_offset), 72);
        assert_eq!(offset_of!(VstTimeInfo, smpte_frame_rate), 76);
        assert_eq!(offset_of!(VstTimeInfo, samples_to_next_clock), 80);
        assert_eq!(offset_of!(VstTimeInfo, flags), 84);
    }

    #[test]
    fn the_editor_rect_is_top_left_bottom_right() {
        assert_eq!(size_of::<ERect>(), 8);
        assert_eq!(align_of::<ERect>(), 2);
        assert_eq!(offset_of!(ERect, top), 0);
        assert_eq!(offset_of!(ERect, left), 2);
        assert_eq!(offset_of!(ERect, bottom), 4);
        assert_eq!(offset_of!(ERect, right), 6);
    }
}
