//! OWNS: tone recall's pure half (engine mode): the tone file, the store, the VST3 state container and
//! the save debounce. A TONE is a plugin's saved state: a CLAP plugin's `state` blob, or a VST3 plugin's
//! component and edit-controller states in one container (`encode_vst3`). The tone follows the plugin,
//! not the slot: the store keeps one file per plugin identity (format, path, id) in the app-local data
//! folder's `tones/`, named by a stable FNV-1a hash of that identity and replaced atomically. The same
//! file is what a session export carries per slot.
//!
//! Nothing here touches a plugin. The engine-mode owners (`clap_engine`, `vst3_engine`) restore a tone
//! only inside their load sequence, before the plugin activates, and save one only on their own thread,
//! through a [`ToneKeeper`]: debounced after the last change, when the editor closes, before an unload,
//! and when a session export or the app's exit asks.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{
    AtomicBool, AtomicU64,
    Ordering::{Acquire, Relaxed},
};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long a plugin must go without a change before its tone is saved. A kill loses at most this.
pub(crate) const SAVE_QUIET: Duration = Duration::from_secs(2);

/// The largest state a tone holds. A session import sizes its archive limit from it
/// (`src/audio/export/import.ts`, `MAX_TONE_BYTES`); change them together.
pub(crate) const MAX_STATE_BYTES: usize = 16 << 20;

/// The largest format, path, id or name a tone file carries.
const MAX_FIELD_BYTES: usize = 4096;

/// A tone file: `MAGIC`, the version (u16), four length-prefixed UTF-8 fields (format, path, id,
/// name; u32 lengths), the length-prefixed state, then an FNV-1a 64 of every byte before it. Little
/// endian throughout.
const MAGIC: [u8; 4] = *b"BLTN";
const VERSION: u16 = 1;

/// The VST3 container inside a tone's state: `VST3_MAGIC`, its version (u16), then the component's
/// and the edit controller's states, each length-prefixed (u32).
const VST3_MAGIC: [u8; 4] = *b"LFV3";
const VST3_VERSION: u16 = 1;

/// FNV-1a 64: stable across Rust versions and runs, unlike `DefaultHasher`.
fn fnv1a64(parts: &[&[u8]]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for &byte in *part {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

/// Which plugin a tone belongs to: the scan descriptor's format (`clap` | `vst3`), bundle path and
/// plugin id.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct ToneIdentity {
    pub(crate) format: String,
    pub(crate) path: String,
    pub(crate) id: String,
}

impl ToneIdentity {
    /// The store's file for this plugin. Each field is followed by a 0 byte, so no two identities
    /// hash the same concatenation.
    pub(crate) fn file_name(&self) -> String {
        let key = fnv1a64(&[self.format.as_bytes(), &[0], self.path.as_bytes(), &[0], self.id.as_bytes(), &[0]]);
        format!("{key:016x}.tone")
    }
}

/// One tone: whose it is, the plugin's name (a session import names it in its toast) and the state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Tone {
    pub(crate) identity: ToneIdentity,
    pub(crate) name: String,
    pub(crate) state: Vec<u8>,
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// A tone file's bytes. Fields past their limits are the caller's bug: `ToneKeeper` never builds one.
pub(crate) fn encode(tone: &Tone) -> Vec<u8> {
    let fields = [&tone.identity.format, &tone.identity.path, &tone.identity.id, &tone.name];
    let mut out = Vec::with_capacity(32 + fields.iter().map(|f| f.len()).sum::<usize>() + tone.state.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    for field in fields {
        put_bytes(&mut out, field.as_bytes());
    }
    put_bytes(&mut out, &tone.state);
    let sum = fnv1a64(&[&out]);
    out.extend_from_slice(&sum.to_le_bytes());
    out
}

/// A bounds-checked reader over a tone file or a container.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize, what: &str) -> Result<&'a [u8], String> {
        let end = self.at.checked_add(n).filter(|&end| end <= self.bytes.len());
        let end = end.ok_or_else(|| format!("truncated: {what} needs {n} bytes at {}", self.at))?;
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }

    fn u16(&mut self, what: &str) -> Result<u16, String> {
        let b = self.take(2, what)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn sized(&mut self, max: usize, what: &str) -> Result<&'a [u8], String> {
        let b = self.take(4, what)?;
        let len = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize;
        if len > max {
            return Err(format!("{what} is {len} bytes, more than {max}"));
        }
        self.take(len, what)
    }

    fn text(&mut self, what: &str) -> Result<String, String> {
        let b = self.sized(MAX_FIELD_BYTES, what)?;
        String::from_utf8(b.to_vec()).map_err(|_| format!("{what} is not UTF-8"))
    }
}

