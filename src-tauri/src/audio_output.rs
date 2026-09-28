//! Native audio output devices and the ASIO tier's driver cache (cpal): the output-device list
//! (`plugin_list_output_devices`), the WASAPI pick and config the engine's device side opens
//! (`engine_io::cpal_driver`, Share output), the backend type, and the one ASIO startup coordinator
//! with its cached duplex device (`asio_startup.rs` owns the rules).
//!
//! cpal 0.18 facts: `StreamConfig` is `Copy`/by-value; `sample_rate` is a bare `u32` alias;
//! `description()`/`id()` replace the deprecated `name()`.
#![cfg(windows)]

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{BufferSize, SampleFormat, StreamConfig};

/// One enumerated output device. Mirrors `audio_input::InputDeviceInfo` 1:1 → the
/// `AudioOutputDevice` boundary struct `plugin_list_output_devices` returns to JS.
pub struct OutputDeviceInfo {
    pub id: String,
    pub name: String,
    pub channels: u32,
}

/// Enumerate WASAPI-shared output devices. Opens NO stream, so it is safe to call off the command
/// thread (no owner-thread hop). Mirror of `audio_input::list_input_devices`.
pub fn list_output_devices() -> Result<Vec<OutputDeviceInfo>, String> {
    let host = cpal::default_host();
    let devices = host
        .output_devices()
        .map_err(|e| format!("cpal output_devices: {e}"))?;
    let mut out = Vec::new();
    for dev in devices {
        let name = dev
            .description()
            .map(|d| d.to_string())
            .unwrap_or_else(|_| "Unknown output".to_string());
        let id = dev
            .id()
            .map(|i| i.to_string())
            .unwrap_or_else(|_| name.clone());
        let channels = dev
            .default_output_config()
            .map(|c| c.channels() as u32)
            .unwrap_or(0);
        out.push(OutputDeviceInfo { id, name, channels });
    }
    Ok(out)
}

/// The ASIO device + its in/out configs, captured ONCE while the single ASIO driver is FREE (startup).
/// Why cache the whole Device, not just the config: once ANY stream (input OR output) holds the ASIO
/// driver, cpal can no longer RE-RESOLVE the device (`default_output_device()` → None) NOR re-query
/// configs (`default_*_config()` → Err) — yet it CAN still build the *other* direction's stream from a
/// device object obtained earlier (proven: cpal runs ASIO input+output duplex on one driver; and
/// `cpal::Device` is Send+Sync, so it lives in a static). So the engine's ASIO open
/// (`engine_io::cpal_driver`) builds both directions from this one cached device + config rather than
/// re-resolving. WASAPI is unaffected (it resolves fresh each time). Populated by the probe
/// (`probe_asio_driver`), requested by the frontend after the UI is up and before any open, and
/// replaced only by a driver switch (`switch_asio_driver`) while nothing holds the driver.
#[cfg(feature = "asio")]
pub struct AsioCache {
    pub name: String,
    pub device: cpal::Device,
    pub in_cfg: StreamConfig,
    pub in_fmt: SampleFormat,
    pub out_cfg: StreamConfig,
    pub out_fmt: SampleFormat,
    /// The buffer sizes the driver accepts (min, max frames); `None` when it did not say. cpal refuses a
    /// fixed size outside it, so the engine never asks for one (`engine_io::transition::asio_block`).
    pub buffer_range: Option<(u32, u32)>,
}
/// The ONE startup coordinator: owns the probe state machine (`asio_startup.rs`) and publishes the
/// cache. Present in every build so status/probe commands answer uniformly; `compiled` tells the
/// frontend whether ASIO can exist at all.
#[cfg(feature = "asio")]
static ASIO_PROBE: crate::asio_startup::Coordinator<AsioCache> = crate::asio_startup::Coordinator::new(true);
#[cfg(not(feature = "asio"))]
static ASIO_PROBE: crate::asio_startup::Coordinator<()> = crate::asio_startup::Coordinator::new(false);

/// Read the cached ASIO device + configs (None until a probe succeeded, while a switch replaces it, or on
/// a non-ASIO rig). A caller keeps the `Arc` it took; a switch never pulls a driver from under it.
#[cfg(feature = "asio")]
pub fn asio_cache() -> Option<std::sync::Arc<AsioCache>> {
    ASIO_PROBE.payload()
}

/// `--disable-asio` launch policy: recorded once in `run()`, before any command can arrive.
pub fn set_asio_disabled_by_flag() {
    ASIO_PROBE.set_disabled_by_flag();
}

