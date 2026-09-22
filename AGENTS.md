# AGENTS.md

Rules and routers only. **Doc ownership:** AGENTS = what you must not violate · ARCHITECTURE = why ·
VERIFY = how to prove it · `STATUS.md` = what is open. `CLAUDE.md` is a one-line adapter importing
this file.

**BleepLoop** — a Windows desktop instrument: an instrument host (two native-VST slots + six
built-in Web Audio synths) over an RC-505 MK II–style 5-track looper, shipped as a Tauri v2 app
(Rust + WebView2). Phases P0–P11 are done and LIVE on `main`.

## Read before you work — area briefings

Harness auto-load of nested files is not reliable: open the briefing yourself.

| Before touching… | Read |
|---|---|
| `src/audio/` — engine, clock, metronome/click, timing, looper, capture, latency, MIDI/live notes | `src/audio/AGENTS.md` |
| Any frontend or looper-UI work: `src/ui/`, `src/app.css`, `src/app.tsx` | `src/ui/AGENTS.md` |
| `src-tauri/` — anything Rust/native, ASIO, plugin hosting, Cargo dependency/lockfile updates | `src-tauri/AGENTS.md` |
| Verification code, interpreting a gate result, the MIRRORS drift canary, a docs-guard failure | `verify/README.md` |
| Any runtime verification (browser probe, `__lf`, `tauri dev`, Mac-vs-PC) | `docs/VERIFY.md` |
| Any non-trivial work; `engine.ts`, bus wiring | `docs/ARCHITECTURE.md` |
| Planning or performing a by-ear/eye/rig session; gate-adjacent code; latency | `STATUS.md` |
| Releases, licences, workflow hardening | `docs/plans/release-prep.md` |
| The looper-UI spec (the approved mockup) | `docs/inspiration/revamp-2026-07/README.md` |

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
- **The play path is MIDI controller + guitar → plugin/synth → play/loop/dub at low latency.** The
  looper is the instrument; PC-keyboard→MIDI is a *fallback*. Guitar and MIDI/keys are equal play
  paths: by-ear sessions happen on guitar only, and the on-screen keyboard stays a first-class view.
- **The browser tier is a VERIFICATION RIG, not a product.** BleepLoop ships as a standalone
  Windows app with native drivers and zero-latency monitoring.
- **Measure latency changes on the path they change,** with signal/timestamp probes before and
  after. Replacing native monitoring or changing its buffering targets requires the L1+L2
  measurements in `docs/ARCHITECTURE.md` § Decided: the looper stays in Web Audio.
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
- ASIO is a cargo OPT-IN feature carried by the npm scripts: a plain `cargo build` must work
  without the LLVM/ASIO SDK, because the repo ships to others.
- Known intermittent red on the Mac: if the pre-push hook goes red with no FAIL line, re-run once
  before digging.
- `pnpm dlx knip` finds unused files/exports/deps; an export kept only for a browser probe carries
  `@public` in its JSDoc. `pnpm verify:jam` needs `pnpm exec playwright install chromium` once.
- There is no JS unit-test runner. `pnpm verify` = pure logic + the docs guard; `pnpm verify:jam` =
  the real app headless, frame-by-frame. What each can and cannot see: `verify/README.md`.

## Architecture in one paragraph

Native VST hosting is the *only* thing that forces Tauri/Rust; everything else is pure Web Audio /
TypeScript and runs standalone in a browser. `src/platform/` is the ONLY place allowed to import
`@tauri-apps/*`; `audio/` and `ui/` depend on its interfaces, never the reverse. **Audio buffers
never cross that boundary as PCM** — native audio reaches the Web Audio graph only as an
*AudioNode*. Frontend `console.error` + uncaught errors feed the release log
(`src/platform/logging.ts`): keep every `console.error` site. Stack: SolidJS + TypeScript + Vite 8
(rolldown/oxc — esbuild is gone) + Tone.js + ringbuf.js.

## Invariants — titles only; `docs/ARCHITECTURE.md` owns the text

In-code comments cite these by number. This list and ARCHITECTURE's share one order: change them
together.

1. The Web Audio `AudioContext` is the single tempo/quantization authority.
2. Never touch `Tone` before the engine has run `setContext` (`clock.ts`'s `tp()` helper).
3. COOP `same-origin` + COEP `require-corp` → `crossOriginIsolated` → SharedArrayBuffer.
4. `?worker&url` for first-party TS AudioWorklets.
5. No allocation in `AudioWorkletProcessor.process()`.
6. No Solid signal WRITES from audio-path timers, no signal READS in the 60 fps draw loop.
7. All `@tauri-apps/*` confined to `src/platform/` (CI-guarded).

## Open threads (non-gate)

Everything owed an ear/eye/rig check or an owner decision lives in `STATUS.md`. Besides that:
**08-11 residuals** — the LoadState timeout (documented at its command fn; needs a design, not a
token) and the check-then-set race note in the R1 commit message.
