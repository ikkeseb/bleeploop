# First external tester feedback (OPEN)

Owner-requested behavior and tester reports collected on 2026-09-19. This is an intake plan,
not a diagnosis or a record of verified fixes. Testing is ongoing; fold new feedback into the
matching item. When resolved, move enduring decisions to their owning briefings and delete this plan.

## Evidence and scope

The tester reports building a Windows `app.exe` with Rust, without ASIO, and running it with a
physical MIDI keyboard. Exact commit, build command, audio device/driver configuration and plugin
versions are unknown. Screenshots and the owner's account establish the observations below;
this session has not reproduced them or inspected implementation. Screenshots are temporary,
uncommitted attachments; their relevant contents are transcribed here.

The current request authorizes documentation, commit and handoff only. Product changes need a
subsequent work order. All items remain open; suggested investigation order is plugin freeze,
loop behavior and export, then the remaining interaction issues. This order is not an owner decision.

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

## Continuation

Start with a bounded reproduction of F7 on Windows using the native-host and runtime briefings.
Obtain the tester's exact build revision, plugin/version sequence and log around the freeze when
available. Distinguish a host hang from a stale scan indicator before changing either subsystem.
For F2, reproduce the recording and stop sequence and measure captured length, silence and repeat
period; do not infer a trim rule from the screenshot. Preserve the complete intake while fixing
one issue at a time. The tester's machine remains the final confirmation for its reported failures.