/// Current probe status; never touches the driver.
pub fn asio_startup_status() -> crate::asio_startup::AsioStatusReport {
    ASIO_PROBE.status()
}

/// Deadline for one probe. The device query normally completes well inside a second; a driver that
/// takes longer is treated as hung for this process (see `asio_startup.rs` for why no retry follows).
const ASIO_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Run (or refuse) the ASIO probe on the automatic driver choice: the DEV probes' entry, so only an asio
/// debug build calls it.
#[allow(dead_code)]
pub fn probe_asio_startup(sentinel: &std::path::Path, explicit: bool) -> crate::asio_startup::AsioStatusReport {
    probe_asio_driver(sentinel, explicit, None)
}

/// Run (or refuse) the ASIO probe. Called from the `plugin_asio_probe` command, which
/// the frontend issues AFTER the window is up and only when the saved preference is on (`explicit`
/// false) or the user asks (`explicit` true). `driver` is the saved pick (`None` = automatic).
/// `sentinel` lives in the app's local data dir.
pub fn probe_asio_driver(
    sentinel: &std::path::Path,
    explicit: bool,
    driver: Option<String>,
) -> crate::asio_startup::AsioStatusReport {
    #[cfg(feature = "asio")]
    {
        ASIO_PROBE.probe(sentinel, explicit, move || resolve_asio_cache(driver.as_deref()), ASIO_PROBE_TIMEOUT)
    }
    #[cfg(not(feature = "asio"))]
    {
        let _ = (sentinel, explicit, driver);
        ASIO_PROBE.status()
    }
}

/// Switch to another ASIO driver (`None` = automatic) without a restart: the cached driver is dropped
/// and `driver` probed in its place. The engine calls this on its device owner with its ASIO run
/// closed (`engine_io::mode::switch_asio`), so nothing holds the driver; a probe that times out leaves
/// ASIO off until a restart, as at startup.
pub fn switch_asio_driver(
    sentinel: &std::path::Path,
    driver: Option<String>,
) -> Result<crate::asio_startup::AsioStatusReport, String> {
    #[cfg(feature = "asio")]
    {
        ASIO_PROBE.switch(sentinel, || None, move || resolve_asio_cache(driver.as_deref()), ASIO_PROBE_TIMEOUT)
    }
    #[cfg(not(feature = "asio"))]
    {
        let _ = (sentinel, driver);
        Ok(ASIO_PROBE.status())
    }
}

/// The installed ASIO drivers' names, as the SDK lists them from the registry: no driver is loaded, so
/// this is safe while one runs. Empty in a build without ASIO.
pub fn asio_driver_names() -> Vec<String> {
    #[cfg(feature = "asio")]
    {
        // The SDK's list brackets its registry read with CoInitialize/CoUninitialize on the calling
        // thread; a thread of its own keeps that away from a pool thread's COM apartment.
        std::thread::Builder::new()
            .name("lf-asio-names".into())
            .spawn(|| asio_sys::Asio::new().driver_names())
            .ok()
            .and_then(|t| t.join().ok())
            .unwrap_or_default()
    }
    #[cfg(not(feature = "asio"))]
    {
        Vec::new()
    }
}

/// The device backend a request asks for and a running device reports (`engine_io::DeviceRequest`,
/// `DeviceStatus`); on the wire as `"Wasapi"` / `"Asio"`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum AudioBackend {
    Wasapi,
    Asio,
}

impl AudioBackend {
    pub fn is_asio(self) -> bool {
        self == Self::Asio
    }
}

/// Whether an ASIO low-latency device is AVAILABLE to select (the `asio` feature is compiled AND the
/// probe published a device). Always false in a build without the feature.
pub fn asio_available() -> bool {
    #[cfg(feature = "asio")]
    {
        asio_cache().is_some()
    }
    #[cfg(not(feature = "asio"))]
    {
        false
    }
}

