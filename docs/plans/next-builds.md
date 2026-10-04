# Next builds: specified, not built (2026-10-04)

A working document: each section is a reviewed spec a writer can build from. When a section lands,
fold what still binds into the briefings and delete the section; delete the file when it is empty.
Line numbers are as of commit `a43e9bc4` and move.

Order: D21 slice 3a, then 3b (same files), then pan (it rests on D21's `LaneMix`). D25 touches no
D21 file and can run beside either in its own worktree. Delayed COPY waits on the owner (STATUS D29).

## D21 slice 3a: the feed carries the applied mix; the host's lane mix becomes a projection

Landed so far: `LaneMix` (lf-engine `api.rs`), `Looper::mix`; snapshots carry the mix taken at their
pin; loads carry and apply it on the swap frame; a block's due commands apply before its session job;
the host records a load's mix in its settings memory. Design reviewed by a second model family.

1. `Event::Mix { frame, lane, mix }` pushed from `Looper::publish` when a lane's applied mix differs
   from the last one DELIVERED. `Feed::push` (`engine.rs`) returns whether the ring took the event, and
   `publish` marks a lane's info and mix published only on success, so a full ring retries next
   publish (fix `Event::Lane`, which has the same bug, the same way). Publish runs per split inside a
   block; it emits only on change. Measure `size_of::<Event>()` and the perf bars (`tests/perf.rs`,
   ignored) before and after; past about 2x, carry the mix compactly (f32 params).
2. The wire and the TS decoder accept `Mix` in the same change (an unknown event makes
   `src/platform/engine-wire.ts` drop the whole frame). The UI may ignore it until 3b.
3. `src-tauri/src/engine_io/settings.rs`: once an engine exists, lane mix entries are a projection
   of drained `Event::Mix`, tagged with the engine generation; accepted lane-mix commands are no
   longer recorded then. Before the first engine (or a failed first open) accepted commands stay as
   bootstrap, replayed into the first engine and superseded per lane by its first Mix. `Copied`,
   `Cleared` and `Muted` stop patching lane mix (`feed.rs`). Master volume and mute and every
   non-lane setting keep today's memory. `Settings::loaded` (slice 2) goes with the projection; it
   closes the window where a setting sent just after a load is overwritten in the memory.
4. Rebuild handoff (`owner.rs`, where `Ends` is replaced): drain the old engine's event ring into
   the projection before replacing it, under the replay's lock order; a Mix from a replaced
   generation never overwrites the new one's. The replay into a new engine is the projection.
5. Reset frames carry the projection's lane mixes.
6. Tests: one Mix per change; overflow then inactivity still delivers; a rebuild with a Mix still in
   the old ring keeps it; a late Mix from an old generation is refused; a reset after COPY, CLEAR
   and an action MUTE carries the applied mix; a bootstrap setting reaches the first engine.
   Mutations: publish marking a failed push delivered; the rebuild not draining the old ring.

## D21 slice 3b: the UI follows the applied mix

1. `src/ui/state/engine-store.ts` keeps an authoritative per-lane mix from `Event::Mix` and reset
   frames; a control shows its gesture overlay when one is set, else the authoritative value.
2. A gesture writes the overlay and sends the command. The overlay clears on gesture end (pointer
   up, cancel, blur), on a send failure, and when a Mix equal to it arrives. A pedal or MIDI change
   updates the authoritative value during a drag. Keyboard edits of a fader (`Looper.tsx`) count as
   gestures. No timeout decides which command an echo acknowledges.
3. `copyLaneMix`, `clearLaneMix` and mix-from-`Muted` go; `Cleared` keeps its recovery bookkeeping
   (the clear token).
4. The `SessionSource` mix getters, and so autosave's fingerprint (`src/session/autosave.ts`), read
   the authoritative values: a delayed or refused command never marks an unapplied mix as saved.
5. The browser fake (`src/platform/host.web.ts`) echoes a Mix after it applies a mix command, and a
   probe can withhold it to test reconciliation. `verify/probes/engine-seam.mjs` asserts a
   COPY-derived volume: update it. New probe cases: a refused gesture returns to the applied value; a
   pedal MUTE during a drag; an echo clears the overlay.
6. After 3b, D21's row leaves STATUS (the fader's feel is a not-heard line); the two load-protocol
   races stay ignored as their own question.

