//! Shared cross-command state + IPC-mirrored types for the `host` module.

use serde::{Deserialize, Serialize};

/// The actual cached ASIO driver, distinct from the WASAPI endpoint selectors.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AsioDeviceInfo {
    pub name: String,
    pub input_channels: u32,
    pub output_channels: u32,
}

/// Mirrors `PluginDescriptor` in `src/platform/host.ts` (camelCase over IPC).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PluginDescriptor {
    pub id: String,
    pub name: String,
    /// "clap" | "vst3" — VST3 is P10; P9 only ever emits "clap".
    pub format: String,
    pub path: String,
    /// P11 output-gain: `Some(true)` = audio EFFECT (amp-sim/FX → near-unity default), `Some(false)`
    /// = INSTRUMENT (synth → conservative default), `None` = unclassified (JS falls back to the input
    /// bus count). Read at scan from CLAP `features()` / VST3 `subCategories` — the robust, portable
    /// discriminator (input-bus presence alone misclassifies synths that declare an audio-in bus).
    pub is_effect: Option<bool>,
}

/// Mirrors `PluginInfo` in `src/platform/host.ts`.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PluginInfo {
    pub slot: u8,
    pub descriptor: PluginDescriptor,
}

/// Native slot lifecycle. `Loading` is a real reservation: a WebView reload can cancel it before
/// the foreign plugin finishes setup, and the old command can then only tear its result down — it
/// can never park invisibly behind the new document's synth state.
#[cfg(windows)]
pub(crate) enum SlotState {
    Empty,
    Loading {
        load_gen: u32,
        frontend_epoch: u32,
        running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    },
    Loaded(super::clap::SlotHandle),
}

#[cfg(windows)]
impl SlotState {
    pub(crate) fn loaded(&self) -> Option<&super::clap::SlotHandle> {
        match self {
            Self::Loaded(handle) => Some(handle),
            Self::Empty | Self::Loading { .. } => None,
        }
    }

    pub(crate) fn owns_load(&self, load_gen: u32, frontend_epoch: u32) -> bool {
        matches!(
            self,
            Self::Loading {
                load_gen: current,
                frontend_epoch: current_epoch,
                ..
            } if *current == load_gen && *current_epoch == frontend_epoch
        )
    }
}

/// One plugin parameter's metadata (P9.5). Mirrors `PluginParamDesc` in `src/platform/host.ts`.
/// `id` is the stable CLAP param id (what `setParameter` takes); the rest drives a future param UI
/// and lets a caller pick a real id to set (host-side `params` ext: `count` + `get_info`).
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ParamDesc {
    pub id: u32,
    pub name: String,
    pub min_value: f64,
    pub max_value: f64,
    pub default_value: f64,
    /// The LIVE value at enumeration time (same units as min/max: plugin units for CLAP, normalised
    /// 0..1 for VST3) — what a freshly mounted drawer must show, not the default.
    pub value: f64,
}

/// P11.0: one enumerated native audio-input device. Mirrors the `AudioInputDevice` boundary type in
/// `src/platform/host.ts` (camelCase fields surface to JS as `{id, name, channels}`).
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AudioInputDevice {
    pub id: String,
    pub name: String,
    pub channels: u32,
}

/// P11.3: a native (cpal/WASAPI-shared) OUTPUT device for the monitor-device picker.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AudioOutputDevice {
    pub id: String,
    pub name: String,
    pub channels: u32,
}

/// Shared host state managed by Tauri (`.manage()` in `lib.rs`). Holds only `Send + Sync` things:
/// the JS-provided sample rate (f64 bits) and the per-slot control handles. The `!Send`
/// `PluginInstance` is NOT here — it lives pinned on its owner thread (see `clap`).
pub struct PluginHostState {
    pub(crate) sample_rate: std::sync::atomic::AtomicU64,
    /// Bumped by every `host_init` (one call per WebView document). Loads must present the current
    /// epoch, so an IPC request from a document being replaced cannot reserve or park a slot later.
    #[cfg(windows)]
    pub(crate) frontend_epoch: std::sync::atomic::AtomicU32,
    #[cfg(windows)]
    pub(crate) slots: std::sync::Mutex<[SlotState; 2]>,
}

impl Default for PluginHostState {
    fn default() -> Self {
        Self {
            sample_rate: std::sync::atomic::AtomicU64::new(0),
            #[cfg(windows)]
            frontend_epoch: std::sync::atomic::AtomicU32::new(0),
            #[cfg(windows)]
            slots: std::sync::Mutex::new([SlotState::Empty, SlotState::Empty]),
        }
    }
}
