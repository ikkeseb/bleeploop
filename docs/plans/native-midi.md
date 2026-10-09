# Plan: native MIDI replaces Web MIDI

A working document (`AGENTS.md`: deleted when the work lands; what still binds folds into the briefings,
`docs/ARCHITECTURE.md` and `STATUS.md`). The owner's call (D22, 2026-10-08): MIDI input moves from the
WebView's Web MIDI to native Rust, measured before and after. The open thread in `src-tauri/AGENTS.md`
points here. Reviewed before any build by an outside-family counter-case and an adversarial read; their
findings are folded in below.

## Why

MIDI is the one timing-critical input that still crosses the WebView's JavaScript: a message waits for
the UI thread (which also draws the stage view and the waveforms), then an IPC call, then lands at the
next block start (`src/ui/state/midi.ts`, `src/platform/index.ts` `sendEngine`, `engine_io/mode.rs`
`engine_send`: every command `frame: None`). Native input takes the UI thread off that path entirely.

## What existed before the work (verified 2026-10-08)

- **Web MIDI path (shipped).** `midi.ts` listens on every input (no device picker), parses 3-byte
  channel messages only (note on/off, velocity 0 = off, CC1 mod, CC64 sustain, CC123, pitch bend
  ±2 semitones; everything else dropped), runs MIDI learn's consume-first hook
  (`src/app/midi-actions.ts`), then the TS input router (`inputRouter`, removed at the switch), which owned note
  ownership, per-owner sustain and last-moved-wins wheels for **all** note sources and feeds the
  on-screen keyboard's highlights synchronously. Owners are physical: `pointer:<pointerId>`,
  `key:<KeyboardEvent.code>` (`Keyboard.tsx`; two held keys can reach one note after an octave shift),
  `[portId, channel]` for MIDI. Notes go to the one active slot's target (`instrument.ts`
  `routeEngine`, which releases held notes before `SelectInstrument`; a plugin swap routes nowhere
  while it unloads; `engineResync` releases everything on a reload).
- **MIDI learn (shipped, TS).** 25 actions (8 with a lane target; of the 17 global, five send engine
  actions directly, ten are UI facade calls preceded by `Press` and two are stage-view actions with no
  `Press`, `src/app/actions.ts`; a named REC/DUB or HOLD selects its lane first; FIXED, RETAKE and
  AUTO REC are refused in the states `src/ui/looper/gates.ts` names), momentary vs latching, HOLD on a momentary REC/DUB pedal, per-binding 10 s release waits
  checked before learn capture (5faf32a2), the "spent" release after an edit, level-based firing,
  port-gone release. Stored in localStorage `lf.midiLearn`, keyed by the Web MIDI input id, each
  binding carrying its port name. Nothing mirrors it to native, whatever `docs/ARCHITECTURE.md:16`
  says.
- **Native stack (built 2026-09-25, never started).** `src-tauri/src/engine_io/midi/` (midir 0.11,
  WinMM; 44 unit tests on a recorder sink): a 1 s port poller (`lf-midi-ports`), the same parse, a
  learn model, a MIDI-only router (`Owner = (conn, channel)`), a sink into `EngineHost::send`. Bound
  actions are stamped with `FrameClock::press_frame`; notes and wheels are not. While no device runs,
  note-ons and bound actions are dropped and releases pass. Nothing builds a `MidiHost`; no command,
  event or wire type carries any of it.
- **The native learn model is stale:** 9 actions against 25, no lane target, no HOLD, one 1 s release
  tail instead of per-binding 10 s waits, learn capture before the release check. `parse_bindings`
  drops unknown actions, ignores `target` and `hold`, and turns unreadable JSON into an empty list.
- **Two known holes** (`src-tauri/AGENTS.md`): a binding's port occurrence is recounted from the WinMM
  list on every poll (`ports.rs` `port_keys`); a port unplugged and replugged inside one poll keeps its
  dead connection (`diff` compares only `(id, name)`; midir has no liveness signal on WinMM). midir's
  port id (the WinMM device-interface path) is obtained and discarded for matching.
- **The engine.** One command ring, one 64-entry pending table filled at block entry (`engine.rs`
  `take_commands`; the rest waits in the ring, late, never dropped). Due commands apply in admission
  order; an unstamped command lands at the next block start, a stamped one on its frame.
