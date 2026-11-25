# The unit contract: a chain per slot, stereo end to end (a working document)

What the UI, the wire, the engine and the saved files share while the chain and the stereo path are
built, so the UI and engine tracks proceed apart. When the build lands, what still binds folds into
`docs/ARCHITECTURE.md`, the briefings and `STATUS.md`, and this file goes (root `AGENTS.md`).

## The picture

A **slot** (A, B) is a signal line: its input, LIVE, a **chain** of at most 8 **units**, its volume. A
unit is a loaded plugin (CLAP, VST3, VST2) or a built-in: OTT, ECHO, REVERB, RING MOD, or SYNTH (the six
built-in instruments as one unit). A unit is a **source** when its format's host calls it an instrument
(today's rule per format, kept: VST2 counts one synth-flagged or with no inputs,
`src-tauri/src/host/vst2_engine.rs` `rearm`; its scan reads the flag alone, `scan.rs`, so only
activation settles a role) or it is SYNTH; a source only heads a chain. Every other unit
is an **effect**. Every signal from the device input to the lanes and the stems is a stereo pair.

    input pair → LIVE gate → unit → unit → … → slot volume → the wet signal (monitor, record tap)
    notes      →             source → unit → … → slot volume → the master bus (and the record tap)

A source-headed chain takes no input (no LIVE) and goes where an instrument goes; any other chain is
**input-fed** and goes where the wet signal goes: today's `SlotKind` rule (`lf-engine/src/api.rs`) lifted
from the unit to the chain. The lanes' FX, reverb bus, click, limiter and Share output keep their place
(`engine.rs`, header). The slot volume now sits after the chain; today the OTT sits after it (`slots.rs`).

## 1. Unit identity

- **A unit id is an opaque non-zero u32, minted by whoever asks for the unit** (the UI: 32 random bits,
  so a project opened later, build step 8, never lands a tone on another's unit by count; a test: any).
- **The chain host reserves an id across both slots from the moment `add` is called** until its removal
  or a failed add; an add of a reserved id is refused at once, never at seating.
- **The id names the instance and nothing else.** Two instances of one plugin, in one chain or in both,
  are two ids. It holds across a move, a bypass, a restart, an engine rebuild, an app restart and a save;
  it ends with a removal. The native chain host still removes every unit at shutdown, but a plugin unit
  that removal ends keeps its own file (`unit-<id>.tone`, or its pin `unit-<id>.pin`), so the rig record's
  restore at the next launch brings it back under its id with its own tone; a unit already leaving (the
  player's removal, a source replacement) retires as any removal does.
- **The rig record** is `lf.rig` in the WebView's storage (`src/ui/state/rig-recall.ts`; its shape and
  rules, `rig-record.ts`): `{ version: 1, slots: [slot, slot] }`, per slot its `volume` (after the chain), its
  `units` in order as `UnitRecord`s (§2; a plugin unit's display name as `plugin.name`, as §8's rig) and,
  while §6's second step is owed, `pending`. The upgrade writes it whole before the first add (§6). One
  writer holds every later write, each a read-modify-write of the whole record, so a volume change never
  overwrites newer units; it writes only once the launch's restore has the record (read, or upgraded and
  stored): a slot level set before then waits and the record takes it, and a record that cannot be read
  (bad text, a version this build does not know) is left as it is, nothing restored or written that
  launch. The restore starts from the first reset frame's `chains` (none in time: nothing restored or
  written). While a recall runs (`src/app/boot.ts`) nothing writes `units` or `pending` (the
  `volume` may change), so neither a half-restored chain nor `resyncNativeSlots` reaches them; then each
  `chains` report writes `units`: every reported unit but one `leaving`, in order, a failed add's plate
  included, and every record unit no report of the page has named yet, at its record place (a plugin the
  scan did not find, an add on its way), so only a confirmed removal drops one. A failed plate is no proof
  of a plugin fault: the feed names no cause (a lost device or an owner thread that would not start fails a
  plate as the plugin's own load does), so the plate keeps its id and place and each launch adds it again
  until the player removes it. A durable step (the upgrade, a placement) adds nothing until its write reads
  back equal; a failed one logs and waits for the next launch. Its crash marker is `lf.rigInFlight`, the
  chain's own (legacy recall's `lf.rigRecallInFlight` stays legacy mode's), naming the plugin unit ids whose
  adds the restore issued: stored (naming none) before the restore's first add of any kind, and again just
  before each plugin add, naming it too; storage refusing it, no further add and no placement that launch. A
  plugin never issued (missing from the scan, or after an add with no plate, whose later units wait for the
  next launch) is neither named nor waited for, and an add rejected with no plate leaves the marker again, so
  a crash in the other slot's load forgets only the units whose adds reached the host. Removed only by a
  settle that completed, which starts once each issued plugin add's outcome is in the feed (seated or
  failed), or at once when none reached the host (none issued, or each rejected with no plate: an IPC
  failure is no crash). Left over, the last launch died
  restoring only in a new native process (the page's first native MIDI subscribe answers epoch 1, `midi.ts`
  `firstPageOfLaunch`) with none of the record's plugin units in the first reported chains: the record
  forgets the units the marker names only, through the record's one writer (a level set meanwhile stays),
  and the slots' `pending` then place. An empty report alone is no proof of a crash: in a WebView reload (a
  later epoch), or with some plugin units reported, a reload cut a restore short: it goes on, the marker
  kept until a settle completes. Unknown which (no epoch in time): the plugin units are kept and not added
  that launch.
