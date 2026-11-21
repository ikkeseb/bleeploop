//! OWNS: the chain commands (E2, `docs/unit-contract.md` §2): the `chain_*` Tauri commands the UI's
//! `ChainHost` calls (`src/platform/host.ts`), the per-slot call order, the editor affinity, the tone
//! tokens, and the `chain:editor-closed` window event. JSON arguments are camelCase; every command that
//! can wait runs on `spawn_blocking`.
//!
//! The epoch (`host_init`'s `frontendEpoch`) is checked at entry: a call from a replaced document is
//! refused; a call already running finishes and its unit stays (the units are the host's, and the new
//! document reads them from the reset frame). `plugin:*`, `plugin_*` and `host_init` are untouched:
//! chain units emit `chain:*` only.

// The seam (E2 S0): every command answers an error until the chain host is built (P4).
#![allow(dead_code)]

use lf_engine::UnitId;
use serde::Serialize;
use std::time::Duration;

use super::chain::{PluginRef, UnitSpec};
use super::mode::{engine, EngineApp};
use crate::host::{ParamDesc, PluginHostState};

/// What every command answers until the chain host is built.
const NOT_BUILT: &str = "chain host not built yet";

/// `chain_add`'s bound when the engine never answers (a stalled device): counted only while the model
/// shows the slot holding nothing; past it the call rejects and the plate is left `loading` (the model
/// follows the events). The legacy load's bound (`host::engine_slot`'s `LOAD_TIMEOUT`).
pub(crate) const ADD_TIMEOUT: Duration = Duration::from_secs(15);

/// The window event a unit's editor sends when the player closed it by its own close, or it went with
/// its unit. (`chain:param-changed` and `chain:params-changed` wait for the activation thread's drawer:
/// no subscriber in E2.)
pub(crate) const EDITOR_CLOSED: &str = "chain:editor-closed";

/// [`EDITOR_CLOSED`]'s payload.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct EditorClosed {
    pub unit: UnitId,
}

/// `chain_add`'s answer: the engine took the install, or holds it until a take ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Added {
    Seated,
    Waiting,
}

/// `chain_reload`'s answer: a fresh instance seated, the unit moved on since `since` (nothing done),
/// or the load failed (the old instance stays).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Reloaded {
    Reloaded,
    Moved,
    Failed,
}

/// `chain_tone_import`'s answer: the token a matching unit's reload passes (`None`: no unit matched,
/// and the bytes became the slot's file), the plugin's name, and the units the tone may go to: its own
/// unit if seated with that identity, in either slot; then the import slot's other seated units of that
/// identity, in chain order.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ChainToneImport {
    pub token: Option<u32>,
    pub name: String,
    pub matches: Vec<UnitId>,
}

/// A command's work on the runtime's blocking pool, reading the one shared `PluginHostState` there (as
/// `host/commands.rs`' `blocking`). A task that failed (it panicked) is the host's fault.
async fn blocking<T: Send + 'static>(
    app: tauri::AppHandle,
    op: &'static str,
    work: impl FnOnce(&'static EngineApp, &PluginHostState) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(move || {
        use tauri::Manager;
        work(engine()?, &app.state::<PluginHostState>())
    })
    .await
    .map_err(|e| format!("{op} task: {e}"))?
}

/// Reserve `unit` across both slots, build it, hand it over at plate index `at` (a source goes to the
/// head, replacing a seated one); settles on the engine's answer. Rejects at once for a reserved id or a
/// full chain, and on a failed load; past [`ADD_TIMEOUT`] unanswered.
#[tauri::command]
pub async fn chain_add(
    slot: u8,
    unit: UnitId,
    spec: UnitSpec,
    at: usize,
    tone_token: Option<u32>,
    frontend_epoch: u32,
    app: tauri::AppHandle,
) -> Result<Added, String> {
    blocking(app, "chain_add", move |_, _| {
        let _ = (slot, unit, spec, at, tone_token, frontend_epoch);
        Err(NOT_BUILT.into())
    })
    .await
}

/// Remove `unit`; settles on the removal's `Applied` (a failed add's plate goes at once).
#[tauri::command]
pub async fn chain_remove(unit: UnitId, frontend_epoch: u32, app: tauri::AppHandle) -> Result<(), String> {
    blocking(app, "chain_remove", move |_, _| {
        let _ = (unit, frontend_epoch);
        Err(NOT_BUILT.into())
    })
    .await
}

/// A fresh instance of `plugin` under the same id, place and bypass, if `unit` is still that plugin and
/// was not re-added since `since` (a `chains` frame's `seq`, `EngineHost::told`); the load applies
/// `tone_token`.
#[tauri::command]
pub async fn chain_reload(
    unit: UnitId,
    plugin: PluginRef,
    since: u64,
    tone_token: u32,
    frontend_epoch: u32,
    app: tauri::AppHandle,
) -> Result<Reloaded, String> {
    blocking(app, "chain_reload", move |_, _| {
        let _ = (unit, plugin, since, tone_token, frontend_epoch);
        Err(NOT_BUILT.into())
    })
    .await
}

