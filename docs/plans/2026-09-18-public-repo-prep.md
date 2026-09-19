# 2026-09-18 — Public-repo prep (OPEN: the repo is still private)

Goal: strangers can clone, open PRs and, later, download a Windows build. Going public is spread
over several sessions; this file is the pickup.

## Landed

- **History rewritten** (git-filter-repo, from a fresh clone; final tree identical to the pre-rewrite
  tree except three removed lines). Every commit is `ikkeseb` + the GitHub-verified address; one GitHub
  web-merge committer remains. Scrubbed from all blobs and messages: a second personal address, a full
  name, machine-local user paths, references to a private notes repo, session/artifact links and agent
  attribution trailers. Removed from all history: five third-party UI reference images and
  the harness-local `.claude/settings.json` (removed, now gitignored).
- **All commit shas changed.** Shas cited in tracked docs were remapped; any other clone must be
  re-cloned or hard-reset to `origin/main`.
- Identity rule: commits are authored through the repo git config; no address in a tracked file
  (`AGENTS.md` § Standing rules).
- `.gitignore`: harness-local agent config (shared guidance lives in `AGENTS.md` only), third-party
  reference images, env files, key material, editor dirs, root-level session exports.
- All workflows run with `permissions: contents: read`, every action is pinned by commit sha (the
  major tag stays as a trailing comment — bump both together) and checkout runs with
  `persist-credentials: false`.
- Verified by re-scanning every blob and message reachable from `main` (pattern scan; an independent
  reviewer added an entropy scan and found nothing). Pattern scans cannot prove absence.

## Before flipping to public

1. **Product name: BleepLoop (owner's decision), renamed in the tree.** Checked free of same-name
   software on the App Store, Google Play, GitHub repos, npm, crates.io, web search and TMview (no
   mark contains "bleeploop"; two ended US filings for "BLEEP BLOOP"). The app icon is the
   wordmark's colour-wheel dot, a placeholder until a real logo exists. The `lf.*` storage keys
   stay; import still accepts the earlier session `app` tag.
2. **This repo is the new one: it starts from ONE parentless commit of the final tree** (owner's
   decision), authored with the account's noreply address. The earlier history lives only in an
   off-GitHub bundle; the owner deletes the old private repo (a force-push would not have removed
   its commits from GitHub). Cost: no public `git blame`. No tracked doc may cite a commit sha from
   before the squash: none resolves here and the docs guard checks them.
3. **Settings.** Set while private: Issues on, Wiki/Discussions/Projects off, `GITHUB_TOKEN`
   read-only, Actions cannot approve PRs, Dependabot alerts, topics and description. GitHub only
   allows these on a public repo, so they follow the flip: a ruleset blocking force-push and
   deletion on `main`, private vulnerability reporting (`SECURITY.md` points at it), secret
   scanning with push protection, approval for first-time contributors' workflow runs. `ci.yml`
   skips docs-only PRs (`paths-ignore`), which matters if CI becomes a required check.
4. A build from the README on a clean Windows machine is unverified.

Landed after the first pass: `CONTRIBUTING.md`, `SECURITY.md`, a current README screenshot + clone and
WASAPI build steps, and `THIRD-PARTY-NOTICES.md` corrected against Steinberg's VST 3 licensing page
(MIT since SDK 3.8).

## Before offering a download

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
- A test draft `v0.1.0` (dispatch run, no tag created) sits on the private repo for the owner's eye:
  six assets, notes and target commit as designed. Delete it before the first real tag, or the
  release job fails on the existing name.
- Unsigned builds trigger SmartScreen (said in the release notes); code signing is not planned.

## Decided: a small impersonal status layer, no working documents

`STATUS.md` and `docs/backlog-taste.md` stay on `main` (fresh clones, both dev machines and the docs
guard depend on them); no nested or separate private repo. Every tracked doc and code comment is
impersonal: roles for names, plain English for verbatim speech, no commentary on a person (the rule:
`AGENTS.md` § Standing rules). All audits, landed plans and specs, the 2026-06 archive, the build
plan, the research brief and the mockup history left the tree (owner's decision); what still bound
code moved into `docs/ARCHITECTURE.md`, `docs/VERIFY.md` and the area briefings first. They exist in
the history bundle only. This file is the one plan left, and it goes when the repo is public.
Every remaining tracked `.md` was read line by line for personal content (four readers, coverage
reconciled against `wc -l`); code comments were pattern-scanned only.
