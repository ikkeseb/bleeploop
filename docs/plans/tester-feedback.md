# First external tester feedback (OPEN)

Owner-requested behavior and tester reports collected from 2026-09-19 (F13–F16: 2026-09-24), with local fixes and
verification recorded below. Testing is ongoing; fold new feedback into the
matching item. When resolved, move enduring decisions to their owning briefings and delete this plan.

## Evidence and scope

The tester reports building a Windows `app.exe` with Rust, without ASIO, and running it with a
physical MIDI keyboard. Exact commit, build command, audio device/driver configuration and plugin
versions are unknown. Screenshots and the owner's account establish the original observations below;
local verification does not confirm the tester's machine. Screenshots are temporary, uncommitted
attachments; their relevant contents are transcribed here.

All items remain open until the tester confirms. § Code reading records what the source did at
`d17c777`, before the fixes under § Landed; § Work order is the owner-approved sequence.

## Requests and reports

| ID | Observation or request | Desired outcome and unresolved evidence |
|---|---|---|
| F1 | After starting with a one-bar loop, the tester could not choose another bar count. The owner suspects FIXED mode. | Make bar selection understandable and usable at the relevant recording stage. Check the actual lock condition; FIXED is a hypothesis, not an established cause. |
| F2 | Track 2 contained four attacks at the beginning, then silence, while track 1 spanned several bars. The owner confirmed that the silence was audible, not just a waveform issue. Screenshot: master reads 8 BARS at 110 BPM; track 1 fills the lane; track 2 has four early peaks then a flat line; both are stopped. | Automatically recognize the logical musical bar/phrase and repeat the shorter track across the longer loop. The owner expects this to work without manual repair. Determine how phrase length is established before choosing an algorithm; silence alone does not prove a phrase boundary. Exact recording/stop sequence and intended short length are unknown. |
| F3 | WAV export appeared not to work, or the tester could not find the saved file. | Export must produce a discoverable file and a clear success/failure result. Exact export action, resulting format/path and whether a file was created are unknown. Existing export proofs on a different build/machine do not settle this report. |
| F4 | Playing the physical MIDI keyboard produced no visual key feedback on the on-screen keyboard. | Show played notes on the corresponding on-screen keys. Screenshot diagnostics identify SV-2 1 KEYBOARD; routing, octave range and affected instrument are unknown. |
| F5 | Play resumes from the paused position. | Provide an explicit way to restart from the beginning, such as a toggle or equivalent control. A separate return-to-start button was an agent suggestion, not an approved design. Per-track versus global scope remains to be settled. |
| F6 | DecentSampler VST3 PARAMS shows sliders without labels. Screenshot includes named controls, unnamed 0.00 sliders and +2103 more params. | Hide unused UI slots if they are placeholders; make real exposed parameters identifiable. The owner suggested hiding surplus controls. Whether these are placeholders or plugin-reported unnamed parameters is unknown. |
| F7 | Switching plugins after tweaking their settings caused a freeze. The tester suspected endless rescanning; restart restored operation. | Plugin switching must complete or recover with a useful error. Diagnostics showed scanning... and no plugin loaded, but scanning... was already present before the freeze, so it does not prove a new scan. Plugin sequence, editor state, settings changed, logs and whether the whole UI froze are unknown. |
| F8 | The tester heard delay in the non-ASIO build. Settings screenshot shows a 64-frame plugin buffer and warns that the audio driver chooses its own device buffer. | Measure and reduce avoidable latency on the affected MIDI/plugin or input path. The affected path is unknown; lack of ASIO is a hypothesis, not a diagnosis. Plugin block size is not total audible latency. |
| F9 | A toast reported inability to start live input because the plugin has no audio input bus and a synth cannot be armed. No subsequent bug was reported. | Make unsupported live-input actions clear and avoid confusing failure feedback. Determine what action triggered the toast; GO LIVE was only an agent hypothesis. The repeated screenshot adds no separate incident. |
| F10 | AUTO 50 is an unclear name. | Use wording that explains the mode and what the number controls. Final label is undecided. |
| F11 | The owner wants the click audible whenever armed and CLICK is enabled. | Include the armed waiting state in audible-click behavior. This is a requested change to the current transport-mode policy described in STATUS Stop 3, not implemented behavior. Check recording arm/wait states, including AUTO; preserve a clear distinction from native input GO LIVE. |
| F12 | The owner asks about downloadable releases, possibly ASIO and non-ASIO variants, after the tester built the app manually. | Supply ready-to-run Windows downloads. Existing distribution decision is an ASIO build with WASAPI fallback, owned by docs/plans/release-prep.md. Two separate downloads are a question, not an approved change. A reported successful manual build is not independent verification of the clean-machine README path. |
| F13 | Audio Settings' output reads "System default" every time it reopens; the tester wants to route the sound to a chosen output, as Ableton does. Screenshot: WASAPI, input Line (MG-XU), output list open. | One output pick routes everything. Reproduced on the dev PC: the pick was saved but the dropdown lost it (input too), and the pick steered only the plugin's native monitor while loops, synths and click followed the Windows default. |
| F14 | A one-bar first take locks every later track to one bar. | A later track longer than the master: extend the loop, keeping the one-bar track repeating across it (an RC-505-style multiply). Waits for the native engine (`docs/plans/native-engine.md` § After the flip). |
| F15 | No effects (delay, reverb) before recording into a track. | An elegant pre-record FX. Waits for the native engine, where input, monitor and record tap share one callback (`docs/plans/native-engine.md` § After the flip). |
| F16 | No control over a recorded track's bar count after the fact. | Adjust the length of a committed track. Design open; waits for the native engine (`docs/plans/native-engine.md` § After the flip). |