/// Read a tone file. Refuses a wrong magic, a newer version, a truncated or overlong field, trailing
/// bytes and a checksum that does not match, without panicking on any input.
pub(crate) fn decode(bytes: &[u8]) -> Result<Tone, String> {
    let mut r = Reader { bytes, at: 0 };
    if r.take(4, "magic")? != MAGIC {
        return Err("not a BleepLoop tone".to_string());
    }
    let version = r.u16("version")?;
    if version != VERSION {
        return Err(format!("tone version {version}; this build reads {VERSION}"));
    }
    let format = r.text("format")?;
    let path = r.text("path")?;
    let id = r.text("id")?;
    let name = r.text("name")?;
    let state = r.sized(MAX_STATE_BYTES, "state")?.to_vec();
    let body = r.at;
    let sum = r.take(8, "checksum")?;
    if r.at != bytes.len() {
        return Err(format!("{} bytes after the checksum", bytes.len() - r.at));
    }
    let want = fnv1a64(&[&bytes[..body]]);
    if sum != want.to_le_bytes() {
        return Err("checksum mismatch: the file is corrupt".to_string());
    }
    if format != "clap" && format != "vst3" {
        return Err(format!("unknown plugin format {format:?}"));
    }
    Ok(Tone { identity: ToneIdentity { format, path, id }, name, state })
}

/// A VST3 tone's state: the component's (`IComponent::getState`) and the edit controller's
/// (`IEditController::getState`, empty when there is none or it keeps nothing).
pub(crate) fn encode_vst3(component: &[u8], controller: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(14 + component.len() + controller.len());
    out.extend_from_slice(&VST3_MAGIC);
    out.extend_from_slice(&VST3_VERSION.to_le_bytes());
    put_bytes(&mut out, component);
    put_bytes(&mut out, controller);
    out
}

/// The component's and the controller's states from a VST3 tone's state.
pub(crate) fn decode_vst3(bytes: &[u8]) -> Result<(&[u8], &[u8]), String> {
    let mut r = Reader { bytes, at: 0 };
    if r.take(4, "VST3 magic")? != VST3_MAGIC {
        return Err("not a VST3 tone".to_string());
    }
    let version = r.u16("VST3 version")?;
    if version != VST3_VERSION {
        return Err(format!("VST3 tone version {version}; this build reads {VST3_VERSION}"));
    }
    let component = r.sized(MAX_STATE_BYTES, "component state")?;
    let controller = r.sized(MAX_STATE_BYTES, "controller state")?;
    if r.at != bytes.len() {
        return Err(format!("{} bytes after the controller state", bytes.len() - r.at));
    }
    Ok((component, controller))
}

/// The tone store: `tones/` in the app-local data folder (a probe's run has a profile of its own).
#[derive(Clone, Debug)]
pub(crate) struct ToneStore {
    dir: PathBuf,
}

impl ToneStore {
    pub(crate) fn new(dir: PathBuf) -> ToneStore {
        ToneStore { dir }
    }

    fn path(&self, identity: &ToneIdentity) -> PathBuf {
        self.dir.join(identity.file_name())
    }

    /// The tone stored for `identity`: `Ok(None)` when there is none, `Err` when the file cannot be
    /// read, is corrupt or belongs to another plugin.
    pub(crate) fn load(&self, identity: &ToneIdentity) -> Result<Option<Tone>, String> {
        let path = self.path(identity);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let tone = decode(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        if &tone.identity != identity {
            return Err(format!("{} holds the tone of {} ({})", path.display(), tone.name, tone.identity.path));
        }
        Ok(Some(tone))
    }

    /// Store a tone file's bytes (checked first) under the identity they carry; returns the tone.
    pub(crate) fn save_encoded(&self, bytes: &[u8]) -> Result<Tone, String> {
        let tone = decode(bytes)?;
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("{}: {e}", self.dir.display()))?;
        write_atomic(&self.path(&tone.identity), bytes)?;
        Ok(tone)
    }
}

/// Write `bytes` to a temporary file beside `path`, flush it to disk, then rename it over `path`: a
/// reader sees the old file or the new one, never half of either. The temporary name is unique per
/// write, so two owners saving one plugin's tone at once cannot interleave (the last rename wins).
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_extension(format!("tmp-{}-{}", std::process::id(), NEXT.fetch_add(1, Relaxed)));
    let written = std::fs::File::create(&tmp).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()
    });
    let renamed = written.and_then(|()| std::fs::rename(&tmp, path));
    renamed.map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

