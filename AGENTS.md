# AGENTS.md

Rules and routers only. **Doc ownership:** AGENTS = what you must not violate · ARCHITECTURE = why ·
VERIFY = how to prove it · `STATUS.md` = what is open. `CLAUDE.md` is a one-line adapter importing
this file.

**BleepLoop** — a Windows desktop instrument: an instrument host (two native-VST slots + six
built-in synths) over an RC-505 MK II–style 5-track looper, all in one native audio engine, shipped
as a Tauri v2 app (Rust + WebView2). Phases P0–P11 are done and LIVE on `main`; v0.1.0 is the first
release.

## Read before you work — area briefings

Harness auto-load of nested files is not reliable: open the briefing yourself.

| Before touching… | Read |
|---|---|
| `src/audio/` — engine, clock, metronome/click, timing, looper, capture, latency, MIDI/live notes | `src/audio/AGENTS.md` |
| Any frontend or looper-UI work: `src/ui/`, `src/app.css`, `src/app.tsx` | `src/ui/AGENTS.md` |
| `src-tauri/` — anything Rust/native, ASIO, plugin hosting, Cargo dependency/lockfile updates | `src-tauri/AGENTS.md` |
| Verification code, the verify rig, interpreting a gate result, a docs-guard failure | `verify/README.md` |
| Any runtime verification (browser probe, `__lf`, `tauri dev`, Mac-vs-PC) | `docs/VERIFY.md` |
| Any non-trivial work; `engine.ts`, bus wiring | `docs/ARCHITECTURE.md` |
| Planning or performing a by-ear/eye/rig session; gate-adjacent code; latency | `STATUS.md` |
| Releases, licences, workflow hardening | `docs/plans/release-prep.md` |
| The native audio engine: the decided direction, its stages, gates and owner decisions | `docs/plans/native-engine.md` |
| The hands-free looper (landed) and what is explicitly not built | `docs/plans/pedalboard.md` |
| The open work order: the recorder-session jam and the looper fixes queued behind it | `docs/plans/gates-and-hygiene.md` |

## Standing rules

- ⚠ **A by-ear "this is off" outranks every green check in this repo** — re-scope the work, don't
  defend the gates. The headless proofs settle neither the looper↔click question nor
  guitar-through-ASIO-into-a-loop.
- **`STATUS.md` is ONE ordered rig lap + a decision table,** short enough for one sitting; agents
  leave those stops to the owner. Past 10 stops the docs guard goes red: consolidate related stops
  or flag it to the owner rather than appending. An audit finding lands as a machine-verified
  commit or as one line in `docs/backlog-taste.md` (taste, not a gate); one that truly needs the
  rig merges into an existing stop.
- **This is a public repo: keep working documents few and short-lived.** A plan or spec may live
  under `docs/` while the work is open. When it lands, fold what still binds into a briefing,
  `docs/ARCHITECTURE.md`, `STATUS.md`, `docs/backlog-taste.md` or a comment at the call site, and
  delete the document. Audits are not kept in the tree; their findings land as described above.
- **Ear-gated work drains slowly — design around it.** Prefer work a machine gate or a probe can
  settle. Rank unbuilt work by provenance: the owner's ear > the owner's stated roadmap > an agent's
  tier list.
- **The play path is guitar → amp-sim plugin (native monitor) → play/loop/dub at low latency.** The
  looper is the instrument. MIDI controller → synth/plugin is the second path, for the other layers;
  its MIDI arrives through the WebView (Web MIDI); PC-keyboard→MIDI is its fallback. By-ear sessions happen on
  guitar. The on-screen keyboard stays available, but it is not the first-screen hero. The promise
  heads `README.md`; the not-built list lives in `docs/plans/pedalboard.md`.
- **One native audio engine is the default** (`docs/plans/native-engine.md`, flipped for v0.1.0). The
  web path behind the Audio Settings switch takes fixes only until Stage 6 deletes it: no new work in
  the plugin bridge, record compensation, the Web Audio looper, synths or FX; new features are built
  in the engine.
- **The browser tier is a VERIFICATION RIG, not a product.** BleepLoop ships as a standalone
  Windows app with native drivers and zero-latency monitoring.
- **Measure latency changes on the path they change,** with signal/timestamp probes before and
  after. Replacing native monitoring or changing its buffering targets requires the L1+L2
  measurements in `docs/ARCHITECTURE.md` § Decided: one native audio engine.
- **Before editing `src/audio/`, check for the dev app (`app` process)**; if it runs, ask for it to
  be closed and wait — hot-reload stacks a second audio engine on the live one.
