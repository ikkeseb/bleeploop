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
    /// The load's answer: what it did with the plugin's stored tone (`host/tone.rs`). `None`: nothing
    /// was stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tone: Option<ToneRestore>,
}

/// A session import's tone, stored (`plugin_tone_import`). Mirrors `ToneImport` in
/// `src/platform/host.ts`: the plugin it belongs to, and when the slot holds that plugin now, the
/// token of the reload that hears it (the caller passes it to that reload's `plugin_load`).
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ToneImport {
    pub reload_token: Option<u32>,
    pub name: String,
    pub format: String,
    pub path: String,
    pub id: String,
}

/// What a load did with the plugin's stored tone. Mirrors `PluginInfo.tone` in `src/platform/host.ts`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ToneRestore {
    Restored,
    /// The file was unreadable or the plugin refused it: the plugin runs at its defaults.
    Failed,
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

/// One folder the plugin scan walks, as Audio Settings lists it. Mirrors `PluginFolder` in
/// `src/platform/host.ts`.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PluginFolder {
    pub path: String,
    /// Whether the folder is there now (a missing one is skipped by the scan).
    pub exists: bool,
}

/// The scan's folders (`host/folders.rs`). Mirrors `PluginFolders` in `src/platform/host.ts`:
/// `builtin` is every root the scan walks by itself (the fixed CLAP and VST3 roots, then the
/// `CLAP_PATH`/`VST3_PATH` entries), read-only; `user` is the player's own list.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PluginFolders {
    pub builtin: Vec<PluginFolder>,
    pub user: Vec<PluginFolder>,
}

/// Shared host state managed by Tauri (`.manage()` in `lib.rs`): the WebView document epoch and the
/// plugin folder list's locks. The plugin slots themselves live in engine mode's state
/// (`engine_io::plugins`).
#[derive(Default)]
pub struct PluginHostState {
    /// Bumped by every `host_init` (one call per WebView document). Loads must present the current
    /// epoch, so an IPC request from a document being replaced cannot reserve or park a slot later.
    pub(crate) frontend_epoch: std::sync::atomic::AtomicU32,
    /// Held across each change of the plugin folder list, from its read to its rename
    /// (`host/folders.rs`); never while the folder dialog is open or a scan child runs.
    pub(crate) folders_write: std::sync::Mutex<()>,
    /// Up while the folder dialog is open: a second `plugin_folder_add` opens no second dialog.
    pub(crate) folder_dialog_open: std::sync::atomic::AtomicBool,
}

impl PluginHostState {
    /// Begin one WebView document's session: the next epoch (never 0). A load still running for the
    /// old document finds the epoch moved and unloads what it made (`engine_io::plugins`). One atomic
    /// read-modify-write, so two overlapping `host_init`s never get the same epoch.
    pub(crate) fn begin_frontend_session(&self) -> u32 {
        use std::sync::atomic::Ordering::Relaxed;
        let next = |epoch: u32| epoch.wrapping_add(1).max(1);
        let previous = self.frontend_epoch.fetch_update(Relaxed, Relaxed, |epoch| Some(next(epoch)));
        next(previous.unwrap_or_else(|epoch| epoch))
    }
}

#[cfg(test)]
mod frontend_epoch_tests {
    use super::PluginHostState;
    use std::sync::atomic::Ordering::Relaxed;

    #[test]
    fn a_session_epoch_is_never_zero_and_steps_up_across_the_wrap() {
        let state = PluginHostState::default();
        assert_eq!(state.begin_frontend_session(), 1, "the first document's epoch");
        assert_eq!(state.begin_frontend_session(), 2);
        state.frontend_epoch.store(u32::MAX - 1, Relaxed);
        assert_eq!(state.begin_frontend_session(), u32::MAX);
        assert_eq!(state.begin_frontend_session(), 1, "the wrap skips 0 (no document has epoch 0)");
        assert_eq!(state.begin_frontend_session(), 2);
    }

    #[test]
    fn overlapping_sessions_each_get_their_own_epoch() {
        let state = std::sync::Arc::new(PluginHostState::default());
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let state = state.clone();
                std::thread::spawn(move || (0..1000).map(|_| state.begin_frontend_session()).collect::<Vec<_>>())
            })
            .collect();
        let mut all = Vec::new();
        for thread in threads {
            let epochs = thread.join().unwrap();
            assert!(epochs.windows(2).all(|w| w[0] < w[1]), "each caller sees its epochs rise");
            all.extend(epochs);
        }
        all.sort_unstable();
        assert_eq!(all, (1..=8000).collect::<Vec<u32>>(), "8000 sessions, 8000 distinct epochs, none 0");
        assert_eq!(state.frontend_epoch.load(Relaxed), 8000);
    }
}