- **The machine.** Windows 11 build 26300 has Windows MIDI Services in-box (`midisrv`, demand-start;
  WinMM goes through `wdmaud2.drv`; loopback and virtual transports registered). No loopMIDI. midir
  cannot create virtual ports on Windows.

## Progress (branch `native-midi`, not merged)

- **Steps 1 to 5 are built and tested on the branch;** step 6, the switch, is built too: the app runs
  native MIDI and the UI's notes through the Rust router, Web MIDI is denied. `pnpm native:midi`
  drives the running app (WASAPI, master muted): the resync, the denied Web MIDI, a PC-key note held
  natively, two owners on one note, blur, a slot switch, a reload releasing at the new page's
  subscribe and cancelling a pending learn. It drives no MIDI port.
- **The ship rule is met** (2026-10-09, loopMIDI, the jam load held unattended by `bench-load`;
  numbers: `docs/VERIFY.md` § MIDI latency benchmark): no note lost or stuck either side, sender to
  applied p99 226–227 ms before and 3.1 ms after, 251–252 ms against 3.1 ms inside UI stalls.
- **Waits on the owner:** the lock-wait measurement under jam load (`LF_LOCK_WAITS`); and a
  controller: whether the stored Web MIDI name equals WinMM's `szPname` (where it differs, an
  imported binding waits with no port and the player assigns it in Audio Settings), the path's
  stability across restart and replug, the interface class a port arrives on, WinMM multi-client,
  the 20 replugs, a jam, and the old pedals firing without relearn.
- **Behaviour changes the reviews accepted:** a plugin swap routes nowhere while it unloads (the
  shipped app played the slot's built-in synth then); the UI's input goes one IPC batch at a time
  (one round trip per batch, unmeasured; native MIDI does not take this path); a press during a
  device gap is refused and told, not queued; the learn row's ASSIGN control is new (taste).
- **Known residuals (no fix yet; each needs an IPC call stalled past its 2 s bound, or a clock):** a
  note-on whose batch timed out can land after its release and stick until a blur, a slot switch or
  a panic; a blur owed after a failure, answered late, can release a hold made after it; a page whose
  `performance.timeOrigin` reads older than its predecessor's (the wall clock moved back and the
  WebView restarted) is refused until the app restarts. Next check: log `input_send` round trips over
  a jam to see whether a 2 s stall ever happens.

## Step 0 findings (2026-10-09, no device plugged in, nothing installed)

- **No measurement port without an install.** Enumerating WinMM starts `midisrv` (demand-start; it
  was stopped, it runs after a `midiInGetNumDevs`). WinMM then lists 0 inputs and 1 output (Microsoft
  GS Wavetable Synth). The service's own loopbacks (`SWD\MIDISRV\MIDIU_DIAG_LOOPBACK_A`/`_B`, "Service
  Test Loopback A/B") come up with the service but expose only the MIDI 2.0 endpoint interface class
  `{e7cce071-3c03-423f-88d3-f1045d02552b}`, no WinMM port. Creating a loop endpoint (the
  `MIDIU_LOOP_TRANSPORT` transport is present) needs the Windows MIDI Services SDK runtime or tools, an
  install; the session runs at medium integrity. So the measurement port is loopMIDI or the MIDI
  Services tools, both the owner's to approve at the PC: step 6 is built but cannot merge until then.
