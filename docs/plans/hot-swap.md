# Plan: hot plugin swap (issue #1), building on a branch

Open plan. The owner approved (2026-10-26) building it on the branch `road1/hot-swap` with invariant 4
reworded as below; it does not land on `main` until the owner has heard the crossfade. Once landed or
dropped, what still binds folds into the briefings and this file goes.

## The ask

Issue #1 (a tester): a heavy plugin swap mid-gig takes seconds. The owner's published plan: load the
next plugin while the current one keeps playing, then crossfade; MIDI Program Change as a trigger; a
pre-loaded pool only if that still is not fast enough. The tester has not said which plugins they swap
or whether between songs or mid-song.

## Today

A swap is cold: the slot goes off live (an effect's tail still rings until the removal fade), the old
unit is removed and torn down on its owner thread (Archetype's release alone is about 5 s), the new
one loads on a new owner thread and crossfades in from bypass, and GO LIVE waits for a press (a tone
reload re-arms it). Path: `src/ui/state/instrument.ts` (`doSelectPlugin`, `unloadSlotPlugin`) →
`src-tauri/src/engine_io/plugins.rs` → `src-tauri/src/host/engine_slot.rs` →
`src-tauri/src/engine_io/slot_host.rs` → `src-tauri/crates/lf-engine/src/slots.rs`.

## Design (step one: different plugin files only)

- **Engine:** a `Replace` slot message. The slot keeps an `outgoing` unit beside `unit` and crossfades
  `(1−t)·old + t·new` over a counted frame ramp, each side at its own gain and fed by its own kind
  (an instrument gets silence, an effect the gated input), so neither the bypass term nor the dry
  guitar is heard. The live flag and gate are untouched. Every slot message and handback carries the
  owner's tag, and the callback answers each `Replace` Accepted or Rejected with its reason; it never
  drops a unit. Buffers for the second unit are allocated at rack build.
- **Staging before the fade:** the candidate starts processing muted on the audio thread while the old
  one plays (CLAP `start_processing` and VST3 `setProcessing` run on the first process call), at least
  through its reported latency, and commits only once it started without a fault. A candidate that
  refuses to start goes back alone; the old plays on. Without this a delay-type plugin would fade in
  from an empty history.
- **One swap per slot at a time:** a native coordinator per slot runs Preparing → Fading → Retiring to
  a confirmed end (owner joined, module released). A later pick is stored last-wins and starts after
  that. At most two units per slot are resident. Cancellation is allowed only before commit; after it,
  the caller reads the actual result.
- **Ownership:** each owner token keeps a durable state (active, retiring, abandoned, released); a
  retiring owner's restart or eviction never reinstalls its unit, and a Remove carries its tag so a
  late one cannot remove a newer unit.
- **Instance generation** on every plugin event, mutating request and reply (parameters, editor, tone
  take), so the UI ignores the old instance's late events and native refuses stale requests; a tone
  take returns the identity it read (`src/ui/state/slot-tones.ts`).
- **No automatic cold fallback:** a refused or transient hot swap keeps the old plugin and reports
  busy or failed. A cold swap is an explicit path, used for the same plugin file (below).
- **Notes:** an explicit policy at commit, released in both the router
  (`src-tauri/src/engine_io/midi/router.rs`) and the rack, or retriggered on the candidate.
- **GO LIVE** during a swap acts on the logical slot, not queued behind the load
  (`src/ui/state/instrument-slots.ts`); commit keeps the last live intent; a kind change follows the
  activated kind, not the scan's.
- **A take across a swap:** monitor and record are specified apart. Default: a latency-changing commit
  waits until the capture ends, while the old plugin plays.
- **Shutdown and update quit:** a closing state refuses new swaps and cancels uncommitted candidates;
  every unit's teardown starts before the bounded wait, inside the existing exit bound.

## Owner decisions

1. **Invariant 4's wording** (`AGENTS.md` and `docs/ARCHITECTURE.md` together), approved for the branch: "Plugin
   lifecycle runs on its owner thread, only while the engine does not hold that instance; the engine
   owns a submitted unit until its tagged handback and never destroys one on the RT thread."
   Format processing start and stop on the processing thread stay as today.
2. **The same plugin file** stays a cold swap (D29's measured GUI binding; the hang for a hot swap is
   inferred), and cold means the earlier instance has ended. A preset change inside one amp-sim
   therefore keeps its gap. Whether that is the tester's real case is their answer on the issue.
3. **Program Change** (later): the slot a PC addresses, where the PC → plugin list is edited, what an
   empty entry does. `src-tauri/src/engine_io/midi/parse.rs` drops 2-byte messages today.

## Proofs

Engine render tests (the crossfade exact at several block sizes, no bypass term, tags and handback,
Remove and evict mid-swap, a candidate refusing to start); `engine_io` tests on the test device and the
Windows fixture plugins for every commit boundary (refused Replace, timeout after claim, eviction with
both units, cancel before and after commit, rate change); a browser probe for the UI path;
`native:swap` and `native:engine --swaps --tone=1` on the PC. By ear only: whether the fade between
two amp-sims sounds clean, loudness jumps, and CPU with two heavy plugins.
