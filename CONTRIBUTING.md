# Contributing

One person maintains BleepLoop. Bug reports, rig reports ("it crackles on
interface X at buffer Y") and pull requests are welcome.

## Pull requests

1. Fork, branch from `main`, open the PR against `main`.
2. `pnpm install`, then make `pnpm check` pass (typecheck, lint, the capability-boundary guard and the
   deterministic `verify/` guards). It is also the pre-push hook. The engine's tests are
   `pnpm test:engine` (`cargo test -p lf-engine`); CI runs the Rust tests only on a change that
   reaches their path filters (mostly `src-tauri/`; the exact lists are in `.github/workflows/ci.yml`
   and `.github/workflows/rust-test.yml`), so run them yourself for anything they cover.
3. CI runs the browser probes (`pnpm probe --ci`); `pnpm probe <name>` runs one locally. Both need
   `pnpm exec playwright install chromium` once.
4. If you touched `src-tauri/`, run `cargo check` in `src-tauri/` (Windows). The ASIO feature is
   optional and needs your own copy of the Steinberg SDK, see `THIRD-PARTY-NOTICES.md`.

Keep changes small and say how you verified them. A change to timing or latency lands on its
machine measurements (`docs/ARCHITECTURE.md` names the ones a latency change needs), not on a jam:
what only an ear can judge goes into `STATUS.md` as unheard, and a later "this is off" reopens it.

## Finding your way around

`README.md` covers running and building. `docs/ARCHITECTURE.md` holds the design decisions and the
numbered invariants the code comments cite. `docs/VERIFY.md` explains how to drive and measure the
running app. `AGENTS.md`, the nested `AGENTS.md` briefings and `STATUS.md` are the
maintainer's working notes, written for the AI coding agents used on this project. They give
context, and you can skip them. The "direct push to `main`" rule in there applies to the maintainer only.

## Licence

By contributing you agree that your contribution is licensed under the MIT licence in `LICENSE`.
