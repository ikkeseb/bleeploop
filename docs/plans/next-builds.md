# Next builds: specified, not built (2026-10-04)

A working document: each section is a reviewed spec a writer can build from. When a section lands,
fold what still binds into the briefings and delete the section; delete the file when it is empty.
Line numbers are as of commit `a43e9bc4` and move.

Order: D21 slice 3b, then pan (it rests on D21's `LaneMix`). D25 touches no D21 file and runs
beside them in its own worktree. Delayed COPY waits on the owner (STATUS D29). Slice 3a landed: the
feed's `Event::Mix` and the host's projection (rules: `src-tauri/src/engine_io/settings.rs` header,
the lf-engine briefing).

## D21 slice 3b: the UI follows the applied mix

Revised after a second model family's read of the first draft (release-clearing snapped back;
equality during a drag ended its protection; toggles read a stale base; autosave could keep an
A-B-A fingerprint).

1. `src/ui/state/engine-store.ts` keeps an authoritative per-lane mix, written only from the feed
   (`Event::Mix`, and a reset frame: its settings for lanes it has no Mix for, then its Mix events).
   `copyLaneMix`, `clearLaneMix`, mix-from-`Muted` and the import's local write of the loaded mix go;
   `Cleared` keeps its recovery bookkeeping (the clear token).
2. Every UI-set mix value but MUTE (volume, DUB FEEDBACK, FX bypass, FX params) shows an overlay when
   one is set, else the authoritative value. A gesture (pointer, keyboard, a select's change, a
   bypass press) writes the overlay from the shown value and sends. The overlay holds through the
   whole gesture. After the gesture it stays until a Mix whose value equals it (both sides
   normalised: the setters' clamps, `Math.fround`), a send failure of that gesture's own command
   (tagged, so an older failure never clears a newer overlay), or a cancel: the lane's `Cleared`, a
   `Copied` into it, an import or recovery load, a reset frame, the lane going EMPTY, the control's
   disposal. Only the UI writes these values, so an equal echo is a safe acknowledgement. No timeout.
3. MUTE sends the engine's toggle (`ActionOn(lane, Mute)`, as a pedal does) and shows the
   authoritative value: two quick presses are two toggles.
4. The `SessionSource` mix getters (and the plain mirrors, the stage view) read the authoritative
   values. Autosave records as saved the fingerprint of the snapshot it persisted (its mix), not the
   one it inspected before, so A, then B pinned, then A again stays dirty.
5. The browser fake (`src/platform/host.web.ts`) echoes a Mix after it applies a mix command or a
   scripted COPY, CLEAR or MUTE, asynchronously, from an immutable copy. Two probe seams: hold the
   echoes (applied, unreported) and hold or refuse the application (unapplied). Probe cases: a
   refused gesture returns to the applied value; an equal echo during a drag keeps the drag's
   protection; an unequal Mix during a drag does not move the fader; release before the echo shows
   no snap-back; two quick MUTE presses; the A-B-A autosave case. `verify/probes/engine-seam.mjs`'s
   COPY-derived volume moves to the Mix model.
6. After 3b, D21's row leaves STATUS (the fader's feel is a not-heard line); the two load-protocol
   races stay ignored as their own question.

## Per-lane pan (the owner's idea, 2026-09-18; the owner said build it)

A reader's spec against `8fb58f5c`; re-read the final D21 shapes first.

- Law: centre-normalised constant power, `L = sqrt(2) cos(t)`, `R = sqrt(2) sin(t)`,
  `t = (p + 1) pi / 4`. Centre is `[1, 1]`, so old sessions play exactly as now; hard pan is +3 dB
  in one channel and silence in the other.
- Where: the lane's direct output after its FX (`effects.rs`, after `process_fading`); the shared
  reverb send stays unpanned; recording, stems, undo buffers and waveforms are untouched.
- Smoothing: the engine's existing f64 glide with its per-sample snap threshold (`lib.rs`), so a
  move settles exactly; gains computed from the reached position and cached at every settled
  position (not only centre); exact zero at hard pan; centre keeps today's output bits. Measure the
  perf bars stationary and moving.
