//! The listening sheet's clip: what the rig rendered from `begin` on (the device output, after the
//! limiter, with the live monitor), the marks its question points at, and its manifest entry. `finish`
//! checks the clip (length, marks inside it, not silent, headroom) and writes `<id>.wav` (48 kHz 16-bit
//! PCM, mono or stereo) and `<id>.json` into `$LF_LISTEN_OUT`; `scripts/listen.mjs` sets it, merges
//! the entries into `manifest.json` and builds the page. Nothing here is normalised: a level difference
//! the question is about stays in the clip.

use std::path::PathBuf;

use lf_engine::grid::Frame;
use serde_json::{json, Value};

use crate::common::Rig;

/// The groups, in the page's order.
pub const GROUPS: [&str; 4] = ["Clicks and seams", "Timing", "Tone and level", "Stereo (headphones)"];

/// What the page says about a clip.
pub struct Meta {
    pub id: &'static str,
    /// An index into [`GROUPS`], and the clip's place in its group.
    pub group: usize,
    pub order: usize,
    pub title: &'static str,
    pub question: &'static str,
    pub cannot_tell: &'static str,
    /// The first words of the `STATUS.md` line it answers.
    pub status: &'static str,
    pub stereo: bool,
    /// The longest the clip may be: 8 s, or 10 where a tail must ring out.
    pub max_seconds: f64,
    pub settings: Value,
}

pub struct Clip {
    start: Frame,
    sr: u32,
    /// The master limiter's pre-delay: the looper, the click and the lanes' FX reach the output this late;
    /// the live monitor joins after it.
    lag: Frame,
    marks: Vec<(String, Frame)>,
    b_start: Option<Frame>,
}

/// Where the clips go: `$LF_LISTEN_OUT`, set by `pnpm listen`.
pub fn out_dir() -> PathBuf {
    match std::env::var_os("LF_LISTEN_OUT") {
        Some(dir) => PathBuf::from(dir),
        None => panic!("LF_LISTEN_OUT is not set: run the listening sheet with `pnpm listen`, which sets it and builds the page"),
    }
}

impl Clip {
    /// The clip starts at the rig's next frame.
    pub fn begin(rig: &mut Rig) -> Clip {
        out_dir();
        rig.keep_output();
        Clip { start: rig.frame, sr: rig.sr, lag: rig.engine.limiter_latency(), marks: Vec::new(), b_start: None }
    }

    /// A moment heard through the bus (a loop, the click, a lane's FX): it reaches the output a pre-delay late.
    pub fn mark(&mut self, label: impl Into<String>, frame: Frame) {
        self.marks.push((label.into(), frame + self.lag));
    }

    /// A moment heard on the live monitor (the player's own input, an input send): on the output as is.
    pub fn mark_out(&mut self, label: impl Into<String>, frame: Frame) {
        self.marks.push((label.into(), frame));
    }

    /// Where the B half of an A/B starts, an engine frame heard through the bus.
    pub fn b_half(&mut self, frame: Frame) {
        self.b_start = Some(frame + self.lag);
    }

    /// Where the B half of an A/B starts, on the live monitor.
    pub fn b_half_out(&mut self, frame: Frame) {
        self.b_start = Some(frame);
    }

    fn seconds(&self, frame: Frame) -> f64 {
        ((frame - self.start) as f64 / self.sr as f64 * 1000.0).round() / 1000.0
    }

    pub fn finish(self, rig: &Rig, meta: Meta) {
        let (left, right) = (&rig.heard, &rig.heard_right);
        let frames = left.len();
        let seconds = frames as f64 / self.sr as f64;
        let id = meta.id;
        assert!(seconds <= meta.max_seconds + 1e-9, "{id}: {seconds:.2} s is over its {} s", meta.max_seconds);
        let channels: Vec<Vec<f32>> = if meta.stereo { vec![left.clone(), right.clone()] } else { vec![left.iter().zip(right).map(|(l, r)| 0.5 * (l + r)).collect()] };
        let peak = channels.iter().flatten().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(peak <= 0.8, "{id}: peak {peak:.3} is too close to full scale");
        assert!(peak >= 0.02, "{id}: silent (peak {peak:.4})");
        let mean = channels.iter().flatten().map(|&x| x as f64).sum::<f64>() / (frames * channels.len()) as f64;
        assert!(mean.abs() < 0.01, "{id}: a DC offset of {mean:.4}");
        let mut marks = Vec::new();
        for (label, frame) in &self.marks {
            let t = self.seconds(*frame);
            assert!((0.0..=seconds).contains(&t), "{id}: the mark {label:?} at {t} s lies outside the clip's {seconds:.2} s");
            marks.push(json!({ "t": t, "label": label }));
        }
        let b_start = self.b_start.map(|f| {
            let t = self.seconds(f);
            assert!(t > 0.0 && t < seconds, "{id}: the B half at {t} s lies outside the clip");
            t
        });
        let dir = out_dir();
        std::fs::create_dir_all(&dir).expect("create the clips' folder");
        write_pcm16(&dir.join(format!("{id}.wav")), self.sr, &channels);
        let entry = json!({
            "id": id,
            "group": GROUPS[meta.group],
            "order": meta.group * 100 + meta.order,
            "title": meta.title,
            "question": meta.question,
            "marks": marks,
            "b_start": b_start,
            "headphones": meta.stereo,
            "cannot_tell": meta.cannot_tell,
            "settings": meta.settings,
            "status": meta.status,
            "file": format!("{id}.wav"),
            "seconds": (seconds * 1000.0).round() / 1000.0,
            "channels": channels.len(),
            "peak_dbfs": (20.0 * (peak as f64).log10() * 10.0).round() / 10.0,
            "sample_rate": self.sr,
        });
        std::fs::write(dir.join(format!("{id}.json")), serde_json::to_string_pretty(&entry).unwrap()).expect("write the clip's entry");
        println!("{id}: {seconds:.2} s, peak {peak:.3}, {} marks", self.marks.len());
    }
}

/// A 16-bit PCM WAV, the channels interleaved.
fn write_pcm16(path: &std::path::Path, rate: u32, channels: &[Vec<f32>]) {
    let n = channels.len() as u16;
    let frames = channels[0].len();
    let data = frames * n as usize * 2;
    let mut b = Vec::with_capacity(44 + data);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&((36 + data) as u32).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&n.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * n as u32 * 2).to_le_bytes());
    b.extend_from_slice(&(n * 2).to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&(data as u32).to_le_bytes());
    for f in 0..frames {
        for c in channels {
            let s = (c[f].clamp(-1.0, 1.0) * 32767.0).round() as i16;
            b.extend_from_slice(&s.to_le_bytes());
        }
    }
    std::fs::write(path, b).expect("write the clip");
}