/// Resolve the ASIO duplex device and its in/out configs while the driver is FREE. This is the ONLY
/// function that contacts an ASIO driver outside a stream build: `default_output_device()` loads and
/// initialises the driver DLL in-process (asio-sys → `CoCreateInstance` + `ASIOInit`), which is where
/// a broken driver hangs or crashes. Runs on the coordinator's probe thread, never from `run()`.
/// A `None` from cpal cannot distinguish "no driver installed" from "every driver failed to load"
/// (cpal skips drivers that fail), so the message says "no usable driver".
///
/// `driver` names the pick (`None` = automatic: the default output's driver, else the default input's,
/// else the first). Found by name through cpal, which loads each driver listed before it once. A pick
/// that is not installed any more falls back to automatic (logged); one that is installed but does not
/// load is an error, never a silent switch to another interface.
#[cfg(feature = "asio")]
fn resolve_asio_cache(driver: Option<&str>) -> Result<AsioCache, String> {
    log::info!("[audio_output] ASIO probe: contacting the driver (host, device, configs); pick {driver:?}");
    let host = cpal::host_from_id(cpal::HostId::Asio)
        .map_err(|e| format!("ASIO host unavailable ({e})"))?;
    let named = match driver {
        Some(name) if asio_driver_names().iter().any(|n| n == name) => {
            let found = host
                .devices()
                .map_err(|e| format!("ASIO devices: {e}"))?
                .find(|d| d.description().is_ok_and(|x| x.name() == name));
            Some(found.ok_or_else(|| format!("the ASIO driver \"{name}\" did not start"))?)
        }
        Some(name) => {
            log::warn!("[audio_output] no ASIO driver named \"{name}\" is installed; taking the automatic choice");
            None
        }
        None => None,
    };
    let dev = named.or_else(|| {
        host.default_output_device()
            .or_else(|| host.default_input_device())
            .or_else(|| host.devices().ok().and_then(|mut it| it.next()))
    });
    let Some(d) = dev else {
        return Err("no usable ASIO driver found".to_string());
    };
    match (d.default_input_config(), d.default_output_config()) {
        (Ok(ic), Ok(oc)) => {
            if ic.sample_rate() == 0 || oc.sample_rate() == 0 {
                return Err("ASIO driver reported a zero sample rate".to_string());
            }
            let name = d
                .description()
                .map(|x| x.to_string())
                .unwrap_or_else(|_| "ASIO".to_string());
            let buffer_range = match *oc.buffer_size() {
                cpal::SupportedBufferSize::Range { min, max } if min > 0 && min <= max => Some((min, max)),
                _ => None,
            };
            let cache = AsioCache {
                name: name.clone(),
                in_cfg: StreamConfig {
                    channels: ic.channels(),
                    sample_rate: ic.sample_rate(),
                    buffer_size: BufferSize::Default,
                },
                in_fmt: ic.sample_format(),
                out_cfg: StreamConfig {
                    channels: oc.channels(),
                    sample_rate: oc.sample_rate(),
                    buffer_size: BufferSize::Default,
                },
                out_fmt: oc.sample_format(),
                buffer_range,
                device: d,
            };
            log::info!(
                "[audio_output] cached ASIO \"{name}\": in {:?} {:?} / out {:?} {:?}, buffers {:?}",
                cache.in_cfg,
                cache.in_fmt,
                cache.out_cfg,
                cache.out_fmt,
                cache.buffer_range
            );
            Ok(cache)
        }
        (ic, oc) => Err(format!(
            "ASIO config query failed (input ok={}, output ok={})",
            ic.is_ok(),
            oc.is_ok()
        )),
    }
}

/// Resolve the WASAPI output (StreamConfig, SampleFormat) via a live query (the ASIO path uses the
/// cached config instead — see AsioCache).
pub(crate) fn output_config(device: &cpal::Device) -> Result<(StreamConfig, SampleFormat), String> {
    let c = device
        .default_output_config()
        .map_err(|e| format!("cpal default_output_config: {e}"))?;
    Ok((
        StreamConfig {
            channels: c.channels(),
            sample_rate: c.sample_rate(),
            buffer_size: BufferSize::Default,
        },
        c.sample_format(),
    ))
}

/// Pick the WASAPI output device (None = default). The ASIO low-latency tier does NOT go through here —
/// it uses the startup-cached duplex device (`asio_cache()`), because the single ASIO driver can't be
/// re-resolved once a stream holds it.
pub(crate) fn pick_output_device(device_id: Option<&str>) -> Result<cpal::Device, String> {
    let host = cpal::default_host();
    match device_id {
        Some(id) => {
            let parsed = id.parse().map_err(|_| format!("bad cpal device id: {id}"))?;
            host.device_by_id(&parsed)
                .ok_or_else(|| format!("cpal device_by_id({id}): not found"))
        }
        None => host
            .default_output_device()
            .ok_or_else(|| "no default output device".to_string()),
    }
}