- Engine and wire: `Command::SetPan(lane, f32)` (-1..1, non-finite gives centre), `LaneMix.pan`
  (default 0). Construction, CLEAR, COPY and a load SEED the position (no glide from an old one);
  only `SetPan` glides. The feed, snapshots, loads, the wet master and the projection carry the
  target, not the moving position. Round trip required: `CompactMix` and its widening, Mix
  comparison, the host projection and replay (a rebuild), reset frames, both wire codecs, snapshot
  pin, load, archive and import, autosave. `session.json` gets an optional `pan` (absent gives 0,
  out of range clamps, a non-number refuses) with `formatVersion` 1 unchanged; autosave's
  fingerprint includes it. The dry fallback export mixes mono today (`src/session/export.ts`): give
  it a stereo dry mix with the same law, identical to today's output at centre.
- UI: a 44x24 px range (-100..100, a pointer-only detent of 2 around 0; arrow keys step across it)
  beside the volume fader, in one mix row of the lane's right cluster (cluster width
  `clamp(240px, 20vw, 264px)`); readout `L 30` / `C` / `R 30`; double-click, Alt-click and the 0 key
  (focused only) centre it; Home and End are hard left and right; `aria-valuetext`; disabled on an
  EMPTY lane as volume is; no MIDI learn yet. It follows 3b's overlay rule.
- Tests: the law and centre bits; glide continuity at every block size; routing after FX with the
  reverb unchanged; COPY and CLEAR; normalisation; snapshot pin and wet master; wire; a legacy
  session imports centred; a `lane-pan` probe for gestures, layout at 1000x700, 1280x820 and
  1920x1080, and accessibility. One not-heard line in STATUS when built.

## D25: the WASAPI join's trim rule

The owner's decision (2026-10-04): skip the rerun with other apps closed and change the trim rule;
the setpoint stays. A second model family's read: the settled setpoint is unchanged, but audio the
old rule dropped stays queued until the controller drains it, and the join aligns a take by the
setpoint, so a take in that window could be accepted but late. The replay measures it.

- Rule: in `src-tauri/src/engine_io/pipes.rs` `PullPipe::pull_piece`, trim when the fill exceeds
  `2 * target + ceil(n * in_rate / out_rate)`, a nominal pull allowance (not the exact input the
  resampler takes). Same drop-to-target action, controller state and counters. Share output builds
  the same pipe: it is in scope.
- Proof without hardware: bounded fixtures of the logged join traces (the `j` lines of the
  2026-10-03 `native:engine` runs: nine trimming or starving opens; no whole logs, no local paths)
  under `src-tauri/src/engine_io/fixtures/wasapi-join/` (not yet built), replayed by a `cfg(test)`
  module through the real pipe as push and pull operations (fill evolves; the old rule must first
  reproduce the traced outcomes), reporting each open's excess queued input and how long it lasts.
  Two seeded witnesses must be red under the old rule and green under the new: a fill of 2508 frames with a 480-frame pull, and 2609 with a late 960-frame double pull. Threshold tests
  (480 frames at 48 kHz: 2880 does not trim, 2881 does; 960; unequal rates); a stalled puller still
  trims once; the open that only starves still starves (this rule does not fix starves); a take over
  a tolerated burst commits (the join helper in `callback.rs`'s tests).
- Rig after (the seat, on the PC): `pnpm native:engine --cycle=asio64,wasapi --seconds=3
  --switches=60 --swaps=0 --mute=1 --amp= --proq=` until at least 85 WASAPI opens, and the no-ASIO
  cycle (`--backend=wasapi --buffer=default --cycle=wasapi`) until at least 43. Baseline: of 85, 8
  opens trimmed, 2 starved, 9 faulty; of 43, 1. Record the counts in `docs/VERIFY.md` and the native
  briefing, and close or narrow D25.

## Delayed COPY: waits on the owner (STATUS D29)