/// When the next debounced save is due: every change restarts the quiet period.
#[derive(Default, Debug)]
pub(crate) struct Debounce {
    since: Option<Instant>,
}

impl Debounce {
    pub(crate) fn touch(&mut self, now: Instant) {
        self.since = Some(now);
    }

    /// A change is waiting to be saved.
    pub(crate) fn dirty(&self) -> bool {
        self.since.is_some()
    }

    /// The last change is `SAVE_QUIET` old.
    pub(crate) fn due(&self, now: Instant) -> bool {
        self.since.is_some_and(|since| now.saturating_duration_since(since) >= SAVE_QUIET)
    }

    pub(crate) fn clear(&mut self) {
        self.since = None;
    }
}

/// Where a loaded plugin's tone lives: the store and the plugin's identity.
#[derive(Clone, Debug)]
pub(crate) struct ToneBinding {
    pub(crate) store: ToneStore,
    pub(crate) identity: ToneIdentity,
}

/// An engine-mode owner's tone bookkeeping. `edited` is raised wherever a change is seen off the owner
/// thread (a host parameter set, a CLAP unit's output events on the audio thread, a VST3 `performEdit`;
/// an atomic store only); the owner polls it once per turn. Without a binding (a test, no data folder)
/// the keeper keeps nothing and a save only hands the tone back.
pub(crate) struct ToneKeeper {
    binding: Option<ToneBinding>,
    edited: Arc<AtomicBool>,
    debounce: Debounce,
    /// A session import replaced this plugin's stored tone and the slot reloads to apply it: this
    /// load's state must not be written over it.
    superseded: bool,
}

impl ToneKeeper {
    pub(crate) fn new(binding: Option<ToneBinding>, edited: Arc<AtomicBool>) -> ToneKeeper {
        ToneKeeper { binding, edited, debounce: Debounce::default(), superseded: false }
    }

    /// The flag a change raises (the unit and the component handler hold it too).
    pub(crate) fn edited(&self) -> Arc<AtomicBool> {
        self.edited.clone()
    }

    /// The state to restore at load: `Ok(None)` when nothing is stored.
    pub(crate) fn stored(&self) -> Result<Option<Vec<u8>>, String> {
        match &self.binding {
            Some(b) => Ok(b.store.load(&b.identity)?.map(|tone| tone.state)),
            None => Ok(None),
        }
    }

    /// An owner-thread change (a CLAP `mark_dirty` or params rescan, a VST3 re-list).
    pub(crate) fn note_change(&mut self, now: Instant) {
        self.debounce.touch(now);
    }

    /// Owner turn: fold in what `edited` saw; true when a debounced save is due now.
    pub(crate) fn poll(&mut self, now: Instant) -> bool {
        if self.edited.swap(false, Acquire) {
            self.debounce.touch(now);
        }
        !self.superseded && self.debounce.due(now)
    }

    /// Whether a change is waiting to be saved (an unload saves it first).
    pub(crate) fn dirty(&mut self) -> bool {
        if self.edited.swap(false, Acquire) {
            self.debounce.touch(Instant::now());
        }
        !self.superseded && self.debounce.dirty()
    }

    /// Stop saving for the rest of this load (`superseded`).
    pub(crate) fn supersede(&mut self) {
        self.superseded = true;
        self.debounce.clear();
    }

    /// Save now: `take` asks the plugin named `name` for its state (`Ok(None)`: it keeps none). The
    /// store gets the tone (unless this load is superseded) and the tone file's bytes come back, empty
    /// for a plugin that keeps no state. The pending change is settled either way, so a failing save
    /// waits for the next change rather than retrying every turn.
    pub(crate) fn save(
        &mut self,
        name: &str,
        take: impl FnOnce() -> Result<Option<Vec<u8>>, String>,
    ) -> Result<Vec<u8>, String> {
        self.debounce.clear();
        let Some(state) = take()? else { return Ok(Vec::new()) };
        if state.len() > MAX_STATE_BYTES {
            return Err(format!("the plugin's state is {} bytes, more than {MAX_STATE_BYTES}", state.len()));
        }
        let identity = match &self.binding {
            Some(b) => b.identity.clone(),
            None => ToneIdentity { format: String::new(), path: String::new(), id: String::new() },
        };
        let name: String = name.chars().take(256).collect();
        let bytes = encode(&Tone { identity, name, state });
        if let (Some(b), false) = (&self.binding, self.superseded) {
            b.store.save_encoded(&bytes)?;
        }
        Ok(bytes)
    }
}