- **A tone is the unit's:** one file per unit id, `unit-<id>.tone` (`src-tauri/src/host/tone.rs`), beside
  one per slot and plugin, `slot<n>-<hash>.tone`. A load reads the unit's own file, else its pin
  `unit-<id>.pin`, else the slot's file, which it then pins (empty: the plugin's defaults), so another
  instance retiring into the slot's file later never changes the tone this unit restores. A save, or a
  redeem of a session import's token (§8), writes the unit's own file and deletes its pin. A removal
  retires it: the unit's own file moves into the slot's file, where the next unit of that plugin starts; a
  unit with only a pin deletes it, the slot's file untouched. An add that ends without ever having applied a
  seat (it failed before it seated, or answers `Err`) deletes what it wrote (its pin, or a redeemed import,
  whose token is parked again); one that applied a seat keeps its files (`src-tauri/OPEN-THREADS.md`).
- **D29 follows the unit.** The editor affinity is per plugin FILE (`instrument.ts` `editorAffinity`);
  each plugin unit has its own owner thread, so a file's editor belongs to the unit loaded first, in either
  slot; a second instance in the SAME chain is refused its editor (its parameters work). `unordered` and
  `orphaned` apply per unit as per slot today; `instrument-controls.mjs` gains each case.
- **Changes:** `MAX_UNITS = 8` beside `SLOT_COUNT` (`api.rs`); `EngineSlot` (`engine_io/plugins.rs`) and
  the holder and eviction tokens (`engine_io/slot_host.rs`) per unit; `tone.rs`; `rig-recall.ts`.

## 2. The wire: commands and the feed, by unit

**Structure is the host's, on its own path.** Add, remove and move allocate, may load a DLL and take
seconds (invariants 4, 5): async host calls, one at a time per slot, never items of the `input_send`
batch. `ChainHost` (`src/platform/host.ts`) replaces `PluginHost`'s slot-addressed calls:

```ts
type UnitId = number; // u32, never 0
type UnitKindId = 'plugin' | 'ott' | 'echo' | 'reverb' | 'ring' | 'synth';
type PluginRef = { format: PluginFormat; path: string; id: string };
type UnitParamId = InputSendParamId | 'instrument' | `gain${'Lead' | 'Bass' | 'Pad' | 'Piano' | 'Organ' | 'Drum'}`;
/** What restores a unit: `add`, the rig record and the v3 rig (§8) share it. */
type UnitSpec = ({ kind: 'plugin'; plugin: PluginRef } | { kind: Exclude<UnitKindId, 'plugin'>;
  params?: Partial<Record<UnitParamId, number>> }) & { bypass?: boolean };
type UnitRecord = UnitSpec & { id: UnitId };
interface ChainHost {
  /** Reserve, build, hand over at `at`; settles on the engine's answer. */
  add(slot: 0 | 1, unit: UnitId, spec: UnitSpec, at: number, toneToken?: number): Promise<'seated' | 'waiting'>;
  remove(unit: UnitId): Promise<void>;
  /** A fresh instance under the same id, place and bypass (§3's replace of itself), if `unit` is still
   *  `plugin` and was not re-added since `since` (a `chains` frame); the load applies `toneToken`. */
  reload(unit: UnitId, plugin: PluginRef, since: number, toneToken: number): Promise<'reloaded' | 'moved' | 'failed'>;
  move(unit: UnitId, to: number): Promise<void>; // `to`: its index in its own chain; no move across slots
  // openEditor, closeEditor, onEditorClosed, listParams, setParameter, takeTone: today's, addressed by unit.
  /** Park a session's tone, identity-checked as today; `matches` by §8's rule. */
  importTone(slot: 0 | 1, bytes: Uint8Array, plugin: PluginRef, unit?: UnitId):
    Promise<{ token: number | null; name: string; matches: UnitId[] }>;
  forgetTone(token: number): Promise<void>;
}
```

