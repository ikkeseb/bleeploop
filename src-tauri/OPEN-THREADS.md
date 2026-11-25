# src-tauri/: open threads (no gate)

- Six timing tests failed once each in a loaded local `pnpm rust:check` (2026-10-16), green alone and on rerun:
  the CLAP `a_request_from_another_thread_never_waits_for_the_owner`, the `engine_slot` give-up-during-a-restart
  test, the feed thread's hold test, and three `engine_io` tests that record a first loop. The cause was read
  from the code, never reproduced: short wall bounds a loaded PC outlasts (500 ms to 5 s waits, the test
  device's `REMOVE_TIMEOUT` 2 s, and `record_loop`'s 1.5-bar stop sent from the test thread's wake, which the
  first take's floor to whole bars turns into a 2-bar loop when late). Tests now wait up to 10 to 30 s
  (`cfg(test)` variants of `REMOVE_TIMEOUT`, `CALLBACK_WAIT`, `START_WAIT`, the owner `ask`; `test_rig::PATIENCE`)
  and stamp the loop's stop on the engine's frame. Next check, only if one fails again: loop it under a
  parallel `cargo build` and read the failing line against these bounds.

- Tone recall: an owner's save of plugin P in slot S waits on that tone's own in-flight store write (an
  import of P into S under its write lock). A disk write that hangs therefore stalls that owner, and an
  unload joins it without a timeout (the app's exit is bounded). Other tones' writes never block it; a
  store worker doing the file I/O would remove the wait.
- An unload that gives up waiting (6 s) for a command still using the handle empties the slot while
  that command's drop of the handle still tears the plugin down (read from source, never run; no
  command found holds a handle that long: each owner round trip is ≤ 5 s). A load meanwhile fails on
  "already holds a unit" (rig recall keeps the plugin) or wins the slot first; the late owner's unit
  may sound for its crossfade. A load whose unit went in as its 15 s bound ran out, and whose report
  then took 2 s more, frees the slot the same way (`engine_slot::LoadClaim`). Next check: a slot test
  in `engine_io/plugins.rs` that holds a handle clone past the unload wait, then loads. A fix carries
  an owner-ended flag in `Leaving`.
- VST2 host gaps (read from source, none reproduced; the only real plugins tried are ReaJS and
  ReaStream, which load, open their editors and swap, and whose `native:tone-recall` fails because
  neither has a parameter that holds a value):
  a restart whose unit the engine does not hand back within 2 s leaves the unit silent until the
  plugin's next `audioMasterIOChanged`; `effSetChunk` answers no verdict, so a chunk the plugin
  could not use is reported as restored; a program is selected without
  `effBeginSetProgram`/`effEndSetProgram`; the plugin gets notes only (no CC, pitch bend or SysEx)
  and a time info without transport; `FAULT_PROCESS` has no VST2 site (its process call
  answers nothing).
- A unit whose restart failed stays parked, bypassed, until the plugin's next restart request; a device
  change does not retry it.
- An output callback that misses the engine lock reads as a duplex fault. Two ASIO overloads each
  reported to a different stream alone count once in `xruns` (`callback::Run::stream_error`).
- The ASIO callback is not lock-free: asio-sys holds `BUFFER_CALLBACK.lock()` around every callback,
  and cpal 0.18.1 takes its `asio_streams` mutex in them (`Start` reads the buffer size before either
  stream plays, so the owner does not contend there). Fix: patched ASIO dependencies, with the L1+L2
  latency measurements.
- Two live slots on the same capture channel sum it (+6 dB).
- An ASIO period the driver drops without its overload report is not flagged as damage (input and
  output stay in step; the take is spliced there). `asio_phase_slips` counts it in the log only (after a run's first
  second; two in consecutive half-seconds count once), beside a lasting move of the driver's phase, which the host clock cannot tell from a drop (asio-sys drops
  the driver's sample position; `callback::PhaseSlips`). A single late wake is no signal: on the rig's
  USB driver at 64 frames, wakes more than a period behind the best phase came ~180 times a second
  with every fault counter 0 (2026-10-07).
- The EVO 4's driver asked for a stream reset 20 times in the first minute after a launch's first
  ASIO open (2026-11-10, a dev build on an empty profile, 256 frames behind the 128-frame preopen): the
  log's "Stream reset was requested by the ASIO driver", about 3 s apart, each handled as a lost
  device and recovered on the same request, the last one staying up. None came in the 40 minutes
  after, three further ASIO opens at that size included, and none in the first 134 s of a second
  launch on another empty profile an hour later (same build path, sizes and preopen). Cause unknown: whether the preopen (written
  for the Focusrite driver), the recovery's own reopen or something outside the app (the thread
  below, EVO Control) set it off was not looked at. Next check: grep the log of a launch on the
  owner's own profile at 128 frames for that line; if it is there, the same launch with the preopen
  skipped.