- **Verify by driving the running app and measuring** — a typecheck, a code read or a subagent's
  self-report is a claim: run the runtime probe yourself (`docs/VERIFY.md`).
- **`main` is the live dev line.** Commit and push whenever the gates are green: `pnpm check` (also the pre-push hook), `pnpm build`, `cargo check`
  asio/no-asio, CI; plus `pnpm verify:jam` after looper/capture/state-machine changes (off the push
  gate; scheduling it is the owner's call). Never push red. The open by-ear/eye/rig gates live ON
  `main`.
- **Tracked files are impersonal and secret-free:** roles ("the owner"), never a person's name,
  verbatim speech or an email address. Development happens on a Mac (no Rust toolchain) and a
  Windows PC, and agent memory does not sync: a durable fact goes in a tracked file.
- **Keep this file a router.** Rules are written BARE — provenance lives in the owner doc. A
  LANDED feature becomes a one-line pointer; keep only what is still active or a recurring gotcha;
  area detail goes in the area briefing. A cut that
  claims "owned elsewhere" quotes the owner line in its commit message.

## Commands — what `package.json` does not say

- `pnpm dev` serves the frontend only, on http://localhost:1420 (strictPort; NOT 5173).
  `pnpm dev:asio` is the by-ear dev command; `pnpm dev:wasapi` iterates faster without the ASIO SDK,
  at higher latency; `pnpm build:app` makes the prod-realistic standalone exe. `app.exe --disable-asio`
  (dev: `pnpm dev:asio -- -- --disable-asio`) starts without touching any ASIO driver.
- ASIO is a cargo opt-in feature (owner: `src-tauri/AGENTS.md`); a plain `cargo build` must work
  without the SDK.
- Known intermittent red on the Mac: if the pre-push hook goes red with no FAIL line, re-run once
  before digging.
- `pnpm dlx knip` finds unused files/exports/deps; an export kept only for a browser probe carries
  `@public` in its JSDoc. The browser probes need `pnpm exec playwright install chromium` once.
- There is no JS unit-test runner. `pnpm verify` = the guards (real source in plain Node + the docs
  guard); `pnpm probe <name>|--ci` = the browser probes, each on its own Vite; `pnpm verify:jam` = the
  golden jam, the real app headless, frame by frame. What each can and cannot see: `verify/README.md`.

## Architecture in one paragraph

The app runs on one native audio engine by default: `src-tauri/crates/lf-engine` (pure, briefing in
its `lib.rs`) and its device side `src-tauri/src/engine_io` (briefing in its `mod.rs`); the WebView
sends commands and reads a JSON feed (`docs/plans/native-engine.md` § Stage 5). `src/platform/` is the
ONLY place allowed to import `@tauri-apps/*`; `audio/` and `ui/` depend on its interfaces, never the
reverse; UI components reach audio through `src/ui/state/audio.ts` (guarded). Live audio never
crosses that boundary as PCM; a session save's snapshot does, once, off the RT path. Frontend
`console.error` + uncaught errors feed the release log (`src/platform/logging.ts`): keep every
`console.error` site. Stack: SolidJS + TypeScript + Vite 8 (rolldown/oxc — esbuild is gone), Rust +
cpal + the CLAP/VST3 hosts. The web path (Web Audio, Tone.js, ringbuf.js, the plugin bridge) stays
behind the Audio Settings switch and runs the browser build until Stage 6; the invariants below and
`docs/ARCHITECTURE.md` still describe that path until Stage 6 rewrites them.

## Invariants — titles only; `docs/ARCHITECTURE.md` owns the text

In-code comments cite these by number. This list and ARCHITECTURE's share one order: change them
together.

1. The Web Audio `AudioContext` is the single tempo/quantization authority.
2. Never touch `Tone` before the engine has run `setContext` (`clock.ts`'s `tp()` helper).
3. COOP `same-origin` + COEP `require-corp` → `crossOriginIsolated` → SharedArrayBuffer.
4. `?worker&url` for first-party TS AudioWorklets.
5. No allocation in `AudioWorkletProcessor.process()`.
6. No Solid signal WRITES from audio-path timers, and no signal READS in the 60 fps draw loop.
7. All `@tauri-apps/*` confined to `src/platform/` (CI-guarded).

## Open threads (non-gate)

Everything owed an ear/eye/rig check or an owner decision lives in `STATUS.md`. Besides that:
**08-11 residuals** — the LoadState timeout (documented at its command fn; needs a design, not a
token) and the check-then-set race note in the R1 commit message. **Native host residuals** (from
the 2026-09-23 audit; no gate): `src-tauri/AGENTS.md` § Open threads.
