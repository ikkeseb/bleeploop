//! OWNS: a session's bytes between the UI and the engine (`docs/plans/native-engine.md` § Stage 5,
//! Session): [`EngineHost::snapshot`] (what `engine_snapshot` answers, for export and crash recovery)
//! and [`EngineHost::load_session`] (what `engine_load_session` takes, for import and recovery). The PCM
//! crosses to the UI once per save; the engine side is `lf_engine::session` (a budgeted copy out, a
//! swap in: nothing allocates, frees or waits on the audio thread). Buffers are built and freed here, on
//! the calling thread, never the audio thread's.
//!
//! Both directions: `[u32 LE header length][UTF-8 JSON header][f32 LE mono PCM]`, one block of
//! `masterLengthFrames` samples per header track, in header order, in play order (a reversed lane's
//! loop as it is heard; `reversed` is its flag). A snapshot's header:
//! `{"rate","masterLengthFrames","bpm","tracks":[{"index","frames","reversed","state"}]}`, the committed
//! lanes ascending, `state` `"Playing"` | `"Stopped"` | `"Overdubbing"` (an overdubbing lane gives its
//! loop as committed before the layer in flight). A load's: `{"bpm","bars","masterLengthFrames","tracks"}`
//! with `state` `"Playing"` | `"Stopped"`, into an engine whose lanes are all EMPTY; `bpm` is an integer
//! 40..300 and `masterLengthFrames` is `bars` bars of it at the engine's rate.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::Ordering::{Acquire, Relaxed};
use std::time::{Duration, Instant};

use lf_engine::grid::{frames_per_bar, Frame};
use lf_engine::overview::PEAK_FRAMES;
use lf_engine::{LaneState, Load, LoadTrack, SessionError, SessionJob, SessionPort, Snapshot, TRACK_COUNT};
use serde::{Deserialize, Serialize};

use super::EngineHost;