A chain unit's window events are `chain:editor-closed`, `chain:param-changed` and `chain:params-changed`,
each carrying `unit` (`engine_io/chain_cmds.rs`); `plugin:*` stays the legacy slots'. `reload` writes the
parked tone to the unit's file before it loads (`ToneStore::redeem`), so the outgoing instance's saves are
dropped from then on; it answers `reloaded` only when its own candidate took the seat.
A new `chain` module in `engine_io` (the chain host) keeps the run's model: per slot the ordered ids,
kinds, plugin identities, each param and bypass **as applied**: it records a built-in's params as the unit
clamped them at build, and the engine answers each `SetUnitParam` with `Event::UnitParam { frame, unit,
param, value }`, the value it took (ECHO's level sent as 2 reads 1), owed as `Seat`. A unit enters an
engine carrying its params and bypass; `engine_io/settings.rs` keeps no per-unit key.

**Seating is acknowledged.** A port push only queues. The engine answers every port message with
`Event::Seat { slot, unit, op, outcome }` (install, remove, withdraw, move; waiting, applied, or refused
with its reason), owed until the ring takes it, as a lane's `Mix`; `add` and `remove` settle on it.
**A `waiting` answer is the first of two:** the held message gets a second `Seat` (applied or refused)
when it resolves, and the slot keeps room owed for it. `add` settles on the first (`seated` or
`waiting`; the second reaches the UI through `chains`); `remove`, `move` and `reload` wait for the
second. The slot's call order (one structural call at a time, `engine_io/chain_cmds.rs`) is released at
`waiting`, so a held op never holds the slot's other calls across a take. `LOAD_TIMEOUT`
(`host/engine_slot.rs`) bounds the plugin's setup and stops at the handover; `REMOVE_TIMEOUT` counts only
while no `waiting` answer came.

**A rebuild keeps every bypass.** The eviction hands back each seat as `(id, token, unit, bypassed, left)`, the
final applied values (the unit holds its params), and the reseat restores exactly those; an owed
`Seat`/`Chain`/`UnitParam` answer only updates the model, never replays. The unit commands the old engine
never took, in its table or ring (peeked as `unapplied_transport` peeks, `engine.rs`), are pushed after
the reseat in their order; an applied toggle is already in `bypassed`, answered or not. `Toggle::Units` is
no kept setting: no setter, its `Toggled` never replayed (`settings.rs` `toggled`).

**What is played live rides the ordered batch** (`src/platform/engine-wire.ts`, `engine_io/wire.rs`,
`verify/fixtures/engine-wire.json`). A command naming an id that is not seated is dropped and counted.

| Command | Does |
|---|---|
| `{ SetUnitBypass: [unit, boolean] }` | the bypass, outright (initialization, a script) |
| `{ Action: { ToggleUnit: unit } }` | the plate's lamp: switches the bypass from the engine's value |
| `{ Action: { Toggle: { Units: kind } } }` | every unit of a built-in effect kind (§6, the old pedals) |
| `{ SetUnitParam: [unit, param, number] }` | a built-in unit's parameter; one of another kind is dropped |

`param` (`UnitParamId`) keeps today's send keys (`INPUT_SEND_PARAMS`) and adds SYNTH's: `instrument`, the
index in the UI's `SYNTHS` order (0 lead, 1 bass, 2 pad, 3 piano, 4 organ, 5 drum; Rust maps it through
`Instrument::from_id`, never its enum's order, where 1 is Pad), sent only by the note router (§4); and a
`gain<Name>` per instrument (0 to `SLOT_GAIN_MAX`, 1 default). Gone: `SetInputSend`, `SetInputSendParam`,
`Toggle.Send`, `SetInstrumentGain`, `NoteTarget.Builtin`. `Toggle::COUNT` stays 11 (`Units` takes
`Send`'s four places). A plugin's own parameters stay off the batch, through its owner.

**The feed** (`src-tauri/src/engine_io/feed.rs`) gains two fields:

```ts
interface FeedFrame {
  chains?: [ChainState, ChainState]; // as applied: on a reset frame and on change; absent = unchanged
  unitMeters?: UnitMeter[];          // one per unit with a range since the last frame; absent = none
}
interface ChainState { units: UnitState[]; latency: Frame; held: boolean }
type UnitState = UnitRecord & { // an interface cannot extend the union
  source: boolean; name: string; latency: Frame; error?: string;
  /** waiting: held until a take ends (§7); failed: no seat, or parked (§3), the plate in its place. */
  state: 'loading' | 'waiting' | 'seated' | 'leaving' | 'failed';
  parked?: boolean; // `failed` by a park (§3): its seat kept, a restart may reseat it; absent: a failed add
  held?: boolean;   // an edit of this unit waits for a take (a removal while `leaving`, a queued move)
  reloading?: boolean; // `loading`/`waiting` by a reload (§3): the old instance still applied; absent: an add
};
interface UnitMeter { unit: UnitId; out: [number, number]; bands?: [BandMeter, BandMeter, BandMeter] }
interface BandMeter { gain: number; gainMin: number; gainMax: number; level: number; levelPeak: number }
```

- The UI draws the chain from `chains` alone (invariant 3): a drag shows a ghost, the plates move when the
  feed says so; a built-in's `params` ride its state, so a reset frame restores every plate. The engine's
  half is `Event::Chain { frame, slot, len, seats }` (id, kind, bypass, latency per seat), owed as `Seat`.
- `out`: the unit's output peak per channel since the last frame. `bands`, the OTT's (low, mid, high):
  `gain` in dB at the frame's last sample, `gainMin`/`gainMax` its range since the last frame, `level` the
  linked follower in dBFS (floor -120), `levelPeak` its largest: what the face draws (`docs/redesign/
  ott.js`). Today's feed has none of it; `Ott::gains()` and `bands()` (`dsp/ott.rs`) feed tests only.
- Transport: the unit folds its range per sample; at most every 4 ms the engine moves each seat's fold
  onto its ring, `EngineHandle::unit_meters` (invariant 2), calling `take_meter` only when the ring has
  room, so a full ring delays a range and never loses one.

## 3. Add, remove, move, bypass, lifecycle, reset

A seat is two crossfades deep, counted in frames so each end lands exactly at any block size (10 ms,
today's `FADE_SECONDS`): lifecycle (undelayed input ↔ what the seat sounds), inside it the seat's bypass
(input delayed by the unit's latency ↔ its output).

- **Add:** built and activated off the callback, handed over with its bypass line, faded in from its
  input; a source fades in from silence.
- **Remove:** a source's notes are released, the seat fades to its undelayed input, `stop` runs once, the
  unit goes back to its owner on the return ring (never dropped, a built-in included); a tail inside it is
  cut. **The slot stays LIVE:** a chain edit does not end the playing; taking the amp-sim out passes the
  dry input through what is left. Today's live-off step (`EngineHost::silence_slot`) goes.
- **Replace:** a source added where one is seated (§4) is one install that `replaces` the old id, sent
  once the new unit is activated: one crossfade. A failed load leaves the old seated, LIVE untouched. A
  reload `replaces` its own id: the same place and bypass.
- **Move:** the chain's output dips to silence over 10 ms, the order changes on the frame the dip ends,
  it comes back over 10 ms. Every unit is called for every frame once, so a tail rings on through its new
  successors. A dip, because one stateful instance cannot run in two orders at once. A move changes no
  latency and no role, so it is never held; a take records the dip.
- **Bypass** keeps the chain's latency (a pedal never moves a take's alignment) and lands on its
  command's frame through `set_bypass` (§7). ECHO, REVERB and RING MOD bypass themselves: the input gate
  closes over 20 ms, the dry passes untouched, the tail rings out over it (`input_fx.rs` `set_on`, as the
  sends do today); the OTT crossfades dry and processed over 10 ms. A plugin and SYNTH are bypassed by the
  seat's crossfade: a plugin keeps being processed (it comes back warm), its tail is cut; a bypassed
  source is silent and still gets its notes, so the held-note bookkeeping stays exact.
- **Lifecycle (invariant 4, unit-scoped):** a restart or an owner's cycle **withdraws** the unit (today's
  `cycle` removes it, which would vacate the seat): the seat fades to its bypass (an effect's input delayed
  by its last latency; a source silent, its notes released), `stop` runs once on the callback, the unit
  goes back on the return ring, and the empty seat keeps its place, role, latency and line; the rest plays
  on. An install under that id reseats it; one with another latency or role is an edit (§7, held during a
  take). One that cannot be reseated (the reinstall fails, or a mid-chain effect came back an instrument)
  **parks**: withdrawn, `failed` with the reason, until a restart reseats it or the player removes it. A
  restart, a park and a failed replacement never change LIVE.
- **Reset** is a unit's audio state (lines, followers, tails, a carrier's phase), never its params: on
  entering an engine (a fresh build, a re-activation after an eviction), never while seated. No gesture
  resets a unit in this build, so the trait has no `reset`.
- **Answers keep order within one engine; a rebuild crosses engines.** A slot answers its port messages
  in message order, so an op settles from its slot's `Seat`s alone (`engine_io/unit_host.rs`). Every
  install carries a token the engine hands back with the unit and why it left (`Left`: removed,
  replaced, withdrawn, evicted, unsent, refused), so the host tells two instances of one id apart (a
  same-id reload evicted mid-crossfade). A rebuild settles what the old engine never answered from that
  engine's own state (its chains, its owed param answers, its unapplied unit commands), never from its
  event ring: a seated unit comes back evicted with its bypass, an install it never applied comes back
  unsent (its op settles as rebuilt), and each owner re-activates at the new engine's terms and submits
  again.
- **Changes:** `SlotPort` (`slots.rs`) carries `Install { id, token, at, bypassed, replaces, unit }`,
  `Remove { id }`, `Withdraw { id }`, `Move { id, to }`, each answered by `Seat`; the rack holds
  `[Seat; MAX_UNITS]` and an order per slot; `src-tauri/AGENTS.md`'s unload order loses its live-off step.

Invariant 4 reads unit-scoped since the owner's word of 2026-11-20 (`docs/ARCHITECTURE.md`).

## 4. The built-in synths: the SYNTH unit, a chain's source

**A built-in synth heads its slot's chain, as a plugin instrument does:** outside it, it would pass no unit
and lose the OTT it has today (`lf-engine/src/instruments.rs`). SYNTH is today's `Instruments` bank as one
unit: all six render, `instrument` picks which takes the notes, a switch allocates nothing, a released note
rings out. Two synth slots are two banks.

- **Notes go to a slot** (`{ Slot: n }` or `Off`): to its source and to every plugin effect in its chain,
  as today's rack hands the targeted slot's unit its notes whatever its kind (`slots.rs` `note_on`); a
  built-in effect ignores them. Held notes stay tracked per slot; a release reaches every seat the note
  reached; a removed unit releases what it holds.
- **A source change releases on the frame it applies.** The rack serves its port at a block start,
  before the block's due commands, and a take can hold a message many blocks (`engine.rs` `process`), so
  a router release is not ordered with it. The engine does it: on the frame a source's install,
  replacement, withdrawal or removal applies, the old source's sounding notes (sustained ones too: their
  `NoteOff` is withheld) are released, the new one starts with none, and a `NoteOff` reaching a seat that
  never got its note is dropped. The router keeps its key (`router.rs` `select_target`, `(slot, target)`)
  and the last pick it sent per slot: a pick (`InputEvent::PickSynth`) is a batch command, so its release
  and `SetUnitParam [unit, 'instrument', n]` go in one ordered batch, and the same pick again sends nothing. SYNTH hands the picked instrument its wheels
  (`Instruments::select`).
