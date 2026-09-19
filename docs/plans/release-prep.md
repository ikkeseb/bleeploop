# Release prep (OPEN: no Windows download is offered yet)

The repo is public. What is left before a downloadable Windows build, and the decisions a release
rests on. This file goes when the first release is published; fold what still binds into
`THIRD-PARTY-NOTICES.md`, `src-tauri/AGENTS.md` or a comment in `build-exe.yml` first.

Rules with no other home yet (move them before deleting this file): every workflow runs with `permissions: contents: read`, every
action is pinned by commit sha with the major tag as a trailing comment (bump both together), and
checkout runs with `persist-credentials: false`. The repo started from one squashed commit, so no
tracked doc may cite an earlier commit sha (the docs guard checks that cited shas resolve). A
build from the README on a clean Windows machine is unverified.

Built and machine-verified:

- **Release workflow** (`build-exe.yml`): builds the ASIO NSIS installer + the bare exe. A `v*` tag, or
  a dispatch with `release_tag`, stages a DRAFT GitHub Release (installer, `SHA256SUMS.txt`, GPLv3 and
  MPL-2.0 texts, `THIRD-PARTY-NOTICES.md`, `SOURCE.txt` naming the exact commit; notes carry the
  SmartScreen warning). Publishing the draft is a manual click. The tag must equal
  `v<tauri.conf.json version>`. Only the `release` job has `contents: write`, and it runs no repo code.
  `src-tauri/tauri.release.conf.json` is the overlay that puts the GPLv3 licence page in the installer
  and the licence files beside the exe; a local trial build carried all of them (7z listing).
- **Licence route** (owner's decision): repo MIT, release exe built with `--features asio` and
  licensed GPLv3, WASAPI the in-app fallback. Basis, read from the SDK (2.3.4): quoted in
  `THIRD-PARTY-NOTICES.md`. Nothing in the SDK speaks to an MIT repo + GPLv3 binary; that rests on
  ordinary GPL compatibility. The SDK is fetched from Steinberg at build time (version + sha256
  pinned), never committed.
- **ASIO trademark** (Steinberg Usage Guidelines 1b/1c/1e/1f/15): the unaltered ASIO Compatible Logo
  sits in Help ("About this build", the About-box equivalent) only, and only when the build can offer
  ASIO: ASIO is on by default there, so 1f asks for the About box, not the settings dialog. Kept
  small by the owner's call (the guidelines set no minimum size): the SDK's white-on-transparent
  variant in the app with the trademark line as text beside it (section 14), the opaque JPG in the
  README and the release notes; "ASIO®" on first use. The artwork is committed by the owner's
  decision although the SDK licence is silent on redistributing it:
  `src/assets/third-party/NOTICE.md`. "ASIO" may never be part of the product name.
- **MPL-2.0** (`ringbuf.js`): licence text + source pointer ship with the installer and the release.
- **CSP** set in `tauri.conf.json`; the WebView2 permission auto-grant is narrowed to the app's own
  origin and the MIDI/microphone kinds. Probed in the real release exe (a temporary startup probe
  driving `engine.start`, `capture.init`, `midi.start`): context running, capture worklet loaded,
  `crossOriginIsolated` true, MIDI access resolved, no violation. CSP violations now reach the release
  log as `[csp]` lines. Not probed: the plugin PCM worklet with a plugin loaded, plugin editors,
  session export, a connected MIDI device.

Still open:

- **Hear the first CI-built exe on the rig before offering it.** The runner proves compile + link
  only; the rig has only ever heard SDK 2.3.3.
- Unsigned builds trigger SmartScreen (said in the release notes); code signing is not planned.