## Code reading at `d17c777`

Static reading only: it says what the code does, not what happened on the tester's machine. Rows
marked ✓ were re-read by the orchestrating session; the rest are reader findings with citations,
to be re-checked before they steer a change.

| ID | What the code does | Kind |
|---|---|---|
| F1 | The bar meter renders only in FIXED (`Transport.tsx:250`); FIXED and +/- disable once the first loop locks BPM (`:244,255,267`). FIXED governs the first take only (`machine.ts:280`). | Tied to F2 |
| F2 ✓ | Every take after the first gets the full master window; a shorter take is zero-filled and its length set to the master (`machine.ts:221-223`). No per-track shorter loop exists. | Missing feature; needs a design |
| F3 ✓ | Export builds ONE `.zip` (stems + master + `session.json`) and hands it to the WebView as an `<a download>` click (`export.ts:17-28`): no save dialog, no success message, no result. The button's aria-label says "WAV files". Capabilities grant no dialog/fs. | Small fix; a real save dialog is a separate decision |
| F4 | MIDI reaches `inputRouter` (`midi.ts:48-63`), but nothing feeds the on-screen keyboard's local `downNotes` (`Keyboard.tsx:54,84`). Visible range C4–C7. | Missing feature, small |
| F5 | PLAY on a STOPPED track computes its offset from the master phase (`machine.ts:809-810`); ALL PLAY does the same per track. No restart path exists. | Design: global vs per track |
| F6 | The first 12 params show unfiltered (`PluginControls.tsx:31,256`); names come straight from the plugin's `info.title` with no fallback (`vst3.rs:1848-1879`). Empty names are plugin-reported, not UI placeholders. | Small fix |
| F7 ✓ | No specific deadlock identified. The swap sequence has waits without a time limit: `owner_join.join()` (`clap.rs:1030`), editor teardown + message pump (`vst3.rs:893`, `editor_window.rs:244`), cpal WASAPI stream drop, a VST3 restart joining RT with the editor open (`vst3.rs:1184`), IPC with no frontend timeout. `scanning` clears only when the scan IPC returns (`instrument.ts:311`), so a long or hung scan explains an indicator that predates the freeze. | Needs runtime: log or repro |
| F8 ✓ | A synth plugin can never reach the native monitor: `goLive` arms input first, which rejects a plugin without an input bus (`native-io.ts:85-87`). MIDI → plugin therefore always takes the WebView path: ~30 ms bridge-queue setpoint (`TARGET_FILL_SECONDS`, `transport.rs`), 128-frame worklet, Web Audio output. ASIO would not shorten that path. Native WASAPI is shared-mode, `BufferSize::Default` (`audio_output.rs:17`). | Architecture; answered by the native engine (§ Work order, item 3) |
| F9 ✓ | The toast is reachable without a GO LIVE click: a fresh pick auto-starts live input when the scan says `isEffect` (`PluginControls.tsx:160-166`), and the scan derives that from VST3 subCategories, not the actual input bus (`scan.rs:683`). Message site: `native_io.rs:201`. | Small fix |
| F10 | The number is AUTO REC sensitivity, 1–100 → −12…−60 dBFS RMS (`auto-record.ts:8-19`); label built at `Transport.tsx:304`. | Wording only |
| F11 ✓ | Ordinary ARMED already clicks with CLICK on; only AUTO LISTENING is excluded from the click gate (`state.ts:318`). AUTO exists for the first take only (`machine.ts` `beginAutoRecording`), so no grid exists while it listens. | No change: see § Work order |

