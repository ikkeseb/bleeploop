//! Native audio input devices (cpal, WASAPI-shared): the capture-device list the input picker shows
//! (`plugin_list_input_devices`) and the WASAPI capture pick the engine's device side opens
//! (`engine_io::cpal_driver`). ASIO does not come through here: the engine builds both directions
//! from the startup-cached duplex driver (`audio_output::asio_cache`).
#![cfg(windows)]

use cpal::traits::{DeviceTrait, HostTrait};

/// One enumerated capture device. `id` is the stable cpal `DeviceId` string (round-trips through
/// `host.device_by_id` for arm-by-id); `name` is the human-readable description. Maps 1:1 to the
/// `AudioInputDevice` boundary struct the `plugin_list_input_devices` command returns to JS.
pub struct InputDeviceInfo {
    pub id: String,
    pub name: String,
    pub channels: u32,
}

/// Enumerate WASAPI-shared input devices. Opens NO stream, so it is safe to call directly off the
/// command thread.
pub fn list_input_devices() -> Result<Vec<InputDeviceInfo>, String> {
    let host = cpal::default_host();
    let devices = host
        .input_devices()
        .map_err(|e| format!("cpal input_devices: {e}"))?;
    let mut out = Vec::new();
    for dev in devices {
        // name() is deprecated in 0.18 → description() (Display) for the human name, id() for a
        // stable handle. Both are fallible; degrade gracefully rather than dropping the device.
        let name = dev
            .description()
            .map(|d| d.to_string())
            .unwrap_or_else(|_| "Unknown input".to_string());
        let id = dev
            .id()
            .map(|i| i.to_string())
            .unwrap_or_else(|_| name.clone());
        let channels = dev
            .default_input_config()
            .map(|c| c.channels() as u32)
            .unwrap_or(0);
        out.push(InputDeviceInfo { id, name, channels });
    }
    Ok(out)
}

/// Pick the WASAPI capture device (None = default). The ASIO low-latency tier does NOT go through here —
/// it uses the startup-cached duplex device (`audio_output::asio_cache()`) shared with the output, so
/// capture + playback ride ONE full-duplex driver (one clock); the single ASIO driver can't be
/// re-resolved once a stream holds it.
pub(crate) fn pick_input_device(device_id: Option<&str>) -> Result<cpal::Device, String> {
    let host = cpal::default_host();
    match device_id {
        Some(id) => {
            let parsed = id.parse().map_err(|_| format!("bad cpal device id: {id}"))?;
            host.device_by_id(&parsed)
                .ok_or_else(|| format!("cpal device_by_id({id}): not found"))
        }
        None => host
            .default_input_device()
            .ok_or_else(|| "no default input device".to_string()),
    }
}