/// Test-only: a fresh folder under the system temp dir, removed on drop (the store tests and the
/// engine-mode fixtures keep their tones there).
#[cfg(test)]
pub(crate) struct TempDir(pub(crate) PathBuf);

#[cfg(test)]
impl TempDir {
    pub(crate) fn new(tag: &str) -> TempDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!("lf-tone-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Relaxed)));
        let _ = std::fs::remove_dir_all(&dir);
        TempDir(dir)
    }

    /// A binding to a store in this folder, for `identity`.
    pub(crate) fn binding(&self, identity: ToneIdentity) -> ToneBinding {
        ToneBinding { store: ToneStore::new(self.0.clone()), identity }
    }
}

#[cfg(test)]
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> ToneIdentity {
        ToneIdentity {
            format: "vst3".to_string(),
            path: r"C:\Program Files\Common Files\VST3\FabFilter Pro-Q 3.vst3".to_string(),
            id: "72c4db717a4d459aa97e3e9e3e5c5a8b".to_string(),
        }
    }

    fn tone(state: &[u8]) -> Tone {
        Tone { identity: identity(), name: "Pro-Q 3".to_string(), state: state.to_vec() }
    }

    #[test]
    fn an_identity_hashes_to_the_same_file_name_on_every_run() {
        // Fixed inputs, fixed names: a change here orphans every tone players have saved.
        assert_eq!(fnv1a64(&[b""]), 0xcbf2_9ce4_8422_2325, "FNV-1a 64 offset basis");
        assert_eq!(fnv1a64(&[b"a"]), 0xaf63_dc4c_8601_ec8c, "the published FNV-1a 64 of \"a\"");
        assert_eq!(identity().file_name(), "e20f937c13804113.tone");
        let clap = ToneIdentity { format: "clap".into(), path: r"C:\x.clap".into(), id: "org.x".into() };
        assert_eq!(clap.file_name(), "a77f40f9ad871724.tone");
        // The separators keep a moved boundary from hashing the same.
        let shifted = ToneIdentity { format: "clap".into(), path: r"C:\x.clapo".into(), id: "rg.x".into() };
        assert_ne!(shifted.file_name(), clap.file_name());
    }

    #[test]
    fn a_tone_file_round_trips() {
        let t = tone(&[0, 1, 2, 250, 255]);
        let bytes = encode(&t);
        assert_eq!(&bytes[..4], b"BLTN");
        assert_eq!(decode(&bytes).unwrap(), t);
        let empty = tone(&[]);
        assert_eq!(decode(&encode(&empty)).unwrap(), empty, "a plugin may keep an empty state");
    }

    #[test]
    fn a_corrupt_truncated_or_foreign_file_is_refused_without_panic() {
        let bytes = encode(&tone(b"some plugin state"));
        for cut in 0..bytes.len() {
            assert!(decode(&bytes[..cut]).is_err(), "truncated at {cut}");
        }
        for at in 0..bytes.len() {
            let mut flipped = bytes.clone();
            flipped[at] ^= 0x40;
            assert!(decode(&flipped).is_err(), "a flipped byte at {at}");
        }
        let mut long = bytes.clone();
        long.push(0);
        assert!(decode(&long).is_err(), "trailing bytes");
        // A length prefix claiming more than the limit is refused before anything is sized from it.
        let mut huge = MAGIC.to_vec();
        huge.extend_from_slice(&VERSION.to_le_bytes());
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode(&huge).unwrap_err().contains("more than"));
        assert!(decode(b"RIFF....WAVE").is_err(), "not a tone at all");
    }

    #[test]
    fn a_vst3_container_round_trips_and_refuses_garbage() {
        let bytes = encode_vst3(b"component", b"controller");
        assert_eq!(decode_vst3(&bytes).unwrap(), (&b"component"[..], &b"controller"[..]));
        let none = encode_vst3(b"only the component", b"");
        assert_eq!(decode_vst3(&none).unwrap(), (&b"only the component"[..], &b""[..]));
        for cut in 0..bytes.len() {
            assert!(decode_vst3(&bytes[..cut]).is_err(), "truncated at {cut}");
        }
        assert!(decode_vst3(b"a CLAP plugin's state").is_err());
        let mut long = bytes.clone();
        long.push(7);
        assert!(decode_vst3(&long).is_err(), "trailing bytes");
    }

    #[test]
    fn the_store_round_trips_replaces_atomically_and_refuses_a_bad_file() {
        let dir = TempDir::new("store");
        let store = ToneStore::new(dir.0.clone());
        assert_eq!(store.load(&identity()).unwrap(), None, "nothing stored yet");

        let first = tone(b"first");
        store.save_encoded(&encode(&first)).unwrap();
        assert_eq!(store.load(&identity()).unwrap(), Some(first));
        let second = tone(b"second, longer than the first");
        store.save_encoded(&encode(&second)).unwrap();
        assert_eq!(store.load(&identity()).unwrap(), Some(second), "replaced");
        let names: Vec<_> = std::fs::read_dir(&dir.0).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, [identity().file_name().as_str()], "one file, no temporary left behind");

        assert!(store.save_encoded(b"garbage").is_err(), "a file that does not decode is never stored");
        let path = dir.0.join(identity().file_name());
        let good = std::fs::read(&path).unwrap();
        std::fs::write(&path, &good[..good.len() / 2]).unwrap();
        assert!(store.load(&identity()).is_err(), "a truncated file is an error, not a tone");

        // A file under this identity's name that carries another plugin's identity.
        let mut other = tone(b"x");
        other.identity.id = "another".to_string();
        std::fs::write(&path, encode(&other)).unwrap();
        assert!(store.load(&identity()).unwrap_err().contains("holds the tone of"));
    }

    #[test]
    fn the_debounce_waits_for_quiet() {
        let t0 = Instant::now();
        let mut d = Debounce::default();
        assert!(!d.dirty() && !d.due(t0 + SAVE_QUIET * 10), "nothing changed");
        d.touch(t0);
        assert!(d.dirty());
        assert!(!d.due(t0 + SAVE_QUIET - Duration::from_millis(1)));
        assert!(d.due(t0 + SAVE_QUIET));
        d.touch(t0 + Duration::from_secs(1));
        assert!(!d.due(t0 + SAVE_QUIET), "a later change restarts the wait");
        assert!(d.due(t0 + Duration::from_secs(1) + SAVE_QUIET));
        d.clear();
        assert!(!d.dirty() && !d.due(t0 + SAVE_QUIET * 10));
    }

    #[test]
    fn a_keeper_saves_after_quiet_and_never_over_an_import() {
        let dir = TempDir::new("keeper");
        let binding = ToneBinding { store: ToneStore::new(dir.0.clone()), identity: identity() };
        let edited = Arc::new(AtomicBool::new(false));
        let mut keeper = ToneKeeper::new(Some(binding.clone()), edited.clone());
        let t0 = Instant::now();
        assert!(!keeper.poll(t0), "nothing changed");
        assert_eq!(keeper.stored().unwrap(), None);

        edited.store(true, Relaxed); // a host parameter set, from a command thread
        assert!(!keeper.poll(t0));
        assert!(keeper.dirty());
        assert!(keeper.poll(t0 + SAVE_QUIET), "due once the change is quiet");
        let bytes = keeper.save("Pro-Q 3", || Ok(Some(b"state v".to_vec()))).unwrap();
        assert_eq!(decode(&bytes).unwrap(), tone(b"state v"));
        assert!(!keeper.dirty() && !keeper.poll(t0 + SAVE_QUIET * 3), "saved: nothing pending");
        assert_eq!(keeper.stored().unwrap().as_deref(), Some(&b"state v"[..]), "the next load finds it");

        // A session import wrote its tone and the slot reloads: this load never writes over it.
        binding.store.save_encoded(&encode(&tone(b"imported"))).unwrap();
        keeper.supersede();
        edited.store(true, Relaxed);
        assert!(!keeper.poll(t0 + SAVE_QUIET * 9) && !keeper.dirty());
        let fresh = keeper.save("Pro-Q 3", || Ok(Some(b"state w".to_vec()))).unwrap();
        assert_eq!(decode(&fresh).unwrap().state, b"state w", "an export still gets this load's state");
        assert_eq!(keeper.stored().unwrap().as_deref(), Some(&b"imported"[..]), "the store keeps the import");

        assert!(keeper.save("x", || Ok(Some(vec![0; MAX_STATE_BYTES + 1]))).is_err(), "an oversized state is refused");
        assert_eq!(keeper.save("x", || Ok(None)).unwrap(), Vec::<u8>::new(), "a plugin that keeps no state");
        edited.store(true, Relaxed);
        let mut fresh = ToneKeeper::new(Some(binding), edited.clone());
        assert!(fresh.dirty());
        assert!(fresh.save("x", || Err("the plugin refused".to_string())).is_err());
        assert!(!fresh.dirty(), "a failed save waits for the next change");
    }
}
