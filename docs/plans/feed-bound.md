# Plan: bound the feed's frames in flight

Open plan, built on the branch `orch/feed-bound`. Once it lands, what still binds folds into
`feed.rs`'s module doc, the Feed row of `src-tauri/AGENTS.md` and `OPEN-THREADS.md`, and this file goes.

## The defect

`turn` (`src-tauri/src/engine_io/feed.rs`) sends a frame every tick that has news, and
`Channel::send` (`engine_io/mode.rs`) succeeds once Tauri has queued it: nothing says whether the
page read it. A WebView whose main thread stalls (a long task, a debugger, a page slower than 60
frames/s) collects every frame sent meanwhile, with no bound, and then applies all of them in a burst;
a page slower than the feed never catches up. Tauri 2.11.5 delivers a frame under its direct-execute
threshold by `eval`, a larger one through a payload queue the page fetches from
(the tauri crate's ipc/channel.rs, ~160–180), and the page's `Channel` hands messages on strictly by index.

## Design

1. **The page acks.** `src/platform/host.tauri.ts` (the subscribe) acks the newest `seq` it has
   handed to the store with `engine_feed_ack(seq)`: 100 ms after a frame arrives (one timer at a
   time), and as a heartbeat every second while subscribed, a failed call retried by the next one. It
   reads `seq` from the raw frame, so a frame the decoder rejects is acked too. Acks are cumulative.
   The command is `async` (never the UI thread) and takes only the number. `engine-store.ts` does
   not change.
2. **The feed counts.** Per subscriber: its frames in flight (seq and send instant, at most
   `MAX_IN_FLIGHT`) and its newest ack. `seq` already rises across subscribers (frames built), so an
   ack below the subscriber's first frame (an older document's) or above its last is ignored, and no
   subscription id is needed.
3. **The bound.** A turn that finds `MAX_IN_FLIGHT` = 30 frames (about 0.5 s at 60/s) unacked withholds
   the subscriber: the feed ticks as a held one does (the ring drained into the mirror, device events
   kept, scope columns dropped) and sends nothing until every frame sent is acked.
4. **A catch-up ends it, not a reset.** The end of a withhold, and the end of a hold (the folder
   dialog: the same "the page missed frames"), is one ordinary frame (`reset: false`; no wire change).
   Its events: every event drained while withheld, in drain order, but beats (`WITHHELD_EVENTS` = 256;
   `Lane`, `Mix` and `Transport` are published only when they change, `lf-engine` `looper.rs`
   `publish`, so the stream grows with the player's actions, not with time); then the count's latest
   beat and downbeat from the mirror while a lane is armed (not one this tick drained); then this
   tick's events. The page applies the stream as it would have live: a layer that committed bumps its
   lane's revision, a `Cleared` meets the state it ended (`holdsLoop`, the clear token), a `Mix` acks
   an overlay where it would have. Peaks: the bins changed since the last frame sent (held ticks leave
   the dirty bits alone); status, live and meter: against what was last sent; scope: a batch with
   `gap`.
5. **A reset instead** when the subscriber is new, when another engine came during the withhold or
   hold (the held tick latches the generation change `tick_held` ignores today), or when the withheld
   stream passed 256. The overflow's reset still carries the withheld `TakeRejected` and `PassDropped`
   (kept aside, at most 16), so a rejected take is never silent; it cancels a gesture's overlay and
   voids an unspent clear token (the recovery is kept, the safe side), as a hold's reset does today.
6. **A stuck channel resubscribes.** A frame lost before the page's handler (a failed payload fetch,
   which Tauri only logs) parks every later one, today for good. `engine_feed_ack` answers `true` when
   the oldest frame the page has not acked was sent more than `STUCK` = 5 s ago and the ack still does
   not cover it; the page then subscribes a new channel and gets a fresh subscriber's reset. A stalled
   page sends no heartbeat, so a stall never trips it; a slow page's acks advance.

## What survives on the page

Without an overflow nothing resets there: overlays, revisions, `resets` (so the clear token and the
GO LIVE disarm of a page's first reset, `engineResync`) behave as if the frames had come live, and
`adoptSettings` does not run, so the page sends no command.

## What the player hears and sees

- Hears: nothing changes. The feed is display only, and a catch-up sends no command (a hold's reset
  today re-sends the note target and the gains, which changes nothing either).
- Sees: after a stall longer than about half a second, at most 30 stale frames, then one step to now;
  the scope trace restarts, the waveforms are not redrawn from scratch. A take rejected, a refusal or
  a pedal's CLEAR while the page stalled or the folder dialog was open shows once the page is back
  (a hold drops them today). A gesture held through a stall keeps its value.
- A page slower than the feed lags at most about 30 frames, then catches up, where today it falls
  behind without limit. A hidden page whose timers run once a second gets about 30 frames and a
  catch-up a second. A lost frame costs 5 s of a frozen view and a reset, where today the view stays
  frozen.

## Settled counter-cases (gpt-6-astra @ high, 2026-10-28)

- A committed overdub with the next one already running, and a take committed then cleared, both
  inside one withhold: a catch-up of the one-shots before the final state lost the lane transitions
  the page's reducers read (no revision bump: autosave skipped the layer; no `holdsLoop`: no clear
  token). Settled by item 4: the withheld stream in order, not a snapshot.
- An overflow's reset loses the take notices and the clear token: the notices are kept aside (item 5);
  the token is voided only past 256 engine events in one stall, which keeps the recovery.
- A lost payload fetch blocks the page's channel, so it never acks and the feed stays withheld:
  item 6. The same hole freezes the view for good today.
- The bound is per subscription: Tauri drops a large payload only when a page fetches it, so a
  document that goes away before fetching leaves up to 30 frames' payloads behind per reload (today:
  all of them). Left, a Tauri internal.

## Residuals

- Device events past 64 still drop their oldest while withheld, as while held.

## Tests owed (native, `feed.rs`)

- Unacked frames reach the bound: nothing is sent, and the ring is still drained into the mirror.
- Acks back to zero: exactly one catch-up, not a reset, carrying the withheld events in drain order
  without beats (a lane RECORDING → PLAYING → `Cleared` → EMPTY, and OVERDUBBING → PLAYING →
  OVERDUBBING, both kept whole), the count once, and only the changed peaks.
- An ack from before the subscriber's first frame, or past its last, changes nothing.
- Another engine during a withhold: a reset. Past 256 withheld events: a reset that carries the
  withheld `TakeRejected`.
- `STUCK`: an ack that does not cover a frame sent more than 5 s earlier answers `true`; one that
  covers it, or a younger frame, `false`.
- A hold ends with a catch-up (the hold tests' reset assertions change); a held or withheld thread
  still stops.