## Landed in code, awaiting the tester's machine

Proven in the browser tier only (`pnpm check`, `pnpm build`, `pnpm verify:jam`, `first-session`,
`midi-note-ownership`); nothing here was run in the Tauri app or on the tester's machine.

- **F4:** the on-screen keys mirror the router's held set (`inputRouter.onHeldChange`), so MIDI lights
  keys and GM pads. Computer keys now own their note per physical key.
- **F3:** export shows a success toast naming the `.zip` and pointing at Downloads; the label says
  zip. Where WebView2 actually puts the file is unverified. A real save dialog stays a separate call.
- **F6:** the PARAMS preview takes the first 12 NAMED params; unnamed ones count toward "+N more".
- **F9:** the auto go-live after a fresh pick is quiet (log line kept); a clicked GO LIVE still reports.
- **F2 + F1 + F5:** short later takes tile across the master (early stop or FIXED), FIXED stays usable
  after the BPM lock, and PLAY on an idle transport starts from the top. `pnpm verify:jam` drives it
  through the real dispatchers (Windows browser run, 2026-09-21: 91/91).
  `verify/probes/playback-restart.mjs` separately captures rendered lane PCM: single/ALL idle restarts
  begin at source frame zero, ALL lanes share a start frame, and live joins keep phase beside a
  muted lane pending END STOP. A deliberately wrong 100 ms source offset fails all three cases.
  By ear, unheard: the tile seams, a 3-over-8 cut, reverse on a tiled track, and the downbeat click
  on a from-the-top start.
- **F10:** the button reads `AUTO REC · SENS n` with an explanatory tooltip. Wording is the owner's eye.
- **F13:** the dropdowns show the saved pick, and the WebView's output follows the output pick by
  endpoint name (`applyWebOutput` in `src/audio/audio-devices.ts`, the match in
  `src/audio/output-match.ts`); under ASIO it stays on the system default. Verified in the Tauri app on
  the dev PC with a throwaway probe: the saved pick shows on reopen and the context's sink follows it.
  The tester's first build toasted "Loops and synths stay on the previous output" for a USB headset. Two
  causes, both fixed: WebView2 hides device labels until the session holds a mic grant (measured in a
  fresh profile: every label empty, and the grant does not survive a relaunch), and Chromium appends
  ` (vid:pid)` to a USB-class device's label. The dev PC's Focusrite runs a vendor driver and gets no
  suffix, which is why it passed. Re-verified in a fresh profile: labels hidden, and the sink still
  followed the pick. Unverified: the tester's machine, and a switch between two physical devices.