- A rate or buffer change made in the ASIO driver's own control panel while the app runs (read from
  source 2026-10-07, never run on a device; Audio Settings sends a fixed-size driver's users there): a
  reset, resync or rate change reaches the run as cpal's `StreamInvalidated`, which the owner handles
  as a lost device, without the fade-out an in-app switch gets, and the recovery reopens the saved
  request (`transition::fallbacks`), likely putting the app's rate and buffer back over the panel's. A
  `kAsioBufferSizeChange`, which cpal 0.18.1 accepts with no error, would leave the run on the new
  block size with its latency alignment frozen at the old one (`callback::LATENCY_SAMPLES`), a take
  possibly off the click, and the buffer select stale. Which messages a driver sends is unknown; the
  Focusrite driver this was written against is uninstalled, and the rig's driver is now the EVO 4's,
  whose EVO Control tray app is where the buffer size and the rate are set (`STATUS.md`). A fix keeps the loops (a rate the owner did not confirm discards them: `Owner::open`) and
  shows what runs, rather than adopting the driver's settings blindly. Next check, on the rig: change
  the buffer in the driver's panel, then the rate, each while loops play, and record a take after
  each: the log, the block size the callbacks get (the status reads it only at the open), and whether
  the take lands on the click. Under WASAPI the buffer select lists every
  size and the run ignores it (`cpal_driver::resolve_wasapi`).
- The feed's bound is per subscription (`feed.rs` module doc): within-page JS callbacks are retired
  in `src/platform/host.tauri.ts` (guard: `verify/guards/feed-channel.mjs`). Tauri 2.11.5 still keeps
  a large payload in its map until a page fetches it; dropping the channel does not clear it
  (upstream `ipc/channel.rs`). Unfetched payloads remain per reload and per watchdog resubscribe
  after a gap, up to 30 frames' payloads per subscription. Read from source; next check if memory
  grows: the app's private bytes over twenty reloads with the stage view open, then with a fetch failed.
- A page that dies for good (a WebView crash, a shutdown) leaves the scope taps on (`Key::Scope`), so
  the engine keeps folding columns the feed drains and drops for nobody. Read from source, not
  reproduced, nothing audible.
