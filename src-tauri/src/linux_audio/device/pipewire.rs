//! PipeWire's own view of the graph, which JACK's port list lacks: which nodes are webcams. Only the
//! default capture pick reads it, on a command thread or the device owner, never the audio callback.
use std::collections::HashSet;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

const DUMP_LIMIT: Duration = Duration::from_secs(2);

/// The JACK client names of webcam nodes. Without a readable `pw-dump` nothing counts as one.
pub(super) fn webcams() -> HashSet<String> {
    match output(&mut Command::new("pw-dump"), DUMP_LIMIT) {
        Ok(text) => webcam_names(&text),
        Err(e) => {
            log::warn!("[linux_audio] pw-dump {e}; no input is treated as a webcam");
            HashSet::new()
        }
    }
}

fn output(command: &mut Command, limit: Duration) -> Result<String, String> {
    let deadline = Instant::now() + limit;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start: {e}"))?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = mpsc::channel();
    // The dump outgrows a pipe's buffer, so it is drained while the child runs.
    let reader = thread::Builder::new()
        .name("lf-pw-dump".into())
        .spawn(move || {
            let mut out = Vec::new();
            let _ = tx.send(stdout.read_to_end(&mut out).map(|_| out));
        });
    let stop = |child: &mut std::process::Child, why: String| {
        let _ = child.kill();
        let _ = child.wait();
        Err(why)
    };
    if let Err(e) = reader {
        return stop(&mut child, format!("reader thread failed: {e}"));
    }
    let out = match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return stop(&mut child, format!("output unreadable: {e}")),
        Err(_) => return stop(&mut child, format!("timed out after {limit:?}")),
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(2)),
            Ok(None) => return stop(&mut child, format!("timed out after {limit:?}")),
            Err(e) => return stop(&mut child, format!("wait failed: {e}")),
        }
    };
    if !status.success() {
        return Err(format!("exited with {status}"));
    }
    String::from_utf8(out).map_err(|_| "printed text that is not UTF-8".into())
}

/// A node is a webcam when its props, or its device's, carry `device.form-factor` "webcam". JACK
/// names a node's client after one of its description, nick or name, so all three are returned.
pub(super) fn webcam_names(text: &str) -> HashSet<String> {
    let objects: Vec<Value> = match serde_json::from_str(text) {
        Ok(objects) => objects,
        Err(e) => {
            log::warn!("[linux_audio] pw-dump printed unparsable JSON ({e}); no input is treated as a webcam");
            return HashSet::new();
        }
    };
    let props = |o: &Value| o.pointer("/info/props").cloned().unwrap_or(Value::Null);
    let webcam = |p: &Value| p.get("device.form-factor").and_then(Value::as_str) == Some("webcam");
    let id = |v: &Value| v.as_u64().or_else(|| v.as_str()?.parse().ok());
    let of_type = |kind: &'static str| {
        objects
            .iter()
            .filter(move |o| o.get("type").and_then(Value::as_str) == Some(kind))
    };
    let devices: HashSet<u64> = of_type("PipeWire:Interface:Device")
        .filter(|o| webcam(&props(o)))
        .filter_map(|o| id(o.get("id")?))
        .collect();
    let mut names = HashSet::new();
    for node in of_type("PipeWire:Interface:Node") {
        let p = props(node);
        let on_webcam = p
            .get("device.id")
            .and_then(id)
            .is_some_and(|d| devices.contains(&d));
        if webcam(&p) || on_webcam {
            for key in ["node.description", "node.nick", "node.name"] {
                if let Some(name) = p.get(key).and_then(Value::as_str) {
                    names.insert(name.to_owned());
                }
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webcams_by_node_or_device_form_factor() {
        let dump = r#"[
          {"id": 0, "type": "PipeWire:Interface:Core", "info": {"name": "pipewire-0"}},
          {"id": 57, "type": "PipeWire:Interface:Device",
           "info": {"props": {"device.form-factor": "webcam", "media.class": "Audio/Device"}}},
          {"id": 58, "type": "PipeWire:Interface:Device",
           "info": {"props": {"media.class": "Audio/Device"}}},
          {"id": 73, "type": "PipeWire:Interface:Node", "info": {"props": {
            "node.description": "C920 Webcam Mic", "node.nick": "C920 Mic",
            "node.name": "alsa_input.c920", "device.id": 57, "device.form-factor": "webcam"}}},
          {"id": 74, "type": "PipeWire:Interface:Node", "info": {"props": {
            "node.description": "Other Cam", "node.name": "alsa_input.other", "device.id": "57"}}},
          {"id": 86, "type": "PipeWire:Interface:Node", "info": {"props": {
            "node.description": "EVO4 0", "node.name": "alsa_input.hw_EVO4_0", "device.id": 58}}},
          {"id": 90, "type": "PipeWire:Interface:Node", "info": {"props": {
            "node.description": "Virtual", "device.form-factor": "headset-ish"}}},
          {"id": 91, "type": "PipeWire:Interface:Node", "info": null}
        ]"#;
        let names = webcam_names(dump);
        let want: HashSet<String> = [
            "C920 Webcam Mic",
            "C920 Mic",
            "alsa_input.c920",
            "Other Cam",
            "alsa_input.other",
        ]
        .map(String::from)
        .into();
        assert_eq!(names, want);
    }

    #[test]
    fn a_dump_is_drained_whole_and_bounded() {
        let big = output(
            Command::new("sh").args(["-c", "head -c 500000 /dev/zero"]),
            Duration::from_secs(5),
        );
        assert_eq!(big.map(|t| t.len()), Ok(500_000), "more than a pipe holds");
        let start = Instant::now();
        let slow = output(Command::new("sleep").arg("5"), Duration::from_millis(200));
        assert!(matches!(&slow, Err(e) if e.contains("timed out")), "{slow:?}");
        assert!(start.elapsed() < Duration::from_secs(2));
        let failed = output(&mut Command::new("false"), Duration::from_secs(5));
        assert!(matches!(&failed, Err(e) if e.contains("exited")), "{failed:?}");
        let missing = output(&mut Command::new("lf-no-such-program"), Duration::from_secs(5));
        assert!(matches!(&missing, Err(e) if e.contains("could not start")), "{missing:?}");
    }

    #[test]
    fn unreadable_dumps_name_no_webcam() {
        for text in ["", "not json", "{\"type\": \"x\"}", "[1, 2"] {
            assert!(webcam_names(text).is_empty(), "{text:?}");
        }
    }
}