/// How long a snapshot or a load may take: a snapshot of five 60-second lanes copies for about 1.4 s
/// at 48 kHz, after waiting out a block job.
const WAIT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(2);
/// A snapshot the looper wrote under, or too small, is taken again this many times.
const ATTEMPTS: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum TrackState {
    Playing,
    Stopped,
    Overdubbing,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrackHeader {
    index: u8,
    frames: Frame,
    reversed: bool,
    state: TrackState,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotHeader {
    rate: u32,
    master_length_frames: Frame,
    bpm: u32,
    tracks: Vec<TrackHeader>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoadHeader {
    bpm: u32,
    bars: Frame,
    master_length_frames: Frame,
    tracks: Vec<TrackHeader>,
}

impl EngineHost {
    /// The committed loops as they stand, as bytes (the module doc's layout). Err while no engine
    /// exists, or when the loops keep changing under the copy.
    pub fn snapshot(&self) -> Result<Vec<u8>, String> {
        let mut stale = self.core.session_busy.lock().unwrap_or_else(|e| e.into_inner());
        let mut need = self.snapshot_estimate()?;
        for _ in 0..ATTEMPTS {
            let mut pcm = Vec::with_capacity(need);
            // Written through: the engine copies into these pages, which must not fault on its thread.
            pcm.resize(need, 0.0f32);
            let SessionJob::Snapshot(s) = self.session_job(&mut stale, SessionJob::Snapshot(Snapshot::new(pcm)))? else {
                return Err("the engine answered a snapshot with a load".to_string());
            };
            match s.result {
                Some(Ok(())) => return Ok(snapshot_bytes(&s)),
                Some(Err(SessionError::TooSmall(more))) => need = more,
                Some(Err(SessionError::Changed)) => {}
                Some(Err(e)) => return Err(e.text()),
                None => return Err("the snapshot came back unfinished".to_string()),
            }
        }
        Err("the loops kept changing while the snapshot copied them; try again".to_string())
    }

    /// Load a session's bytes (the module doc's layout) into the engine, whose lanes must all be
    /// EMPTY. The PLAYING lanes start together from loop position 0; volume, mute and FX are the UI's
    /// to send after.
    pub fn load_session(&self, bytes: &[u8]) -> Result<(), String> {
        let mut stale = self.core.session_busy.lock().unwrap_or_else(|e| e.into_inner());
        let (header, pcm) = split(bytes)?;
        let header: LoadHeader = serde_json::from_slice(header).map_err(|e| format!("session header: {e}"))?;
        let rate = self.core.rate().ok_or("no audio device is open")?;
        let capacity = self.capacity()?;
        let master = header.master_length_frames;
        if !(40..=300).contains(&header.bpm) || header.bars < 1 {
            return Err(format!("session: bpm {} or bars {} out of range", header.bpm, header.bars));
        }
        if master != header.bars * frames_per_bar(header.bpm as f64, rate) {
            return Err(format!("session: {} bars at {} BPM are not {master} frames at {rate} Hz", header.bars, header.bpm));
        }
        if master <= 0 || master as usize > capacity {
            return Err(format!("session: {master} frames do not fit the engine's {capacity}"));
        }
        let samples = master as usize;
        if header.tracks.is_empty() || header.tracks.len() > TRACK_COUNT || pcm.len() != header.tracks.len() * samples * 4 {
            return Err(format!("session: {} tracks and {} PCM bytes do not match", header.tracks.len(), pcm.len()));
        }
        let mut tracks = Vec::with_capacity(header.tracks.len());
        for (k, t) in header.tracks.iter().enumerate() {
            if t.frames != master || usize::from(t.index) >= TRACK_COUNT || t.state == TrackState::Overdubbing {
                return Err(format!("session: track {} does not fit", t.index));
            }
            let mut buf = Vec::with_capacity(capacity);
            buf.resize(capacity, 0.0f32);
            for (x, b) in buf.iter_mut().zip(pcm[k * samples * 4..(k + 1) * samples * 4].chunks_exact(4)) {
                *x = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            }
            if t.reversed {
                // Play order back to buffer order: the engine reads a reversed lane's buffer backwards.
                buf[..samples].reverse();
            }
            let peaks = buf[..samples].chunks(PEAK_FRAMES).map(|c| c.iter().fold((0.0f32, 0.0f32), |(lo, hi), &x| (lo.min(x), hi.max(x)))).collect();
            tracks.push(LoadTrack { index: t.index, buf, peaks, reversed: t.reversed, playing: t.state == TrackState::Playing });
        }
        let load = Load { bpm: header.bpm, bars: header.bars, master, tracks, result: None };
        let SessionJob::Load(load) = self.session_job(&mut stale, SessionJob::Load(load))? else {
            return Err("the engine answered a load with a snapshot".to_string());
        };
        // `load.tracks` now holds the engine's old buffers: they are freed here.
        match load.result {
            Some(Ok(())) => {
                log::info!("[engine_io] session loaded: {} tracks, {master} frames at {} BPM", load.tracks.len(), header.bpm);
                Ok(())
            }
            Some(Err(e)) => Err(e.text()),
            None => Err("the load came back unfinished".to_string()),
        }
    }

    /// The engine's lane capacity, in frames.
    fn capacity(&self) -> Result<usize, String> {
        let ends = self.core.ends.lock().map_err(|_| "engine ends poisoned".to_string())?;
        ends.as_ref().map(|e| e.overview.capacity()).ok_or_else(|| "no audio device is open".to_string())
    }

    /// Samples a snapshot needs at most now: every lane's frames (a take in flight counted too).
    fn snapshot_estimate(&self) -> Result<usize, String> {
        let ends = self.core.ends.lock().map_err(|_| "engine ends poisoned".to_string())?;
        let overview = &ends.as_ref().ok_or("no audio device is open")?.overview;
        Ok((0..TRACK_COUNT).map(|i| overview.lane(i).frames.max(0) as usize).sum::<usize>().max(1))
    }

    /// Hand `job` to the engine and wait (≤ `WAIT`) for it to come back. While no device runs the
    /// engine is serviced here, under its lock. The port is out of `Ends` meanwhile; an engine that is
    /// replaced meanwhile takes its job with it. `stale`: jobs earlier calls gave up on, still in the
    /// engine of that generation; they come back first and are dropped.
    fn session_job(&self, stale: &mut (u64, usize), job: SessionJob) -> Result<SessionJob, String> {
        let (mut port, gen) = {
            let mut ends = self.core.ends.lock().map_err(|_| "engine ends poisoned".to_string())?;
            let ends = ends.as_mut().ok_or("no audio device is open")?;
            (ends.session.take().ok_or("the session port is in use")?, self.core.engine_gen.load(Acquire))
        };
        if stale.0 != gen {
            *stale = (gen, 0);
        }
        let result = self.run_job(&mut port, gen, &mut stale.1, job);
        let mut ends = self.core.ends.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ends) = ends.as_mut().filter(|_| self.core.engine_gen.load(Acquire) == gen) {
            ends.session = Some(port);
        }
        result
    }

    fn run_job(&self, port: &mut SessionPort, gen: u64, stale: &mut usize, job: SessionJob) -> Result<SessionJob, String> {
        let deadline = Instant::now() + WAIT;
        let mut job = Some(job);
        loop {
            self.service_session_if_idle();
            if let Some(back) = port.returned() {
                if *stale > 0 {
                    *stale -= 1;
                } else if job.is_none() {
                    return Ok(*back);
                }
            }
            if *stale == 0 {
                if let Some(next) = job.take() {
                    if let Err(back) = port.send(Box::new(next)) {
                        job = Some(*back);
                    }
                }
            }
            if self.core.engine_gen.load(Acquire) != gen {
                return Err("the engine was replaced during the session job".to_string());
            }
            if Instant::now() >= deadline {
                // Sent and not back: it comes back to a later call, which drops it.
                *stale += usize::from(job.is_none());
                return Err(format!("the session job did not finish within {} s", WAIT.as_secs()));
            }
            std::thread::sleep(POLL);
        }
    }

    /// With no device running nothing services the port: do it here, under the engine lock, as a slot
    /// host does (`SlotHost::service_if_idle`).
    fn service_session_if_idle(&self) {
        if self.core.running.load(Acquire) {
            return;
        }
        let mut rt = self.core.rt.lock().unwrap_or_else(|e| e.into_inner());
        if self.core.running.load(Acquire) || rt.faulted {
            return;
        }
        let rt = &mut *rt;
        let Some(engine) = rt.engine.as_mut() else { return };
        if catch_unwind(AssertUnwindSafe(|| engine.service_session_idle())).is_err() {
            self.core.counters.panics.fetch_add(1, Relaxed);
            self.core.latch_fault(rt);
        }
    }
}

/// `[u32 LE length][header][rest]`.
fn split(bytes: &[u8]) -> Result<(&[u8], &[u8]), String> {
    let len = bytes.get(..4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize).ok_or("session: no header length")?;
    let header = bytes.get(4..4 + len).ok_or("session: the header runs past the bytes")?;
    Ok((header, &bytes[4 + len..]))
}

fn snapshot_bytes(s: &Snapshot) -> Vec<u8> {
    let master = s.master as usize;
    let tracks = s.tracks[..s.count]
        .iter()
        .flatten()
        .map(|t| TrackHeader {
            index: t.index,
            frames: s.master,
            reversed: t.reversed,
            state: match t.state {
                LaneState::Stopped => TrackState::Stopped,
                LaneState::Overdubbing => TrackState::Overdubbing,
                _ => TrackState::Playing,
            },
        })
        .collect();
    let header = SnapshotHeader { rate: s.rate, master_length_frames: s.master, bpm: s.bpm, tracks };
    let json = serde_json::to_vec(&header).unwrap_or_default();
    let samples = &s.pcm[..s.count * master];
    let mut out = Vec::with_capacity(4 + json.len() + 4 * samples.len());
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(&json);
    for x in samples {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_layout_splits_at_its_header_length() {
        let mut bytes = 5u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(b"{\"a\"}");
        bytes.extend_from_slice(&1.5f32.to_le_bytes());
        let (header, pcm) = split(&bytes).unwrap();
        assert_eq!((header, pcm), (&b"{\"a\"}"[..], &1.5f32.to_le_bytes()[..]));
        assert!(split(&bytes[..3]).is_err() && split(&bytes[..6]).is_err());
    }
}