- A punch-out inside a take's last quarter-beat commits the whole bars before it, where a stop there
  rounds up (owner's call).
- The feed's count-in mirror infers which count a lane event ends from arming and frames alone, so two
  splits stay: a counted take's cancel, transport start and ordinary re-arm of the same lane within one
  block publish no lane event (unchanged info), and a reset can replay the old count; and a full event
  ring that takes a new count's beat but not the lane events after it republishes them at a later frame
  (`Looper::publish`), where the old lane's disarm reads as the end of the new count, so a reset before
  the next beat shows none. Read from source, not reproduced. Next check: an engine-backed feed test of
  each (the count-in tests in `engine_io/feed.rs` drive a real engine) once `LaneInfo` reports the
  counted arm.
- The no-device removal path (a 1-frame process and `stop` on the plugin owner's thread) has no test
  with a real unit, and the CLAP restart fixture's thread check would flag it.
- Native MIDI (rules: `docs/ARCHITECTURE.md` § Decided: native MIDI; the by-ear checks: `STATUS.md`
  § Not heard yet). Residuals, each needing an IPC call stalled past its 2 s bound or a clock moved
  back: a note-on whose batch timed out can land after its release and stick until a blur, a slot
  switch or a panic; a blur owed after a failure, answered late, can release a hold made after it; a
  page whose `performance.timeOrigin` reads older than its predecessor's (the wall clock moved back
  and the WebView restarted) is refused until the app restarts. Next check: log `input_send` round
  trips over a jam: does a 2 s stall ever happen? Only a hardware controller can settle
  (none has reached native MIDI yet): whether the port name Web MIDI stored equals WinMM's `szPname`
  (where it differs, an imported binding waits until the player assigns it), whether midir's id (the
  device-interface path) is stable across a restart and a replug, which interface class a port
  arrives on (the next check: `src/engine_io/midi/liveness.rs`'s header), and whether WinMM input is
  multi-client under `wdmaud2`. Owed: the release after the first with native MIDI removes the web
  build's `lf.midiLearn` key (`src/app/midi-actions.ts`).
- Archetype Plini (VST3) once stalled 4–14 s in 5 of 20 unloads, editor closed, and has not repeated
  since (cause unknown). The VST3 teardown and the unload log per-step timing in release too, so the
  next occurrence names its step.
- Crackle, the owner's two reports, measured on the Scarlett with silent soaks (`native:engine`; the runs' numbers:
  this thread's git history). **In a call:** Share output into the interface's own Windows endpoint
  ran short in bursts at a 20 ms setpoint and soaked clean at 40 ms (`engine_io/share.rs`); the
  mirror's first pull now plays silence (unsoaked). **While agents build** (ASIO 64, `--tone=1
  --load=test`): a normal-priority build on the audio thread's own core stretches the callback four to
  eight times its median with its thread on the CPU throughout (`tone::cycle_stamp`; MMCSS priority
  25, with or without plugins), and the loopback tone breaks (6 to 48 silent samples) where a
  callback finishes about 2.5 periods or more after the wakes' best phase, with no fault counter moving.
  A larger buffer does not help (256 broke too); a load kept off cores 0 to 3
  (`--load-affinity=FF00`) ran nothing long in two runs. The release log's `asio_late_finishes`
  (`callback::LATE_FINISH_PERIODS`) counts near misses too and missed one break at 256; `over_budget=N`
  marks every long callback. The cable returns through the same driver, so a break's side (input or
  output) is unknown. Open, with the next check: by ear, whether keeping builds off the audio core
  keeps a jam at ASIO 64 clean, and whether the call's crackle is gone (`STATUS.md` § Not heard yet);
  whether a below-normal build (the rig's tmux session starts them so) stalls it too, and which of
  the core's SMT sibling and caches matters (`--load-affinity=AAAA`); one-frame steps in the tone (no
  late callback near; side unknown); something added near the tone's frequency (a beat fading over
  0.6 s about 6.8 s into the stream, smaller ones minutes apart, seen at `--tone-level=0.125`; next:
  the input with no tone). A tester hears crackle too, less with a gate on their interface (their
  input side; their backend unknown, and the WASAPI join's bursts below would sound like that).
- Plugin-host gaps a source review found (2026-10-02; read from source, none reproduced), ranked by
  exposure on the owner's plugins. First: VST3 omits trailing inactive aux buses (the SDK's
  `activateBus` rule permits it) and passes short `setBusArrangements` arrays whose result is read
  back, not checked; a tone saved by an export or an unload before the next block has consumed an
  accepted parameter edit loses that edit. Then: two slots loading one VST3 DLL run its init
  unserialized; a CLAP restart can deliver a removed parameter id; a non-discardable VST3 module is
  unloaded. Lower: kReloadComponent only reactivates; a MIDI-only CLAP note port gets CLAP notes; CLAP
  editor edits never reach the drawer.
- The quit window (a close and an update share it: `src/app/close-guard.ts` `quitWithSave`): input
  stays open from the quit save's snapshot to the engine's shutdown, so a take in flight, or one a
  pedal or the UI commits in those 1-5 s, is not in the recovery. Next: an engine-owned quiesce through
  the command ring (stop admitting fresh presses, end captures as a punch-out keeps them, let block jobs
  finish while the device renders, then answer), after which the quit save reads its snapshot. And an
  update whose installer fails after the engine's shutdown (`ShellExecuteW` failing, or the installer
  quitting with an error after it started) leaves the app without an engine or with none open: the
  first says "restart BleepLoop", the second nothing; a recovery needs a process outside the app that
  watches the installer. Neither has been seen; only a real update shows them.
- After a WASAPI open the join can trim or starve within ~2.5 s, and a take that overlaps it is
  rejected (how often: `docs/VERIFY.md`, the `native:engine` baseline). Traced (the join trace in
  `engine_io/callback.rs`): once primed, capture delivers a packet early and a render callback comes a
  period late, so the ring sits up to two periods over its 25 ms setpoint, under the trim line, for the
  ~10 s the controller takes to drain it, and one more late callback trims. Why the endpoints start
  this way is unknown. The trims stay (D25): a rule that tolerated the bursts, replayed on nine traced
  opens, removed the trims but kept 27 to 29 ms more input queued for up to 3.5 s. `native:engine`
  forgives a WASAPI open's first 3 s (`probe.rs` `GRACE`). Past the open the join also starves in
  bursts minutes in, beside an ASIO process and alone under load; neighbour, load and priority class
  are not separated. Widening the setpoint is monitoring latency: the owner's call (D25).
- Loopback offsets the bars pass but nothing explains (`native:engine-loopback` on the Scarlett): each
  device open lands a whole number of frames off (−5..+4), the same for every take inside it, and once
  at 128 a 32 s multiply take stepped one frame late between its beats 24 and 32 (two relaunches did
  not repeat it). A stray sound reached the cable on 2026-10-02 (about 200 Hz decaying over ~1.4 s and a
  ~400 Hz tone every 6.000 s, mostly in the first take after a device switch), now failed by name as
  stray sound (likely also the one launch at 64 that read 149 times the floor bar past FADE's bar
  line); its source is unknown, other apps playing through the interface the suspect. Next check:
  the first EVO 4 loopback run (`--channel=0`), other apps' sound silenced, logging each open's offset.
- A session load the engine already took off the session port cannot be withdrawn (`engine_io/session.rs`
  `run_job`): a callback that stalls more than two 10 s waits between taking a load and applying it would
  apply a load already answered as failed; only the engine's fault path recovers it. Read from source,
  never seen. Next check, only if a load ever plays after a failed answer: the log's session-job lines.
- Chain units (`engine_io/chain_cmds.rs`, `unit_host.rs`; read from source, none reproduced). A
  `SetUnitParam` racing a withdrawal (the gate holds a reseating unit's commands and replays them) is
  unobserved: no host unit overrides `UnitProcessor::set_param` (`lf-engine` `api.rs`; a plugin's
  parameters ride its owner's ring), so no plugin unit takes one. Next check, when a host unit does: a
  test that sets one while its unit is withdrawn and reads the value it reseats with.
  A unit command already in the engine for a future frame reaches whatever seat holds its id when that
  frame comes; the UI mints random u32 ids (`mintUnitId`, `src/platform/index.ts`), but an id removed and
  added again within that window would take a command meant for the old instance. A fix is an
  engine-side fence (the command carrying its install, or a removal dropping its id's queued commands).
  Next check: a test that queues a command for a later frame, removes the unit and re-adds one under
  the same id before that frame.
- `tauriChainHost`'s per-slot queue (`src/platform/host.tauri.ts` `onChainSlot`) waits for each call to
  settle, so a remove or reload held across a take keeps the slot's later calls waiting until the take
  ends, while the native order mutex was already released at `waiting` (`chain_cmds.rs`
  `wait_ordered`). Conservative: calls wait, never reorder. A fix needs native to signal per call when
  it is ordered (taken under the slot's order), so the queue can move on then. The same queue lets a
  remove of a unit whose add is still in flight skip it (the unit has no slot mapping until native
  accepts the add), so it can run ahead of a later add queued on that slot. Next check: a probe that
  queues an add behind a held remove during a take and times the add, then one that removes a loading
  unit with a second add queued behind it.
- Chain host edges to settle before chains reach players (a cross-family review of the chain
  commands, read from source, none reproduced; the chain host runs only in DEV builds today):
  a) an evicted source that answers `Replaced` (a newer source queued after a rebuild) retires its tone
  before the engine has taken the newer one, which the engine can still refuse (latency), leaving the
  slot with neither; b) a source parked after a failed reactivation is not replaced by a newer source
  that seats while it is out (the projected head omits it), so it keeps its owner and tone until it
  retries; c) a plugin owner retrying `PortFull`
  after its add's command returned can lose its place to a later edit on that slot; d) a built-in add
  answered `waiting` whose box a rebuild returns after the 10 s first-answer bound is dropped instead of
  offered to the new engine; e) a reload the engine refused can log a generic cause when its row is
  released before the waiter polls. Sources and built-in factories arrive with E3. Next check: one
  `chain_tests.rs`/`chain_cmds_tests.rs` test per letter, red first, before chains reach players.
- A clean chain unit's tone is not pinned (DEV chain mode, read from source, never run; a plan review's
  finding, left for later in E3f): a plugin unit that never saved its own `unit-<id>.tone` (nothing was
  changed) reads its plugin's slot file at every load (`ToneStore::load_for_restore_key`'s fallback). When
  another instance of that plugin in that slot retires its tone into that file (the player removes it), the
  clean unit's next load, at the next launch's restore under its id, starts from the other instance's tone,
  not the one it played. The shutdown's own removal no longer retires (`EngineHost::remove_unit_at_shutdown`),
  so only a player's removal or a source replacement does this. Next check: a `chain_cmds_tests.rs` test with two units of one plugin in slot
  0, the first never saving, the second saving a new tone and removed by the player, then `unload_all` and
  the first re-added under its id: read the tone its load restores. A fix writes the inherited tone to
  `unit-<id>.tone` at a unit's first load.
- The shutdown's keep of a unit's tone misses a reload in flight (DEV chain mode, read from source by a
  review, not run; declined at E3f's second review round as rare and no worse than before E3f): with a same-id
  reload queued ahead of the shutdown's removal and its `Event::Seat` undrained, the candidate's owner can end
  on `Left::Removed` before `Table::promote_reload` carries the predecessor's `Row::keep_tone` to it, so its tone
  retires into the slot's file (the restore then reads it from there: the pre-E3f behaviour, wrong only with two
  instances of one plugin in a slot); and a different-id source replacing that candidate ahead of the shutdown's
  removal can have its cleared keep revived by the later promotion, so the replaced unit keeps an orphan
  `unit-<id>.tone` instead of retiring it. Next check: a `chain_cmds_tests.rs` test per case (a reload queued,
  then `unload_all` with the Seat undrained; a reload then a replacing source then `unload_all`), asserting both
  files. A fix binds the disposition before the candidate's terminal return is posted and lets promotion copy
  the keep only onto a row still `Staying`.
- Two `chain_tests.rs` tests are `#[ignore]`d (quarantined): `held_remove_outlives_remove_timeout` and
  `legacy_install_during_take_returns_within_settle_seats_on_end_and_surfaces_refusal`. On CI's Windows
  runner (`rust-test`) their `start_take` never sees lane 0 recording, even with `test_rig::PATIENCE`
  (30 s); every local run passes them, alone and in the full suite. Both run a real-time `TestDevice` at
  64 frames every 5 ms (other chain tests: 1-2 ms), where dev's own take tests drive frames by hand
  (`tests.rs` `Harness`). Next check: run them on CI with `--ignored` and a log of the device's block
  count and lane 0's state while `start_take` waits; whether the take is not sent, not started, or not
  seen in `drain_feed`'s overview settles which fix (faster device period, `SetSlotLive`, or reading
  lane events instead of the overview). Un-ignore both before chains reach players.