## Per-lane pan (the owner's idea, 2026-09-18; the owner said build it)

A reader's spec against `8fb58f5c`; re-read the final D21 shapes first.

- Law: centre-normalised constant power, `L = sqrt(2) cos(t)`, `R = sqrt(2) sin(t)`,
  `t = (p + 1) pi / 4`. Centre is `[1, 1]`, so old sessions play exactly as now; hard pan is +3 dB
  in one channel and silence in the other.
- Where: the lane's direct output after its FX (`effects.rs`, after `process_fading`); the shared
  reverb send stays unpanned; recording, stems, undo buffers and waveforms are untouched.
- Smoothing: a one-pole glide of the position (10 ms), per sample, gains computed from the reached
  position; a settled-centre fast path keeps today's output bits.
- Engine and wire: `Command::SetPan(lane, f32)` (-1..1, non-finite gives centre), `LaneMix.pan`
  (default 0). COPY copies the target and seeds the destination's position; CLEAR resets it;
  snapshots, loads and the wet master carry it; `session.json` gets an optional `pan` (absent gives
  0, out of range clamps, a non-number refuses) with `formatVersion` 1 unchanged; autosave's
  fingerprint includes it. Check whether the dry fallback export mixes mono; if so, give it a
  stereo dry mix that applies pan.
- UI: a 44x24 px range (-100..100, a detent of 2 around 0) beside the volume fader, in one mix row
  of the lane's right cluster (cluster width `clamp(240px, 20vw, 264px)`); readout `L 30` / `C` /
  `R 30`; double-click, Alt-click and the 0 key centre it; `aria-valuetext`; disabled on an EMPTY
  lane as volume is; no MIDI learn yet.
- Tests: the law and centre bits; glide continuity at every block size; routing after FX with the
  reverb unchanged; COPY and CLEAR; normalisation; snapshot pin and wet master; wire; a legacy
  session imports centred; a `lane-pan` probe for gestures, layout at 1000x700, 1280x820 and
  1920x1080, and accessibility. One not-heard line in STATUS when built.

## D25: the WASAPI join's trim rule

The owner's decision (2026-10-04): skip the rerun with other apps closed and change the trim rule;
the setpoint stays (no latency change).

- Rule: in `src-tauri/src/engine_io/pipes.rs` `PullPipe::pull_piece`, trim when the fill exceeds
  `2 * target + ceil(n * in_rate / out_rate)`: the input this pull piece is about to consume is not
  an overrun. Same drop-to-target action and controller state as today. Settled latency is
  unchanged; audio the old rule discarded is kept and the controller drains it.
- Proof without hardware: bounded fixtures of the logged join traces (the `j` lines of the
  2026-10-03 `native:engine` runs: nine trimming or starving opens; no whole logs, no local paths)
  under `src-tauri/src/engine_io/fixtures/wasapi-join/` (not yet built), replayed by a `cfg(test)` module through
  the real pipe. Two seeded witnesses must be red under the old rule and green under the new: a fill
  of 2508 frames with a 480-frame pull, and 2609 with a late 960-frame double pull. Threshold tests
  (480 frames at 48 kHz: 2880 does not trim, 2881 does; 960; unequal rates); a stalled puller still
  trims once; the open that only starves still starves (this rule does not fix starves); a take over
  a tolerated burst commits (the join helper in `callback.rs`'s tests).
- Rig after (the seat, on the PC): `pnpm native:engine --cycle=asio64,wasapi --seconds=3
  --switches=60 --swaps=0 --mute=1 --amp= --proq=` until at least 85 WASAPI opens, and the no-ASIO
  cycle (`--backend=wasapi --buffer=default --cycle=wasapi`) until at least 43. Baseline: of 85, 8
  opens trimmed, 2 starved, 9 faulty; of 43, 1. Record the counts in `docs/VERIFY.md` and the native
  briefing, and close or narrow D25.

## Delayed COPY: waits on the owner (STATUS D29)