- **The wheels become slot events** (`SlotEventKind`): SYNTH applies them, a plugin ignores them (D12).
- **A note sounds one quantum late** (`instruments::LEAD`): SYNTH reports it as its latency, so one formula
  covers every source: a source chain's record path lags by the input side plus the live chains' largest
  latency, less its own sum, never under 0.
- **Placement:** a source goes to index 0 whatever `at` asked, replacing a seated source (§3). The kind is
  known only at activation, so `+` on a nonempty chain resolves there, in the one install; the plate moves
  to the head on the feed's answer. An effect never moves ahead of a source.
- **Changes:** `instruments.rs` behind the unit trait; `SelectInstrument` and the router's target lose
  `Builtin`; `instrument.ts` maps a synth pick to a SYNTH unit; `tests/synth.rs`, `voices.rs`,
  `note_ownership.rs` and the router's tests drive the unit, with a same-slot synth pick and a source
  replaced under a held and a sustained note, each released on its applied frame.

## 5. Built-in effects in series, and what proves each

OTT, ECHO, REVERB and RING MOD become units in a new `units` module of lf-engine, their DSP moved, not
rewritten (`input_fx.rs`, `dsp/ott.rs`). ECHO, REVERB and RING MOD stop being parallel sends: each is an
insert whose output is `input + its wet part`, the wet part as today (levels, 20 ms ramps from the next
quantum boundary, the echo's time from the clock). ECHO and RING MOD run per channel; REVERB feeds the
pair's mean to its convolver and adds its left and right tails to their own channels; the OTT is
`Ott<2>`, detectors linked. Two ECHOs are two lines. A serial chain cannot null against today's sends
once two are on, so the acceptance is per effect, in `tests/units.rs` (not yet built):

1. **Alone, it is today's send.** Before `input_fx.rs` goes, a test pins today's record tap and monitor
   per send (slot 0 live and empty, one deterministic input, 2 s at 48 kHz, the defaults and one other
   setting, master at 1) and the convolver's left and right outputs before they are summed. With equal
   channels, one live slot and chain latency 0, the unit renders the pin: bit for bit per channel for ECHO
   and RING MOD; REVERB each channel against its own side within 1e-6 of full scale (a mean would pass a
   swapped pair). At level 0 the output is the input, bit for bit.
2. **Composition.** For each ordered pair, `[X, Y]` equals `Y` run on `X`'s output alone, bit for bit.
   ECHO and REVERB commute at a steady tempo, so order is proved by E2's clip and gain fakes, and here by
   each pair holding RING MOD or OTT, whose two orders differ by a stated floor.
3. **Bypass.** Settled, the output is the input, bit for bit, the latency unchanged. A bypassed ECHO or
   REVERB still rings out; un-bypassed, its input fades in over 20 ms from the command's frame.
4. **Tails of two units.** Two ECHOs at different times, an impulse, then the first bypassed: its tail
   rings through the second, as item 2 composes it; removing one leaves the other's line untouched. The
   same with one ECHO in each slot.
5. **The OTT** keeps `tests/ott.rs`, run on the unit; its `bands` meter equals the processor's own gains
   and followers sample by sample, the range included.

## 6. Legacy: the pedals and a v0.8.9 profile

v0.8.9 has ECHO, REVERB and RING MOD and no OTT (`8610a20e`, `a02b8f8c` are on `dev` only). Every rule
below reads an old key through the resolver that reads it today, so a missing key means what it meant.

- **A learned pedal keeps working, no relearn, no file change.** The bindings and action ids stay
  (`engine_io/midi/bindings.rs`); `InFxEcho`, `InFxReverb`, `InFxRing`, `InFxOtt` run `Toggle::Units(kind)`
  (`midi/actions.rs`): every unit of that kind in both chains; if any is on, all go bypassed, else all come
  on; answered by one `Toggled` and each changed `Chain`; with none of the kind it does nothing and answers
  no event (`api.rs`). In chain mode native MIDI knows it from the page's subscription
  (`midi_subscribe_chain`), and the UI's own press of an input effect (`src/app/actions.ts`) sends the same
  `Toggle.Units`; legacy mode keeps the sends. `ToggleUnit` serves a pedal bound to one unit; learning one
  is not in this build.
- **The old sends become units in two durable steps,** as a plugin chain's role is known only at
  activation (§The picture: VST2's scan and activation disagree). The upgrade's `lf.rig` write holds
  `pending`: per slot, the send units to place, ids minted, values read (a param with no key reads its
  resolver's default). Once the recall settled and that slot's head (the record's first unit) has a known
  role, one `lf.rig` write adds them to its chain and clears its `pending`; a restart between places them
  again under the same ids, none lost or doubled. No head, a built-in effect or a SYNTH head is known at
  once; a plugin head once a fresh `chains` report shows it applied (seated or parked, its `source`). A
  plugin head missing from the scan, still loading or waiting, or whose add failed with no plate (the host,
  the IPC), keeps the slot's `pending`; so does one whose add left a `failed` plate: the plate names no cause
  (§1), so it is kept, its slot's `pending` with it, and retried each launch until the player removes it (a
  crash at load is still the marker's, §1). Not now: a native cause on the plate (`UnitState.failedBy`) would
  let a plugin's own load failure be forgotten again as legacy recall forgets one, and the slot place as
  input-fed (`src/ui/AGENTS.md` § Open threads). The placement, appended at each input-fed chain's
  end (a reload or the player may have added units since): the OTT, RING MOD → ECHO → REVERB, with their
  saved values; a send that was on goes on into every input-fed chain (it sounded on both slots' sum), one
  that was off goes bypassed, so its pedal reaches either input. One off, at its defaults and bound to no
  pedal is not migrated (boot stores every send's key, `legacy-storage.mjs` header); "bound" reads native
  MIDI's first `bindings` report, and none within `NATIVE_WAIT_MS` counts every send as bound. The OTT
  follows the same rule through its resolvers: v0.8.9 stored no OTT key, so it migrates only when bound; a
  dev profile's on, changed or bound OTT migrates even without its switch key. The upgrade puts only the
  OTT in a SYNTH-headed slot's `pending`, every migrating send in the others'; a source-headed slot (a
  SYNTH, or a plugin that activates as one) gets the OTT alone, if it was on. `lf.inputSend.*` stays as it
  is: in chain mode the legacy sends go off on every reset frame and nothing writes their keys.
- **Rig recall:** `lf.rigRecall` becomes `lf.rig`, written whole before the first load (§1), after a scan of
  the legacy recall's plugin paths: each slot's plugin a plugin unit with a fresh id (dropped when the
  legacy crash marker is stored, as v0.8.9 would have), else its source as `instrument.ts` resolves
  `lf.slotSource.<n>` (unset: Lead in A, Bass in B, Off on a guitar-first install) as a SYNTH unit or an
  empty chain. A DEV chain already reported on the first reset frame with no record yet is adopted as the
  record's units instead (its ids, its order, each slot at the gain the reset frame's settings replay,
  unity where they hold none); the sends still go to `pending`. The old keys stay
  one release.
- **Tones:** a migrated plugin unit's first load starts from its slot and plugin's v0.8.9 file (§1).
- **Slot volume:** a plugin slot takes `savedPluginLevel` (`lf.pluginGain.*`, else 0.9 for a scanned
  effect, 0.1 otherwise or when the scan did not find it, `instrument.ts`); an empty input-fed slot
  `lf.slotInputGain.<n>`; a SYNTH slot unity, its instrument levels from `lf.synthGain.<id>` (the input trim
  never scaled a synth). In chain mode no slot level is sent before the record is known: with no record yet
  the slot's fader shows the level these rules give from the old keys (the scan kind not yet known), and a
  level set on it waits for the record's first write. The restore then applies each slot's volume
  (`SetSlotGain`) and adds nothing until the engine took it and the reset's send-off of the legacy sends;
  either refused (a briefly full command ring) goes again a short backoff apart until taken, with no reset
  frame needed. GO LIVE is never restored.
- **Input picks:** `lf.audioDevices`' per-slot picks (`readAudioDeviceSettings`, the legacy single
  `inputChannel` seeding both, as today) each become a mono pick, `null` auto; stereo is the player's (§7).
- **The autosaved jam:** a v1 or v2 recovery restores into both channels (§8); the next save writes v3.
- **Proved by** `verify/guards/rig-upgrade.mjs` (the upgrade case by case, the placement, the record's
  units from a report), `verify/probes/legacy-storage.mjs`, `legacy-sessions.mjs` (v1/v2 recovery at 44.1
  and 48 kHz) and `rig-recall.mjs` on a stored v0.8.9 profile; at the boundaries those fakes skip, `src-tauri`
  tests that load a v0.8.9 `slot<n>-<hash>.tone` into a new unit (`host/tone.rs`) and dispatch a v0.8.9
  bindings file's `InFx*` into `Toggle::Units` (`actions.rs`). A real profile is rehearsed at the release.

## 7. The stereo unit interface

```rust
pub trait UnitProcessor: Send {
    /// Read once, at install: the kind, whether it is a source, its latency in frames.
    fn info(&self) -> UnitInfo;
    /// Planar stereo both ways, every slice `out[0].len()` long (at most `max_block`). `input` is the
    /// seat before it (a source: silence); `events` the slot's notes and wheels. `out` is overwritten,
    /// every sample finite. Never allocates, locks or waits.
    fn process(&mut self, cx: &UnitCx, input: [&[f32]; 2], events: &[SlotEvent], out: [&mut [f32]; 2]);
    /// A built-in's parameter, on its frame. A plugin's ride its owner's ring: ignored here.
    fn set_param(&mut self, _cx: &UnitCx, _param: UnitParam, _value: f64) -> Option<f64> { None } // Some(applied) clamped, None not a param of this unit
    /// The bypass, on its frame. True: the unit bypasses itself (§3); false: the seat's crossfade does.
    fn set_bypass(&mut self, _cx: &UnitCx, _bypassed: bool) -> bool { false }
    /// Hand over and restart the meter fold (§2). False: it shows nothing.
    fn take_meter(&mut self, _bands: &mut [BandMeter; 3]) -> bool { false }
    fn stop(&mut self);
    fn into_any(self: Box<Self>) -> Box<dyn Any + Send>;
}
/// The call's first device frame, the DSP clock's (less the frames the device skipped), the tempo.
pub struct UnitCx { pub frame: Frame, pub dsp_frame: Frame, pub bpm: f64, pub meter: Meter }
```

It replaces `SlotProcessor` (`api.rs`); `lf_engine::units` builds the built-ins, the three hosts
implement it for plugins (`host/clap_engine.rs`, `vst3_engine.rs`, `vst2_engine.rs`).

- **Always two channels; mono is the same samples twice.** No width flag travels with a signal.
- **A mono plugin:** the adapter feeds a one-channel input the pair's mean and copies a one-channel output
  to both sides; a stereo side takes the pair as it is. `sum_to_mono` (`host/clap.rs` and its twins) goes.
  The host asks each format for stereo first and takes what the plugin grants.
- **A slot's input is one channel or a pair.** A mono pick is that channel on both sides at unity, no pan
  law. The device side always hands the engine a pair per slot (`Engine::process_inputs` takes
  `[[&[f32]; 2]; SLOT_COUNT]`; `callback.rs`, `pipes.rs`). One shape in request, archive and fake: `null`
  (auto: input 2, mono), a channel number, or `[left, right]` (`DeviceRequest.inputChannels`,
  `setSlotInputChannel`, `SlotControls.tsx`, `audio-settings.ts`). A pick the device lacks, either side of
  a pair, opens as auto, flagged (`transition.rs` `open_channels`); the request and the saved pick keep it,
  so a device that has it again reads it, and the slot shows it with the input it reads ("In 4 · In 2"),
  judged from the status alone: its read against the pick it answers (`DeviceStatus.inputRequests`); a
  pick whose switch the status has not answered yet shows plain.
  The feed's `meter` has a peak per channel.
- **Latency sums within a chain,** bypassed units included. A take aligns on the largest sum across live
  input-fed chains (plus the driver's report and the limiter's pre-delay); each live chain's wet reaches
  the record tap delayed by what its own sum lacks of it.
- **The record lines fit every delay.** Each slot's lines and SYNTH's are sized when the engine is built
  for a device, off the callback: the input side (`EngineConfig` gains it; today per block in
  `ProcessContext.input_frames`) plus `MAX_CHAIN_LATENCY` (one second) plus a block. An install past
  `MAX_CHAIN_LATENCY` is refused; a block whose input side exceeds the config's is clamped and counted.
  That settles the clipped-delay thread in the engine briefing.
- **An edit that moves the alignment waits for the take.** Today `Rack::latch_record` latches the
  compensation while a capture runs and the unit changes anyway. Now an install, removal or reseat that
  changes a chain's sum OR its role (input-fed ↔ source, in or out of the live set: a zero-latency source
  included) stays in its port, the slot's later messages behind it, until no capture holds the alignment,
  answered `waiting` (`ChainState.held`); the engine judges it at a block start. Any other edit, a move
  and a bypass apply at once. After any edit that can move audio in time, AUTO REC's listening ignores
  the tap for a fade plus the longest delay any path to it had or has (`Rack::latch_record`): sound
  rendered at the old timing never starts a take.
- **One port for plugins and built-ins;** an eviction hands back every seat and every waiting install.
- **The lanes are stereo:** interleaved `[f32; 2]` frames in the looper's pool (`looper.rs`), one index per
  job, reversal and edge. Eleven 60 s buffers: 126.7 → 253.4 MB at 48 kHz (116.4 → 232.8 at 44.1). Lane FX
  per channel, shared controls; pan becomes a balance. Record tap, lookback, monitor and instruments'
  record path are pairs; a waveform bin and a scope column fold both; the tuner reads the mean.
- **AUTO REC detects on linked energy** (`(l² + r²) / 2` per frame) and keeps its look-back as pairs, so
  the onset it copies into the loop is stereo (`autorec.rs` `scan`); a mean would cancel an attack of
  opposite polarity.
- **The regression bar: a signal with equal channels renders, on each channel, today's mono bits.** The
  suite keeps its expected values with its input fed as a pair. Deliberate breaks, each named in its test's
  header: a stereo plugin's and a built-in synth's recorded sound, the REVERB's tail, and the VST2 fixture's
  mono expectation (`host/vst2_engine_tests.rs`).

## 8. The session and stem format: version 3

`src/session/stem-archive.ts`, `session-schema.ts`, `archive-read.ts`, `autosave.ts`; the engine's bytes in
`engine-wire.ts` (`SnapshotHeader`, `LoadHeader`) and `src-tauri/src/engine_io/session.rs`.

- **`formatVersion: 3`, always** (no 4/4 session is written as v1 any more), so an older build refuses
  it as newer, or, past its archive limit, as too large (the build before v3 checks the size first).
  **Stems are two-channel Float32 WAV, always** (`"channels": 2`; only the master is PCM16), and the
  import refuses a v3 stem that is not; the engine's bytes carry two planar blocks per track and
  `channels` in the header; a track saying 1 (a v1 or v2 stem) is copied to both natively.
- **A v1 or v2 session loads into both channels,** bit for bit; its `plugins` list reads as each slot's one
  plugin unit, under today's import rule (the rig is never swapped).
- **v3 holds the rig,** which build step 8's project files open (whether an import applies it is that
  step's rule). Per slot: its input pick (§7's shape), its volume, its units in order as `UnitRecord`s; a
  plugin unit's `state` names its tone entry, absent when the plugin kept none. GO LIVE is not stored. A
  reader skips a kind or a param it does not know and says so, so a later kind needs no version 4.
  Recovery writes the same archive without the master and may leave `rig` out.
- **Tones on import** (`src/ui/state/slot-tones.ts`) go through `importTone`, parked under a token, in two
  passes: every v3 tone whose unit is seated with its identity takes it first; then each other tone takes
  the first unit of its identity in its slot, in chain order, that no tone took. A match goes through
  `reload` (§2; `add` refuses a seated id). A tone with no match is stored as its slot-and-plugin file
  (§1), so the next unit of that plugin added there starts from it (today's toast says so); every token
  no reload took is forgotten.
- **The import limits fit a full v3 archive:** `maxImportArchiveBytes` budgets five stereo Float32 stems,
  the PCM16 master and `SLOT_COUNT × MAX_UNITS` tones; `parseZip`'s entry cap is `TRACK_COUNT + 2 +
  SLOT_COUNT × MAX_UNITS` (`src/session/archive-limits.ts`, `MAX_UNITS` mirrored, changed together). The
  tones' allowance funds no other read: session.json is bounded before it is parsed, the grid to 60 s,
  and each stem's header must hold its channels, format, rate and frames before a sample is decoded.
  `verify/guards/import.mjs` round-trips the largest legal archive (five 60 s stereo stems at 48 kHz,
  sixteen tones of the largest size).

```json
"rig": { "noteTarget": "A", "slots": [ { "slot": "A", "input": 0, "volume": 0.9, "units": [
  { "id": 2841707, "kind": "plugin", "bypass": false, "state": "<base>-unit-2841707.tone",
    "plugin": { "format": "vst3", "path": "…", "id": "…", "name": "…" } },
  { "id": 771204, "kind": "ott", "bypass": true,
    "params": { "ottDepth": 0.5, "ottTime": 0.5, "ottUpward": 1, "ottDownward": 1 } } ] } ] }
```

## 9. The order of the engine work, and what proves each step

Every test renders offline under `assert_no_alloc`, bit-identical at block sizes 1, 32, 64, 127, 128,
480 and 1024; the fakes are test units that log what they saw.

**E1. A thin stereo slice, end to end.** The trait, a pair per slot input, a chain seating two units
(install at the end, remove, `Seat`), the stereo wet, monitor and record tap, stereo lanes and lane FX,
AUTO REC's stereo onset, the lines' sizing, the snapshot, the load, the wet master (`render.rs`). In
`tests/stereo.rs` (not yet built):

- *Distinct impulses:* left 1.0 at one frame, right 0.5 at another, through two fakes with latencies 64
  and 32 that scale the channels differently: each on its own channel, at its exact frame and gain, and
  nowhere else, in the monitor, the record tap, the lane, its playback, the snapshot's stems, a fresh
  engine loaded from them, the wet master. A one-channel load plays the block on both channels.
- *Unequal latencies on two live inputs:* A sums to 96 frames, B to 17; impulses on one input frame land
  on one loop frame, each on its own channel, each heard at its own latency; a unit that comes or goes
  mid-take moves neither.
- *Capacity:* a chain at exactly `MAX_CHAIN_LATENCY` beside a dry slot and a SYNTH slot, at the largest
  input side, records every start intact; one frame more is refused.
- *AUTO REC:* an attack with `l = -r` triggers, and the loop opens with that pair, bit for bit.
- *The regression bar (§7):* the golden jam, the gesture scripts, `tests/slots.rs`, `tests/share_mix.rs`
  and the Stage 3 references, unchanged.
- *Cost, written down* (`tests/perf.rs`, ignored bars): Stage 2 and 3 loads with stereo lanes and
  per-channel lane FX, a multiply's burst, a TRIM's step.

**E2. N units per slot.** Install at an index, replace, remove, move, bypass, the source rules,
`MAX_UNITS`, held edits, `Event::Chain`, the toggles. In `tests/chain.rs` (not yet built):

- *Order:* a hard clip and a gain of 2; each order equals its composition bit for bit, and they differ.
- *Move:* the dip frame by frame; each unit sees every frame once, in order, its state carried.
- *Bypass:* a plugin seat settles to its input delayed by its latency, still called; a self-bypassing unit
  gets `set_bypass` on the command's frame; a take's impulse lands on the same frame either way.
- *Held edits:* a latent unit added or removed, and a zero-latency source added to an input-fed chain,
  mid-take: answered `waiting`, the take on the grid, the edit on the first block after; a zero-latency
  effect does not wait.
- *Lifecycle:* a ninth unit and an effect ahead of a source are refused; a source at any `at` lands at the
  head, replacing the seated one in one crossfade; a removal releases notes, stops once, returns the unit
  under its id, LIVE unchanged; a withdrawn seat passes its input at its last latency, in place, until
  reseated or parked; an eviction returns every seat with its bypass; the engine never drops a fake.
- *Latency:* 5 + 7 + 11 reads 23; two live chains align on the larger.
- *A proptest* of add, move, remove and bypass scripts at two block sizes: finite, bit-identical, every
  unit returned once, every port message answered once.

**E3. The effects and the synths as units.** The `units` module, §5's acceptance, the meter ring (full
across two feed frames, the next range covers every sample), `Toggle::Units`' any-on rule, SYNTH as a
source (notes, wheels, the record lag, a pick through the router). `input_fx.rs` and the OTT instances
go last, after §5's pin.

The device side follows each step, under `pnpm rust:check`. E1: the pair per slot, and a distinct left and
right signal through each native adapter (the CLAP, VST3 and VST2 fixtures in `src-tauri/src/host/`). E2: a
plugin owner per unit, the chain host and `Seat`, with tests for a cold restore of a bypassed plugin and
two ECHOs at different settings, a rebuild with a kind toggle unanswered and a per-unit change after it
(both kept), a `ToggleUnit` applied but unanswered and one not yet taken (each flips once), ECHO's level
sent as 2 answered 1, and a restart that changes latency mid-take. E3: §6's migration and its native tests.

## 10. The browser fake

The UI is built against `src/platform/host.web.ts` first. The fake models the chain, not its sound:

- **`ChainHost`:** a chain per slot under the contract's rules (a reserved id, a ninth unit, a source's
  placement and replacement refused or resolved as the engine does), each change answered with a `chains`
  frame and its outcome, through `loading` first. Canned plugin descriptors (an effect, an instrument, one
  with latency) feed the picker; editor calls are recorded. `importTone` answers by §8's rule.
- **Bypass and params:** `SetUnitBypass`, `ToggleUnit` and `Toggle.Units` switch the model and answer with
  `chains` (and `Toggled`); `SetUnitParam` lands in `params` clamped as the engine clamps. A probe
  restores a cold rig (a bypassed plugin, two ECHOs at different settings), reads it from a reset frame.
- **The faces' feed:** while an OTT or an ECHO unit is seated and on, `unitMeters` on the fake's own 60 Hz
  timer from a deterministic signal: the OTT's bands from the mockup's simulated part
  (`docs/redesign/ott.js`, through the same curve), following its four params; the ECHO's `out` per
  channel from a plucked note on every bar's downbeat and its repeats through its own three params, at the
  tempo and meter the scripted `Transport` set (120 in 4/4 from a reset). A probe turns the timer off and
  scripts frames through `emit`.
- **Probe seams:** every `ChainHost` call recorded in order; a hold in `loading` or `waiting`; a scripted
  failure or park; a scripted `held`; an injectable id mint.
- **Stereo:** four canned inputs, §7's pick shape per slot, a peak per channel in `meter`, two blocks per
  track in a snapshot and a load, a one-channel load accepted.

## Still to measure

- **Memory at an export and the worst callback** with stereo lanes: measured (`tests/perf.rs`, the
  five-lane 60 s export): about 737 MB of the engine's own buffers held at the render's peak (live pool,
  snapshot, load, render engine), before the WAVs and the zip. The timings there were taken on a busy PC
  (Stage 3 read 50.8 % against its 50 % bar): a quiet rerun settles them.
- **The lane FX per channel** (the pitch shifter above all) and two SYNTH banks: E1's and E3's perf bars;
  if one fails, a mono-linked FX path or one shared bank is the fallback.
- **What each real plugin grants** when asked for stereo (the amp-sim included): read from none.
- **The feed at 60 frames a second under a jam's load,** and **the WASAPI join with four channels**
  (`pipes.rs`; its drift matrix reruns with E1).

## Not heard yet (only the owner's ear settles these, once built)

- The old sends as RING MOD → ECHO → REVERB in series after the plugin: with two on, each feeds the next.
- An old ECHO, REVERB, RING MOD or OTT pedal switching every unit of its kind at once.
- Removing a unit by hand leaves the slot LIVE: taking the amp-sim out passes the dry input through.
