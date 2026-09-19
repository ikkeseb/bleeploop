# Contributing

One person maintains BleepLoop. Bug reports, rig reports ("it crackles on
interface X at buffer Y") and pull requests are welcome.

## Pull requests

1. Fork, branch from `main`, open the PR against `main`.
2. `pnpm install`, then make `pnpm check` pass (typecheck, lint, the capability-boundary guard and the
   deterministic `verify/` guards, about a minute). It is also the pre-push hook.
3. If you touched looper timing, capture or the track state machine, run `pnpm verify:jam` too (needs
   `pnpm exec playwright install chromium` once).
4. If you touched `src-tauri/`, run `cargo check` in `src-tauri/` (Windows). The ASIO feature is
   optional and needs your own copy of the Steinberg SDK, see `THIRD-PARTY-NOTICES.md`.

Keep changes small and say how you verified them. The final test of audio behaviour is the
maintainer's ear on real hardware, so a PR that changes timing or latency may sit until it has been
played.

## Finding your way around

`README.md` covers running and building. `docs/ARCHITECTURE.md` holds the design decisions and the
numbered invariants the code comments cite. `docs/VERIFY.md` explains how to drive and measure the
running app. `AGENTS.md`, the nested `AGENTS.md` briefings and `STATUS.md` are the
maintainer's working notes, written for the AI coding agents used on this project. They give
context, and you can skip them. The "direct push to `main`" rule in there applies to the maintainer only.

## Licence

By contributing you agree that your contribution is licensed under the MIT licence in `LICENSE`.