## Work order (owner-approved)

1. **F7.** No tester log is coming; a further report arrives as an issue. Dev-PC repro
   (`swap-stress` probe, WASAPI, 6 plugins, 60 in-place swaps, editor closed and open): no hang,
   60/60 completed, no fault lines. Found and fixed: every VST3 unload waited out the owner's idle
   `recv_timeout` (~1.2 s per unload or swap, no busy indication); unload now wakes the owner
   (17-90 ms measured). Still open: Archetype Plini (VST3) stalled 4-14 s in 5 of 20 unloads in the
   first run, all with the editor closed, and in 0 of 44 in three later runs the same evening;
   cause unknown, not reproduced since. The VST3 teardown and the unload now log per-step timing
   (release log included), so the next occurrence names its step. The slot now shows "Updating…"
   and disables its source/plugin controls while operations run or wait in the queue;
   the other slot stays usable. Deferred-operation browser checks cover errors, queued work and
   automatic GO LIVE. A Windows native probe verified pending/unlock through CLAP load, VST3 swap,
   unload, failed load and retry, with PCM consumed after successful loads. This does not resolve
   the intermittent Plini stall; when it recurs, read the teardown line. DecentSampler is not
   installed on the dev PC and was not tested.
2. **F2 + F1 + F5 + F11: design approved by the owner and built.** Reference read: the RC-505 MK II
   Parameter Guide. Per-track MEASURE is AUTO (= the first-recorded track), FREE ("set
   automatically, corresponding to the length of the recording") or a pre-set number; with LOOP
   SYNC on a track "retriggers at the beginning of the first-recorded phrase", and a record stop is
   quantized to the measure. The guide does not say what happens when the first track is shorter
   than a later one: unknown, and out of scope here (a take is never longer than the master).
   - **F2, short takes are TILED at commit.** A later take's length is a whole number of bars, at
     most the master. At commit the take is repeated across the master-length region of `record`
     and cut at the master boundary (3 over 8 sounds 3+3+2: the retrigger). `lengthFrames` stays
     the master, so playback, overdub, undo, reverse, export, session and waveform keep their one
     length. Known limits, accepted: an overdub on a tiled track spans the whole master (it does
     not repeat per tile), and the take's own length is not kept after commit.
   - **The length comes from either gesture.** Stopping early keeps the whole bars COMPLETED at the
     press (`planFreeStop`'s wall-clock floor with its quarter-beat grace, measured from the take's
     musical start, the boundary without C): a stop at 1.5 bars keeps one bar and commits at once.
     A stop inside the first bar records on to the bar line. Silence is never read as a phrase
     boundary. FIXED pre-selects the length: the capture ends by itself.
   - **F1:** FIXED and its bar meter stay usable after the BPM lock and then mean "length of the
     next take", clamped to the master's bar count. FIXED off = the stop decides.
   - **RETAKE rolls at the master length once a master exists,** whatever FIXED says: its pass
     edges and the lane handoff seam assume master boundaries. A stop that `planRetakeStop`
     resolves as `stop-now` is an ordinary stop and follows the rule above.
   - **F5:** when no track is PLAYING (a pending END STOP counts as playing; mute does not count) and
     nothing records, PLAY (one track or ALL) starts from the top: ONE shared start time, the master
     grid and pulse re-anchored to it. While anything plays, PLAY joins at the live phase as today.
   - **F11: no code change.** Ordinary ARMED already clicks (`state.ts` `publish`). AUTO listening
     stays silent: AUTO exists only for the first take, so no grid exists to click on, and through a
     mic the click could trigger the take.
3. **F8: answered 2026-09-24** by the native engine (plugins and MIDI in the device callback,
   `docs/plans/native-engine.md` Stage 4); nothing on the shipping line. F12 is the plan's Stage 0
   (the first published release, `docs/plans/release-prep.md`).

Preserve the complete intake while fixing one issue at a time. The tester's machine remains the
final confirmation for its reported failures.
