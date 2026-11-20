# docs/redesign: the accepted redesign, as references for the build (a working document)

The mockups the owner accepted for the redesign (2026-11-20), copied here while the build is open.
When the build lands, what still binds moves into `src/app.css`'s header, `src/ui/AGENTS.md` and
`docs/ARCHITECTURE.md`, and this folder is deleted (root `AGENTS.md`, working documents).

- `direction.md`: the design's idea in words. Where it and a mockup differ, the mockup wins.
- `main.html|css|js`: the main screen at 1920x1080. `?w=<take>` picks the wave: `m` (default) the
  stripes with a soft vertical gradient, `a` the dense fine wave; the others are takes that were
  turned down, kept for reference. The wave is a half-open point: the app keeps the stripes as the
  default and the dense wave, the light bars and today's plain body as alternates.
- `wave-lightbars.js`: an earlier pass of `main.js` whose wave is the light bars (its paint B, line
  195), kept for that one function; it is not loaded by any page here.
- `narrow.html|css|js`: the same screen at 1000x700 (the build goes down to 960x600).
- `controls.html|css|js`: the control sheet: every key, LED, value key, armed key, fader, knob, meter.
- `ott.html|css|js`, `ott-open.html`: the OTT face, alone and open under the slot row.
- `assets/wave.js`: made-up audio for the mockups only. `fonts/`: Instrument Sans (OFL-1.1), the
  same file the app vendors.

Open a page straight from disk in a browser; nothing here is built or served by the app.
