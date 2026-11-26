//! The existing Tauri engine interface backed by the JACK host.
use serde_json::Value;
use std::time::Instant;
use tauri::{
    ipc::{Channel, InvokeBody, Request, Response},
    State,
};

use super::{
    device::{DeviceRequest, DeviceStatus, InputPick},
    lock,
    wire::{FeedFrame, InputItem},
    App, OpenError,
};

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub(crate) async fn engine_open(
    state: State<'_, App>,
    request: DeviceRequest,
    force: bool,
) -> Result<DeviceStatus, OpenError> {
    let host = state.host.clone();
    tauri::async_runtime::spawn_blocking(move || host.open(request, force))
        .await
        .map_err(|e| OpenError::Failed(e.to_string()))?
}

#[tauri::command]
pub(crate) async fn engine_close(state: State<'_, App>) -> Result<(), String> {
    let host = state.host.clone();
    blocking(move || {
        host.close();
        Ok(())
    })
    .await
}

#[tauri::command]
pub(crate) async fn engine_status(state: State<'_, App>) -> Result<Option<DeviceStatus>, String> {
    Ok(state.host.status())
}

#[tauri::command]
pub(crate) async fn engine_set_slot_input_channel(
    state: State<'_, App>,
    slot: usize,
    channel: Option<InputPick>,
) -> Result<(), String> {
    let host = state.host.clone();
    blocking(move || host.set_channel(slot, channel)).await
}

#[tauri::command]
pub(crate) async fn engine_set_share(endpoint: Option<String>) -> Result<(), String> {
    if endpoint.is_none() {
        Ok(())
    } else {
        Err("Share output is unavailable on JACK".into())
    }
}

#[tauri::command]
pub(crate) async fn midi_subscribe(
    state: State<'_, App>,
    origin: f64,
    channel: Channel<Value>,
) -> Result<u64, String> {
    let mut run = lock(&state.host.core.run);
    let active = run.device.is_some();
    Ok(run.input.subscribe(&state.host, origin, channel, active))
}

#[tauri::command]
pub(crate) async fn input_send(
    state: State<'_, App>,
    epoch: u64,
    items: Vec<InputItem>,
) -> Result<Option<&'static str>, String> {
    let mut run = lock(&state.host.core.run);
    let running = run.device.as_ref().is_some_and(|d| d.fault().is_none());
    run.input.send(&state.host, epoch, items, running)
}

#[tauri::command]
pub(crate) async fn engine_feed(
    state: State<'_, App>,
    channel: Channel<FeedFrame>,
) -> Result<(), String> {
    state
        .feed
        .subscribe(move |frame| channel.send(frame).is_ok());
    Ok(())
}

#[tauri::command]
pub(crate) async fn engine_feed_ack(
    state: State<'_, App>,
    seq: Option<u64>,
) -> Result<bool, String> {
    Ok(state.feed.ack(seq, Instant::now()))
}

#[tauri::command]
pub(crate) async fn engine_snapshot(
    state: State<'_, App>,
    master: bool,
) -> Result<Response, String> {
    let host = state.host.clone();
    blocking(move || host.snapshot(master).map(Response::new)).await
}

#[tauri::command]
pub(crate) async fn engine_load_session(
    state: State<'_, App>,
    request: Request<'_>,
) -> Result<(), String> {
    let InvokeBody::Raw(bytes) = request.body() else {
        return Err("engine_load_session takes raw session bytes".into());
    };
    let (host, bytes) = (state.host.clone(), bytes.clone());
    blocking(move || host.load_session(&bytes)).await
}