- `chain_tests.rs`' `legacy_install_during_take_returns_within_settle_seats_on_end_and_surfaces_refusal`
  is load-sensitive: its 2 s wall bounds (the install's return, the seat and the refusal after the take)
  failed once in a 479-test `cargo test -p app` run and pass alone. Next check: rerun the full
  `cargo test -p app` on a quiet PC; if it fails again, widen the bounds to `test_rig::PATIENCE` or
  stamp them on the engine's frames, as the timing tests above were.
- A note can overtake a synth pick held at the unit gate (DEV chain mode, read from source, never run): across
  a device rebuild a queued `PickSynth` waits at the gate for its SYNTH's reseat; once the SYNTH is back in the
  engine but its `Seat` answer is still unread (the model says reseating), a fresh `NoteOn` through
  `EngineHost::send` is not gated and reaches the ring before the pick (`engine_io/mod.rs` `send`,
  `unit_host.rs` `flush_gate_locked`), so it plays the previous instrument. Next check: `midi/tests.rs`'
  `a_queued_synth_pick_reaches_its_reseated_synth_through_the_gate_before_a_later_note` without its wait for
  the gate to empty (send the note right after `wait_op`), and read the log's order. A fix gates a note behind a
  pick held for its slot's unit.
- A target switch split across two blocks can retrigger a sustained note under a mono Bass for one block
  (read from source, never run; review finding). The MIDI queue drains command by command (`midi/queue.rs`
  module doc), so when the engine's command ring takes only part of a switch's batch (held notes' `NoteOff`s
  first, then the sustained ones, then `SelectInstrument`), the Bass holding G with C sustained gets G's
  `NoteOff` alone, falls back to C and attacks it before C's release and the switch arrive a block later.
  Today's `Builtin` target switch has it, in the same order; a DEV synth pick's batch is the same shape and
  adds nothing new. It needs the 1024-command ring full. Next check: an lf-engine test that plays a mono Bass,
  G held over a sustained C, applies G's `NoteOff` alone in one block and C's with the switch in the next, and
  reads whether the first block carries a new C attack; if it does, a jam settles whether one block of it is
  heard. A fix sends a switch's or a pick's batch to the ring whole or not at all.
