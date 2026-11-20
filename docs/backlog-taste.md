# Taste backlog (not a gate)

Feel, wording and placement questions for the owner, answered with the app open: yes, no or change,
and the line leaves. Nothing here blocks a build, a release or a rig stop. Agents add one line per
finding (an audit's taste finding lands here, never in `STATUS.md`), keep the list short
enough for one sitting, and never re-rank the owner's answers. Provenance lives in the commit that
added a line.

## With the app open, `pnpm dev:asio`

- **The empty first screen reads dark (the owner's eye on a 2560 still, 2026-11-10):** a fresh
  install's five empty wave wells are near-black over most of the window; possibly fine as it is.
  Lift an empty well a tone, or leave it?
- **Sample rate row (unseen):** Audio Settings' select under Buffer, "Device (44.1 kHz)" / "44.1 kHz"
  / "48 kHz" (under ASIO only the rates the driver runs; under WASAPI a read-out of the device's own
  rate, no caret, with a hint that Windows sets it). Clear beside Buffer, and is the hint useful?
- **END STOP reads as a record control:** with END STOP on and FIXED off, a stopped free take keeps
  every whole bar, as built (END STOP only stops playback). The owner asks other players before
  deciding on wording or behaviour.
- **Stage view (B): SCOPE's visual direction.** Its additive live columns are the part to keep (a
  locked loop glows, a one-off peaks and dies). A taste review of stills read the rest as a dark-mode
  DAW arrangement view (label gutter, gridlines, a large dim top bay); whether a new design round is
  wanted waits for the owner's eye on the device (`STATUS.md` next jam, 4). A hitch the owner saw came
  under the MIDI benchmark's deliberate 250 ms UI stalls, not yet in an ordinary run. Reference the
  owner flagged for a design round (2026-10-26): Thundercode's concert visuals for Alesso,
  https://thundercode.se/work/alesso/.
- **Input-effect defaults (unheard, an agent's pick):** echo 1/8, feedback 0.4, level 0.5; reverb 0.5. The
  echo's level is scaled so its repeats carry the input's energy (at feedback 0.95 the first echo is
  about ⅓ of the level): does high feedback still feel right? The reverb is summed to mono.
- **A free take past the loop (unheard):** a stop 1.5 loops in keeps two loops, just under keeps one.
  Right by ear, or lean toward the longer take?
- **FADE (unheard):** the level is r² of a linear ramp from the press to the first bar line at or
  after its bars (2 bars pressed mid-bar lasts up to almost 3). A natural ending? Start on the next bar
  instead? The bars ride on FADE's key (a drag or a scroll changes them): is that findable?
- **Command bar with an UPDATE offer (unseen):** where two rows with UPDATE beside the tools would run past
  the window (below about 1080 px), the gaps narrow and UPDATE takes a row over the tools, now three
  columns (`app/cmd-fit.ts` `.cmd--tight`). Findable, and calm enough?
- **From the designer walk, small calls:** the input effects' settings, each in its plate's
  menu: right, once seen in the browser or the dev build? DUB FEEDBACK's slider
  in the one-row FX drawer is 80 px at 1000 px wide: widen the dub module?
- **Tone recall toasts:** a refused tone toasts at every load until the plugin is changed once (the
  saved settings are kept): say that they are kept? An import whose slot holds another plugin toasts
  "This session used X in slot B": enough, or offer to load it?
- **The master meter reads weak (the owner's eye on the running app, 2026-11-20, not urgent):** the
  ladder and its scale (-48 … 0) beside the master knob; which of the two he means is unread.
- **MIDI learn row:** is "latching" clear to a guitarist? A long binding list makes the popover tall.

## By ear, when convenient

- Should the click be ON by default while recording?
- Levels: the click volume, the drum kit's pad balance, the plugin output-gain default.

## Parked ideas (for the long run)

- **The owner's own ideas, not scheduled:** full panel drag, with plugin editors inline, each filling
  its panel (the layout vision of 2026-06-17; the movable, hideable keyboard is built). A meter per
  track (one lane in 4/4 over another in 3/4; 2026-10-15): more than passing a lane its meter
  (`grid.rs` module doc). A BPM change after the loops are recorded (2026-10-26): today the tempo
  locks at the first take's count-in until CLEAR ALL, so the loops would need a real-time stretch in
  the engine; candidates, unevaluated: Signalsmith Stretch (MIT, crates `signalsmith-stretch`,
  `ssstretch`), Bungee (MPL-2.0), Rubber Band (GPL).
- **BleepLoop's own plugins with full faces (the owner, 2026-11-20):** native units with their own
  interface, full plugin-style faces in the manner of Neural DSP's; VST3 is enough for the owner if it is the best format (Ableton Live 12
  loads VST2 and VST3, not CLAP). The chain per slot he asked for the same day is being built
  (`docs/unit-contract.md`), and the OTT face is the start he accepted. An own plugin outside BleepLoop
  can share `lf-engine`'s DSP through a VST3 shell (`nih-plug` is in maintenance mode; its community fork
  and Coupler's MIT/Apache `vst3` bindings are the candidates; the VST 3 SDK is MIT since 3.8).
- **Not built on purpose** (no owner or tester ask; each would add its own lap): a FREE tempo-setting
  first take (it meets the bar math every meter shares, `grid.rs`); more built-in
  synths (they fill layers, they do not compete with
  plugins); a recent-jams shelf (keep the last N recovery archives on ✕ ALL and close, offered in
  IMPORT, M); resampling on import or recovery (S); one `.pill` primitive with type tokens. (The GM-kit grooves
  left this list on the owner's word, 2026-11-20: built last and detachable.)
- **Dropped by the owner (2026-10-07):** a tester's "open device settings" button that opens the ASIO
  driver's own panel for rate and buffer: asio-sys 0.3.0 does not expose the call, and a change made
  there mid-run is the open thread in `src-tauri/OPEN-THREADS.md` (a panel change while the app runs).
- Looper aesthetic forks: ring/state colour = state vs track identity; Day/Night.
- Undo as visible history (layer count on ↶ UNDO); scenes/snapshots switched on the loop boundary;
  songs as chained scenes; click "01" to name a track; piano hidden by default; synth pills gone once a
  plugin is loaded.
- Stage looks beyond Orbit, Strata and Ember: Horizon (designed, not built: five lanes of road in
  perspective rolling toward a near edge where now is, the input glowing in the sky; a look is one
  module and one line in `src/ui/stage/views.ts`). Orbit, Strata and Ember read each lane's waveform
  bins, its volume and the input meter, so they see neither lane FX nor a fade's level; SCOPE is the
  look that reads the live columns (`lf_engine::scope`), and the same tap would make the three
  loop-drawing looks honest. Band levels, which a spectrum look would want, are not built.
