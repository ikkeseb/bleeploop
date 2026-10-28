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
  could not use is reported as restored;
  `effIdle` is sent once per `audioMasterNeedIdle`, not until it answers 0; a program is selected
  without `effBeginSetProgram`/`effEndSetProgram`; the plugin gets notes only (no CC, pitch bend
  or SysEx) and a time info without transport; `FAULT_PROCESS` has no VST2 site (its process call
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
- The feed has no backpressure: `turn` (`feed.rs` 396) sends every tick while `Channel.send` succeeds
  (`mode.rs` 231), and Tauri 2.11.5 queues large payloads unbounded until the page fetches them, so a
  stalled WebView grows a backlog of stale frames. Fix: a fixed number of outstanding frames with an ACK
  per subscription and sequence; drain meanwhile, resync on resume without breaking active gestures or
  session revisions, and keep one-shot events in their own bounded queue.
- A page that dies for good (a WebView crash, a shutdown) leaves the scope taps on (`Key::Scope`), so
  the engine keeps folding columns the feed drains and drops for nobody. Read from source, not
  reproduced, nothing audible.
- A punch-out inside a take's last quarter-beat commits the whole bars before it, where a stop there
  rounds up (owner's call).
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
  build's `lf.midiLearn` key (`src/app/midi-actions.ts`). Its module briefing (`midi/mod.rs` header)
  restates ARCHITECTURE's subscription/epoch, nothing-is-stamped, no-device and rebuild-needs-no-WebView
  rules: replace only that overlap with pointers, keep its local access, batch, lock and retry detail.
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
