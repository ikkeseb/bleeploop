# First external tester feedback (OPEN)

Owner-requested behavior and tester reports collected on 2026-09-19. This is an intake plan,
not a diagnosis or a record of verified fixes. Testing is ongoing; fold new feedback into the
matching item. When resolved, move enduring decisions to their owning briefings and delete this plan.

## Evidence and scope

The tester reports building a Windows `app.exe` with Rust, without ASIO, and running it with a
physical MIDI keyboard. Exact commit, build command, audio device/driver configuration and plugin
versions are unknown. Screenshots and the owner's account establish the observations below;
nothing has been reproduced at runtime. Screenshots are temporary, uncommitted attachments; their
relevant contents are transcribed here.

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
| F8 ✓ | A synth plugin can never reach the native monitor: `goLive` arms input first, which rejects a plugin without an input bus (`native-io.ts:85-87`). MIDI → plugin therefore always takes the WebView path: 5 ms drain, ~30 ms hop-2 setpoint (`transport.rs:401`), 128-frame worklet, Web Audio output. ASIO would not shorten that path. Native WASAPI is shared-mode, `BufferSize::Default` (`audio_output.rs:17`). | Architecture; owner roadmap call, measure first |
| F9 ✓ | The toast is reachable without a GO LIVE click: a fresh pick auto-starts live input when the scan says `isEffect` (`PluginControls.tsx:160-166`), and the scan derives that from VST3 subCategories, not the actual input bus (`scan.rs:683`). Message site: `native_io.rs:201`. | Small fix |
| F10 | The number is AUTO REC sensitivity, 1–100 → −12…−60 dBFS RMS (`auto-record.ts:8-19`); label built at `Transport.tsx:304`. | Wording only |
| F11 ✓ | Ordinary ARMED already clicks with CLICK on; only AUTO LISTENING is excluded from the click gate (`state.ts:318`). | One-line gate change + `pnpm verify:jam` |

## Landed in code, awaiting the tester's machine

Proven in the browser tier only (`pnpm check`, `pnpm build`, `pnpm verify:jam`, `first-session`,
`midi-note-ownership`); nothing here was run in the Tauri app or on the tester's machine.

- **F4:** the on-screen keys mirror the router's held set (`inputRouter.onHeldChange`), so MIDI lights
  keys and GM pads. Computer keys now own their note per physical key.
- **F3:** export shows a success toast naming the `.zip` and pointing at Downloads; the label says
  zip. Where WebView2 actually puts the file is unverified. A real save dialog stays a separate call.
- **F6:** the PARAMS preview takes the first 12 NAMED params; unnamed ones count toward "+N more".
- **F9:** the auto go-live after a fresh pick is quiet (log line kept); a clicked GO LIVE still reports.
- **F10:** the button reads `AUTO REC · SENS n` with an explanatory tooltip. Wording is the owner's eye.

## Work order (owner-approved)

1. **F7.** Ask the tester for the release log (`%LOCALAPPDATA%\com.bleeploop.app\logsleeploop.log`)
   and the plugin pair they switched between. Meanwhile run a bounded repro on the dev PC: tweak,
   switch plugin, editor open and closed. DecentSampler is NOT installed on the dev PC (VST3 folder:
   Archetype, FabFilter, Neural DSP, Surge); install it or accept a stand-in. Distinguish a host
   hang from a stale scan indicator before changing either subsystem.
2. **F2 + F1 + F5 + F11 as ONE design conversation with the owner before any build.** Agent starting
   point, not approved: a manually stopped take rounds to whole bars and loops at its own length
   (RC-505 style), which never reads silence as a phrase boundary. Do not infer a trim rule from
   the screenshot. Reader material for that conversation (static reading, RC-505 behaviour unknown):
   - ~30 sites assume track length == master length: playback bounds, overdub sum/swap, undo,
     reverse, RETAKE, END STOP/STOP ALL, export render + session schema + import, waveform, recovery.
   - Smallest honest version: a PRE-CHOSEN length for the next take, limited to whole-bar divisors
     of the master (1/2/4 over 8). It reuses `lengthFrames`, needs no per-track phase anchor, and
     answers F1 by letting the bar meter pick the next take's length after the BPM lock.
   - The free "stop decides" version also needs a per-track phase origin (3 over 8 realigns every 24
     bars), a rounding rule for late/early stops, a RETAKE priority rule, and an export period that
     can reach the LCM of the lengths.
   - F5 is its own product call in either version: a global "from the top" when everything is
     stopped is cheap; a per-track restart while others play needs a boundary or a phase anchor.
   - **F11 is not a one-line gate change.** In AUTO LISTENING no grid exists yet, and the pulse
     re-anchors to the detected onset (`machine.ts` `beginAutoRecording`), so a click during
     listening would jump phase when the take starts; through a mic the click could trigger AUTO.
     Ordinary ARMED already clicks.
3. **F8 waits** for an owner decision on a native monitor path for synth plugins; F12 stays with
   `docs/plans/release-prep.md`.

Preserve the complete intake while fixing one issue at a time. The tester's machine remains the
final confirmation for its reported failures.