/// Move `unit` to plate index `to` in its own chain (no move across slots).
#[tauri::command]
pub async fn chain_move(unit: UnitId, to: usize, frontend_epoch: u32, app: tauri::AppHandle) -> Result<(), String> {
    blocking(app, "chain_move", move |_, _| {
        let _ = (unit, to, frontend_epoch);
        Err(NOT_BUILT.into())
    })
    .await
}

/// Open `unit`'s editor on its owner thread (one editor per plugin file at a time: the affinity table).
#[tauri::command]
pub async fn chain_open_editor(unit: UnitId, frontend_epoch: u32, app: tauri::AppHandle) -> Result<(), String> {
    blocking(app, "chain_open_editor", move |_, _| {
        let _ = (unit, frontend_epoch);
        Err(NOT_BUILT.into())
    })
    .await
}

/// Close `unit`'s editor. Idempotent.
#[tauri::command]
pub async fn chain_close_editor(unit: UnitId, frontend_epoch: u32, app: tauri::AppHandle) -> Result<(), String> {
    blocking(app, "chain_close_editor", move |_, _| {
        let _ = (unit, frontend_epoch);
        Err(NOT_BUILT.into())
    })
    .await
}

/// `unit`'s plugin parameters (stable ids, ranges, live values), from its owner thread.
#[tauri::command]
pub async fn chain_list_params(unit: UnitId, app: tauri::AppHandle) -> Result<Vec<ParamDesc>, String> {
    blocking(app, "chain_list_params", move |_, _| {
        let _ = unit;
        Err(NOT_BUILT.into())
    })
    .await
}

/// Set one of `unit`'s plugin parameters; `param_id` must be one it listed.
#[tauri::command]
pub async fn chain_set_param(
    unit: UnitId,
    param_id: u32,
    value: f64,
    frontend_epoch: u32,
    app: tauri::AppHandle,
) -> Result<(), String> {
    blocking(app, "chain_set_param", move |_, _| {
        let _ = (unit, param_id, value, frontend_epoch);
        Err(NOT_BUILT.into())
    })
    .await
}

/// Save `unit`'s tone now, through its owner, and answer the tone file's bytes as the raw response
/// (empty: the plugin keeps no state).
#[tauri::command]
pub async fn chain_tone_take(unit: UnitId, app: tauri::AppHandle) -> Result<tauri::ipc::Response, String> {
    let bytes: Vec<u8> = blocking(app, "chain_tone_take", move |_, _| {
        let _ = unit;
        Err(NOT_BUILT.into())
    })
    .await?;
    Ok(tauri::ipc::Response::new(bytes))
}

/// Park a session's tone for the units it may go to. The raw request body is the tone file's bytes;
/// headers: `slot`, `plugin` (`{ format, path, id }` as JSON, ASCII with `\u` escapes, as
/// `plugin_tone_import`), `unit` (optional: the unit the session saved it for), `epoch`. The tone is
/// checked against `plugin` and nothing is written unless no unit matches (then it becomes the slot's
/// file, token `null`).
#[tauri::command]
pub async fn chain_tone_import(request: tauri::ipc::Request<'_>, app: tauri::AppHandle) -> Result<ChainToneImport, String> {
    let header = |name: &str| request.headers().get(name).and_then(|v| v.to_str().ok());
    let slot: u8 = header("slot").and_then(|v| v.parse().ok()).ok_or("chain_tone_import needs a slot header")?;
    let plugin: PluginRef = header("plugin")
        .and_then(|v| serde_json::from_str(v).ok())
        .ok_or("chain_tone_import needs the plugin the session names (a plugin header)")?;
    let unit: Option<UnitId> = match header("unit") {
        Some(v) => Some(v.parse().map_err(|_| "chain_tone_import's unit header is no unit id")?),
        None => None,
    };
    let epoch: u32 = header("epoch").and_then(|v| v.parse().ok()).ok_or("chain_tone_import needs an epoch header")?;
    let tauri::ipc::InvokeBody::Raw(bytes) = request.body() else {
        return Err("chain_tone_import takes the tone file's bytes as the raw request body".to_string());
    };
    let bytes = bytes.clone();
    blocking(app, "chain_tone_import", move |_, _| {
        let _ = (slot, plugin, unit, epoch, bytes);
        Err(NOT_BUILT.into())
    })
    .await
}

/// The reload a tone import answered `token` for did not happen: drop the tone parked for it.
#[tauri::command]
pub async fn chain_tone_forget(token: u32, frontend_epoch: u32) -> Result<(), String> {
    let _ = (token, frontend_epoch);
    Err(NOT_BUILT.into())
}