- **Web MIDI ids carry no device identity** (Chromium source on `main`, 2026-10-09, read, not run):
  WebView2's Chromium uses the WinMM backend unless the `MidiManagerWinrt` feature is on (off by
  default; whether WebView2 turns it on is unknown). An input's `MIDIInput.id` is `"input-<N>"`, N
  the order in which that run's MIDI manager first saw the port (`midi_manager_win.cc` `set_index`,
  "TODO: Use hashed ID"); Blink passes it through unchanged. A replug inside one run matches the port
  back by `wMid`, `wPid`, `vDriverVersion` and `szPname`; a restart renumbers from the WinMM order, so
  `input-2` can name another device on the next run. Two devices with identical caps show as one.
  The stored `portName` is therefore the strongest identity a legacy binding holds (that it equals
  WinMM's `szPname` is inferred, to confirm with a device).
- **midir's port id** is the WinMM device-interface path (`DRV_QUERYDEVICEINTERFACE`, midir 0.11
  `backend/winmm`); several ports of one device share it, so a binding needs a discriminator beside it.
  Its stability across restart and replug, the interface class a WinMM port arrives on under
  `wdmaud2`, and whether WinMM input is multi-client there are unknown: no input port exists to test
  (the owner's check with a controller, `STATUS.md` when the switch ships).
- **Not run:** the DEV app's `[diag]` Web MIDI list (WinMM has no input to show).

## Decided

1. **One router, in Rust, for every note source.** Moving only MIDI would leave two routers sending
   `NoteOn`/`NoteOff`/wheels to one engine with no shared ownership. The Rust router's owner widens to
   `Midi(conn, channel) | Ui(epoch, owner)`, where `owner` is the physical id the UI uses today and
   `epoch` the document's input epoch (answered by its `midi_subscribe`, first thing in boot; the
   switch's review moved it off `host_init`'s `frontendEpoch`, which comes late). `Keyboard.tsx` keeps its gesture handling (pointer capture,
   repeat suppression, press-time note mapping, text-entry guards, blur and unmount cleanup) and forwards
   key events through a thin adapter in place of `input-router.ts`'s musical logic. The on-screen
   keyboard keeps a local overlay for pointer and PC holds (highlight on press, as today); MIDI holds
   reach it through native state (a held-note set published without the router's lock) and correct it
   on resync. "Held" means physically held, not sustained.
2. **Native owns MIDI end to end:** ports, parse, learn, bindings, routing, the note target. The
   WebView keeps the learn UI, the device list and the toasts, driven by native events on their own
   channel (never the feed, which is held while the folder dialog is open) and commands through a new
   `src/platform` interface (invariant 7). Web MIDI is not opened in the app: one path per run, no
   fallback. The Web MIDI permission is explicitly denied, and a guard keeps `requestMIDIAccess` out of
   `src/`.
3. **One ordered path, the existing ring.** Every router output, `SelectInstrument` and "route nowhere"
   included, goes through the router under its lock and then `EngineHost::send` (which records the
   remembered settings), so a target switch and a note are applied in the order they happened. A UI
   slot pick becomes a router command; the router releases held notes, then selects. No second ring:
   it would break that order and the settings replay, and the lock contention it would remove is
   unmeasured (step 1 measures it; a ring is reconsidered only on numbers).
4. **Nothing is stamped at first:** notes, wheels and bound actions all land at the next block start,
   as every UI gesture does, so one ring's admission order is also the execution order. The native
   stack's existing stamp on bound actions goes. A stamped command executes on its frame while a later
   unstamped one executes at the block start before it: a stamped `NoteOn` at frame 180 and a release
   applied at 128 leave a note no owner holds; a HOLD stamped at 180 and its release applied at 128
   leave a lane recording; CLEAR at 180, a UI press at 128 and CLEAR at 200 confirm a CLEAR that the
   press should have disarmed. Stamping comes back only with one execution-order contract covering
   notes, looper presses, releases, UI presses and clockless gaps, and only if the comparison after the
   merge (step 7) shows it pays.
5. **One ordered output queue.** Every input-originated command goes through one bounded FIFO into the
   ring: the router's outputs (notes, target changes, actions, releases) and the UI's input commands
   from `engine_send` (looper presses, `Press`, toggles), so a pedal and a click keep their order. A
   command the ring refuses stays at the head and nothing behind it overtakes it; the port thread
   drains it on a short timer, so a stalled queue empties when traffic stops. Capacity is reserved, not
   hoped for: accepting an attack or a HOLD reserves the slot its release will need, a batch (a
   re-strike's `NoteOff` and `NoteOn`) is accepted whole or not at all, and a target change replaces a
   target change still queued. When the unreserved room is gone, fresh attacks, HOLDs and wheel updates
   are refused (counted, logged; a refused attack records no owner); wheel updates coalesce, never
   across a note or an action.
6. **The router follows the engine generation.** Each queued command carries the engine generation
   it was made for. On an engine rebuild (`owner.rs` `swap_engine`) the router pauses admission and
   draining, discards the queued one-shot commands of the old generation (attacks, actions, HOLDs; the
   new engine has no voice or capture for their releases to end, so those go too), folds the latest
   desired target and wheels, queued or held, into the remembered settings, keeps its owners (a later
   release stays harmless) but forgets which notes the old engine sounded (a fresh press attacks
   again), clears its HOLD bookkeeping, and resumes once the settings replay has run. The target and
   wheels reach the new engine through that replay alone, never a second time from the router. A stopped retained engine keeps its state.
   This runs natively, with no help from the WebView, so a stalled UI cannot block recovery.
7. **No-device admission is one rule for every source:** with no device running (no engine, a stopped
   retained engine, or a rebuild), fresh note-ons and actions are dropped, controller state is kept,
   and every release passes: note-offs, pedal-ups and HOLD `Release` (a dropped HOLD release would
   leave a lane recording). A UI-owned hold is released when its epoch is replaced (a reload or
   recovery subscribes again) and on window blur; every UI input event from a replaced epoch is
   refused, a target change and a panic included.
8. **Bindings live native,** in a versioned JSON store beside `plugin-folders.json` with explicit
   missing, invalid and unsupported-version results (an unreadable file is reported and never
   overwritten, as the folder store). A binding persists the strongest endpoint identity midir gives
   (the device-interface path, whose stability across restart and replug is the owner's device check)
   plus a port discriminator, and keeps the name. Resolution runs on one enumeration snapshot: exact
   identities first, and the ports they match are claimed; a stored identity that is absent moves to
   an unclaimed port only when that port is the one unclaimed port with its name and no other absent
   identity carries the name (a pedal moved to another USB port keeps working, as under Web MIDI), and
   the move is persisted. Ambiguous identity leaves a binding unresolved; it never fires another
   controller's action. Writes happen off the router's lock.
9. **Migration never guesses.** At first start, `lf.midiLearn` imports idempotently, keeping each
   record's legacy port id. Step 0 found that id a per-run ordinal, so the record's port name is its
   identity: a record activates on the one present port with that name, and stays inactive and listed
   while no port or more than one carries it, until a matching port appears alone or the player
   assigns it to a present controller in Audio Settings. Records of one name from several legacy ids
   that bind the same message stay inactive too (a renumbered run may have relearned it). The legacy key is removed one release later, after
   the native store has acknowledged a durable write. Bindings load before input executes actions.
10. **Port liveness:** `CM_Register_Notification` (no window pump) on both MIDI interface classes a
    WinMM port may arrive on under `wdmaud2` (the WinMM MIDI input class, and the MIDI 2.0 endpoint
    class step 0 saw the service's endpoints on; which one a controller uses is the owner's device
    check), registered before the first enumeration.
    A notification invalidates the connection generation and wakes the port thread; a removal releases
    that port's notes and HOLD; an arrival reopens after enumeration confirms the port, retried a
    bounded number of times; duplicate notifications are harmless; callbacks from an old generation
    are ignored. The 1 s poll stays as a backstop. A connection keeps its identity for its lifetime
    (hole 1).
11. **Every toggle has one owner, the engine.** Toggles of engine state (click, END STOP, FIXED,
    RETAKE, AUTO REC, the IN FX sends) become engine actions that read the current state when applied,
    refuse with the existing reasons in the states `gates.ts` names (leaving applied and remembered
    settings unchanged), and are sent as intent by every producer: UI buttons, transport keys and MIDI
    alike, so a pedal and a click never cancel each other. Absolute setters stay for initialization and
    replay. The engine publishes each applied toggle as an event; the native settings memory takes the
    applied value from it (and reads the engine's applied values before a rebuild, events drained or
    not), and the UI's mirrors follow the event instead of flipping their own signal. A toggle action is itself a looper press (it disarms a pending CLEAR before
    any refusal), and the input-send toggles keep `SetInputSend`'s scheduling (never behind a looper
    command held for a block job). Native sends `Press` before every other facade action that has one
    today and `SelectTrack` before a named REC/DUB or HOLD. The stage view and GO LIVE stay UI actions, run on the native event. Tap
    tempo keeps one history owner in the UI, fed at event receipt (no worse than today).

## Parity: what must not get lost

Each item is a TS rule today; each gets a test at the layer that can see it (router semantics in Rust,
gestures and highlights in the browser probes on the native-interface fake).

- velocity 0 is a note-off, in play and in learn; velocity reaches the engine as `v/127`;
- the active slot is routed before a note-on (`ensureActive`); a target switch releases held notes
  before `SelectInstrument`; a plugin swap routes nowhere while it unloads;
- note ownership: `NoteOn` on a note's first hold, `NoteOff` only when its last owner lets go and no
  sustain holds it; two UI owners on one note; a re-strike under the pedal sends `NoteOff` then
  `NoteOn`;
- sustain per owner; pedal-up releases only that owner's deferred notes; CC123 releases only its
  owner's notes and honours its own pedal;
- wheels last-moved-wins per owner; an unplugged owner's wheels give way to the survivor's; a new
  target is seeded with the current wheels; unchanged values are suppressed, but replayed into a
  rebuilt engine exactly once;
- learn: consume-first before the CC64/1/123 branches (a learned CC64 never sustains; a learned note's
  note-off is consumed); CC120-127 are never learned; learning a CC releases that owner's sustain and
  modulation; per-binding 10 s release waits (two controls can wait at once), surviving cancel and
  relearn, checked before learn capture; the spent release after an edit; level-based firing,
  reversed-polarity presses and same-value latching presses; all 25 actions with lane targets; HOLD
  with its control numbers, bounded by the engine's 16 (`HOLD_CONTROLS`) with a defined refusal when
  exhausted; a forgotten or edited held binding and a port that goes release their HOLD;
- toggles: FIXED, RETAKE and AUTO REC refused where `gates.ts` refuses them today, with the same
  reasons; alternating pedal and UI presses each toggle once;
- the learn UI: `learning` and `awaitingRelease` (ended by a native timer, not the next message) as
  native events, with a UI mirror of `learning` so Esc cancels synchronously; the refusal cues, the
  list, the kind switch, the HOLD toggle;
- the unplug-mid-note toast and note release; the device list, which says when a port is held by
  another program; release-log lines for failed opens and failed sends.

## Work order

Steps 1 to 5 add only dormant code, instrumentation and engine actions the UI can already use, so each
lands on `main` green (`pnpm check`, `pnpm build`, `pnpm rust:check`). Step 6 is one atomic switch on
a branch, merged only when its gates, its running-app test and its measurement (§ Measurement, ship
rule) pass. Re-read this plan's § Decided before each step.

0. **Settle the machine unknowns** (no install): launch the DEV app once and read the `[diag]` Web
   MIDI input list; list midir's WinMM ports and their ids; find out whether Windows MIDI Services'
   loopback endpoints reach WinMM once `midisrv` runs, and whether WinMM input is multi-client through
   `wdmaud2`; which interface class device arrivals come on. Map Web MIDI port ids and names to
   midir's, and test whether midir's ids stay stable across an app restart and a replug (decisions 8
   and 9). Settle the measurement port here: without one, step 6 cannot merge (§ Measurement). Write
   the answers into this plan.
1. **Baseline instrumentation and the Web MIDI baseline**, before any input path moves (§ Measurement),
   plus a 10-minute measurement of the lock waits (`settings`, `ends`) under the jam load.
2. **Native learn and router parity, dormant:** the owner type, the 25 actions, targets, HOLD, the
   release rules, the no-device rule, target ownership, the output queue, the generation handshake,
   the stamp removed; test first from § Parity. The toggle engine actions (decision 11) land here too,
   with their applied events, the wire and UI reducers and the settings-memory projection, and the UI
   switches its own buttons and keys to them at once (tests: alternating producers, a refusal that
   changes neither state nor memory, a saturated event ring, a frontend reload, a rebuild right after
   an accepted toggle).
3. **Lifecycle, storage and the interface, dormant:** the binding store and migration, the platform
   interface and its browser fake, the native event channel, `MidiHost` in `EngineApp` (a take out of
   the static on shutdown and on the updater's shutdown path, before `host.shutdown()`), the held-note
   set.
4. **Ordering and overload tests:** a target switch then a note-on back to back sounds on the new
   target; mixed MIDI and UI owners on one note; a dense multi-port CC stream with a release burst
   against the 64-entry table (a release the engine did not accept is retried, wheel updates
   coalesce, never across a note or an action); a FIFO filled with reserved entries, then a pedal-up,
   a disconnect's releases and a target switch; a refused attack, switch or release followed by a
   re-strike; an engine rebuild with notes and HOLD held, a refused attack, a HOLD and a wheel update
   queued, and the WebView stalled; a retained-engine device gap with a HOLD press and its release.
5. **Ports: identity and liveness, dormant**, with tests on the pure parts and a notification test on
   the rig with a dynamically created Windows MIDI Services endpoint, if step 0 finds one (physical
   USB replug stays the owner's check).
6. **The switch, one branch:** start `MidiHost`; MIDI and UI input go through the Rust router; Web
   MIDI and `input-router.ts`'s musical logic go; the browser probes move to the native-interface
   fake (gesture-to-command and feed-to-screen assertions stay); a running-app test covers mixed MIDI
   and PC ownership, focus loss, native events and a frontend reload; then the after measurement on the
   branch, against the ship rule, before the merge.
7. **After the merge:** fold this plan into the briefings and delete it. A stamping experiment
   (decision 4), if any, runs separately from the accepted switch.

## Measurement

The question: how long a MIDI message takes from reaching the app to the engine applying it, how much
that varies, and how it behaves while the UI is busy.

- **Correlation instrumentation (DEV only, identical in both paths):** each input gets a sequence id;
  the app records its arrival `Instant`, and the engine records the frame it applied that `NoteOn` on,
  into a preallocated diagnostic buffer drained off the RT thread (invariant 5). No musical command is
  added (a companion `SelectTrack` would perturb the jam and wait behind block jobs that notes skip).
  Frames convert to time through FrameClock's `Instant`-based `(entry, frame)` pairs recorded with each
  sample, never the feed's SystemTime anchor.
- **The comparison (needs a port):** a DEV `midir` sender in the app, on its own thread at a fixed
  interval independent of the UI, sends to a loopback port and stamps the send `Instant`; before and
  after use the same sender, so one QPC clock spans the whole path. The port is a Windows MIDI Services
  loopback if step 0 finds one, else loopMIDI (its install is the owner's to approve at the PC).
  The ship rule needs this comparison, never internal numbers alone; it ran on loopMIDI (§ Progress).
- **Diagnostic only, labelled as such:** injection at the JS handler seam (before) and at
  `Core::message` (after) measures internal dispatch; it starts after the UI thread's queue, the very
  delay the move removes, so it is no before/after comparison.
- **Load and traffic:** the DEV app on ASIO 128 (`pnpm dev:asio`), an amp-sim or Pro-Q live in slot
  1, three lanes looping, the stage view open; notes spaced and in bursts, wheel sweeps, pedal presses;
  deliberate UI stalls (a long main-thread task every few seconds). The same plugin, preset, rate,
  buffer, traffic, warm-up and load before and after; several runs, at least 5000 notes in all.
- **Audible check (needs the loopback cable):** notes sent at a fixed QPC interval, their onsets read
  off the loopback recording in device frames; this sees the callback wake jitter that every
  frame-based number above shares and hides.
- **Report:** sent, applied and lost counts; p50, p99 and max of arrival-to-applied time, and the same
  inside UI stalls; queue lateness; callbacks, xruns and clock discontinuities during the run.
- **Ship rule:** on the step-6 branch, no note is lost or stuck, and p99 sender-to-applied is no worse
  than the baseline's and clearly better inside UI stalls (handler-arrival-to-applied is reported
  beside it, never instead). Otherwise the switch does not merge, and the cause is found first.
- **By ear (the owner):** a controller and a footswitch on the native path in a jam; 20 sub-second
  replugs of real hardware (held notes released, bindings firing after each); the pedals learned before the update still fire, with no relearn. These enter
  `STATUS.md` § Not heard yet when the switch ships, and its next-jam footswitch check is rewritten.

## Docs that change at the switch

`AGENTS.md` (the play-path and architecture lines on Web MIDI), `docs/ARCHITECTURE.md` (the MIDI
paragraph, the `MidiBackend` row, line 16), `README.md` (the MIDI lines), `src/ui/AGENTS.md` (learned
MIDI behind `state/midi.ts`), `src-tauri/AGENTS.md` (the thread map's native MIDI row, this thread),
the `engine_io` and lf-engine briefings (the D22 lines; lib.rs, `api.rs` and `instruments.rs` naming
`input-router.ts` as the owner of sustain), `verify/README.md`, `docs/VERIFY.md` (Web MIDI under
Playwright), `src/platform/host.ts`, `src-tauri/src/lib.rs` (the permission auto-grant becomes a deny),
`STATUS.md` (the next-jam footswitch check).

## Not in this plan

MIDI out, MIDI clock in or out, SysEx, program change, MPE, per-channel slot routing, wheels to plugin
slots (D12), VST2 CC/bend (`src-tauri/AGENTS.md` § Open threads), pointer glissandi, and a second
command ring or any stamping unless step 1's lock waits or a later comparison justify them.
