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

1. **The page acks.** `src/platform/host.tauri.ts` (the subscribe) acks the newest `seq` its current
   channel has handed to the store with `engine_feed_ack(seq)`: 100 ms after a frame arrives (one
   timer at a time), and as a heartbeat every second while subscribed, a failed call retried by the
   next one. It reads `seq` from the raw frame, so a frame the decoder rejects is acked too. Acks are
   cumulative. A channel that has handed on no frame yet acks `null`, so its heartbeat still runs
   (item 6). The command is `async` (never the UI thread) and takes only that number or `null`.
2. **The feed counts.** Per subscriber: its frames in flight (seq, send instant and the frame's
   notices, at most `MAX_IN_FLIGHT`) and its newest ack. `seq` already rises across subscribers
   (frames built), so an ack below the subscriber's first frame (an older channel's) or `null` covers
   nothing of it, one above its last is ignored, and no subscription id is needed.
3. **The bound.** A turn that finds `MAX_IN_FLIGHT` = 30 frames (about 0.5 s at 60/s) unacked withholds
   the subscriber: the feed ticks as a held one does (the ring drained into the mirror, device events
   kept, scope columns dropped) and sends nothing until every frame sent is acked. A hold and a
   withhold can overlap: the feed sends again once neither applies.
4. **A catch-up ends it, not a reset.** The end of a withhold, and the end of a hold (the folder
   dialog: the same "the page missed frames"), is one frame with `reset: false` and `catchUp: true`
   (the one wire addition; absent on every other frame). Its events: every event drained while
   withheld or held, in drain order, but beats (`WITHHELD_EVENTS` = 256; `Lane`, `Mix` and
   `Transport` are published only when they change, `lf-engine` `looper.rs` `publish`, so the stream
   grows with the player's actions and with RETAKE's passes, not with time); then the count's latest
   beat and downbeat from the mirror while a lane is armed (not one this tick drained); then this
   tick's events. The page applies the stream as it would have live: a layer that committed bumps its
   lane's revision, a `Cleared` meets the state it ended (`holdsLoop`, the clear token), a `Mix` acks
   an overlay where it would have. Peaks: the bins changed since the last frame sent (held ticks leave
   the dirty bits alone); status, live and meter: against what was last sent; scope: a batch with
   `gap`. On a catch-up the page (`engine-store.ts`) also cancels the beat timers it scheduled before
   (a count numeral due before the stall would otherwise overwrite the catch-up's), and a lane's peak
   update bumps its revision even while it captures: the edit before a new overdub (UNDO, TRIM,
   REVERSE) reaches its peaks only after the overdub's own `Lane`, and autosave must still see it.
5. **A reset instead** when the subscriber is new, when another engine came during the withhold or
   hold (the held tick latches the generation change `tick_held` ignores today), or when the withheld
   stream passed 256. Every such reset that ends a withhold or a hold carries the withheld notices
   (`TakeRejected`, `PassDropped`, `Refused`, kept aside, at most `KEPT_NOTICES` = 32, the newest
   kept), whatever its cause, so the notices survive another engine too. It cancels a gesture's
   overlay and voids an unspent clear token (the recovery is kept, the safe side), as a hold's reset
   does today. A meter pick writes the kept meter at the pick (`engine-store.ts` `setMeter`), so a
   reset whose `Transport` the page never saw does not send the old one back (`adoptSettings`).
6. **A stuck channel resubscribes.** A frame lost before the page's handler (a failed payload fetch,
   which Tauri only logs) parks every later one, today for good. The watchdog judges progress, not a
   frame's age: an ack that advances clears it; one that does not (a lower or equal seq, `null`)
   while frames are in flight starts it, and `engine_feed_ack` answers `true` once acks have not
   advanced for `STUCK` = 5 s. A page back from a long stall, whose first heartbeat may run before
   its queued frames, advances on its next ack; a slow page's acks advance; a stalled page sends
   none. On `true` the page retires its channel first (its handler and its pending ack answers are
   fenced by a channel generation, so a late frame or a late `true` of the old one does nothing),
   then subscribes a new one, one resubscribe at a time, and gets a fresh subscriber's reset. A new
   subscribe (this one or a reload's) takes over the notices of the frames the old subscriber had in
   flight unacked, so a notice parked behind a lost frame comes with the reset.

## What survives on the page

Without an overflow or another engine nothing resets there: overlays, revisions, `resets` (so the
clear token and the GO LIVE disarm of a page's first reset, `engineResync`) behave as if the frames
had come live, and `adoptSettings` does not run, so the page sends no command.

## What the player hears and sees

- Hears: nothing changes. The feed is display only, and a catch-up sends no command (a hold's reset
  today re-sends the note target and the gains, which changes nothing either).
- Sees: after a stall longer than about half a second, at most 30 stale frames, then one step to now;
  the scope trace restarts, the waveforms are not redrawn from scratch. A take rejected, a refusal or
  a pedal's CLEAR while the page stalled or the folder dialog was open shows once the page is back
  (a hold drops them today), through the same toasts as live, which show at most four at once
  (`src/notify.ts`). A gesture held through a stall keeps its value.
- A page slower than the feed lags at most about 30 frames, then catches up, where today it falls
  behind without limit. A hidden page whose timers run once a second gets about 30 frames and a
  catch-up a second. A lost frame costs about 5 s of a frozen view and a reset, where today the view
  stays frozen.

## Settled counter-cases

First read (gpt-6-astra @ high, 2026-10-28):

- A committed overdub with the next one already running, and a take committed then cleared, both
  inside one withhold: a catch-up of the one-shots before the final state lost the lane transitions
  the page's reducers read. Settled by item 4: the withheld stream in order, not a snapshot.
- An overflow's reset loses the take notices and the clear token: the notices are kept aside (item 5);
  the token is voided only past 256 engine events in one stall, which keeps the recovery.
- A lost payload fetch blocks the page's channel, so it never acks and the feed stays withheld:
  item 6.
- The bound is per subscription: Tauri drops a large payload only when a page fetches it, so a
  document that goes away before fetching leaves up to 30 frames' payloads behind per reload (today:
  all of them). Left, a Tauri internal.

Second read, of items 4–6 (gpt-6-astra @ high, 2026-10-28):

- A stuck rule on a frame's age fires on a page back from a long stall, whose heartbeat can run
  before its queued frames; and a lost first frame left the page nothing valid to ack, so the rule
  never ran. Settled by item 6's progress watchdog and item 1's `null` ack.
- The old channel's late frames reached the store after the new reset; settled by item 6's fence.
- Notices parked behind a lost frame, withheld before another engine came, or a `Refused` in an
  overflow were lost; settled by item 5 (every reset, `Refused` included) and item 6 (the takeover).
- An edit (UNDO) then a new overdub inside one withhold gave autosave no revision until the overdub
  ended; settled by item 4's catch-up peak rule. A beat timer due before a hold overwrote the
  catch-up's count; settled by item 4's timer cancel.
- An overflow's reset re-sent the page's saved meter over a 4/4 pick whose `Transport` the page never
  saw (the host keeps no `SetMeter` for 4/4); settled by item 5's write at the pick.

## Residuals

- Device events past 64 still drop their oldest while withheld, as while held. Notices past 32 in one
  stall drop their oldest; the toasts show at most four at once.
- Late delivery is still late, as in today's burst after a stall: a replayed take start reads the
  page's FIXED bars as they are at the catch-up; a `Mix` of before the stall can ack or a `Cleared`
  cancel a gesture the page made while withheld (the overlay acks by value); a device stopped during
  a stall freezes the playhead where the catch-up finds it.

## Tests owed (native, `feed.rs`)

- Unacked frames reach the bound: nothing is sent, and the ring is still drained into the mirror.
- Acks back to zero: exactly one catch-up (`catchUp`, not a reset), carrying the withheld events in
  drain order without beats (a lane RECORDING → PLAYING → `Cleared` → EMPTY, and OVERDUBBING →
  PLAYING → OVERDUBBING, both kept whole), the count once, and only the changed peaks.
- An ack from before the subscriber's first frame, `null`, or past its last, moves nothing.
- Another engine during a withhold: a reset that carries the withheld `TakeRejected`. Past 256
  withheld events: a reset that carries the withheld `TakeRejected` and `Refused`.
- The watchdog: acks that do not advance for 5 s while frames are in flight answer `true`; an ack
  that advances clears it, and a first non-advancing ack after a long stall answers `false`.
- A new subscribe carries the old subscriber's unacked notices in its reset.
- A hold ends with a catch-up (the hold tests' reset assertions change); a held or withheld thread
  still stops.
