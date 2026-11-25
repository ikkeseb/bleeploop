import { createSignal } from 'solid-js';
import {
  input,
  mintUnitId,
  platform,
  sendEngine,
  type ChainState,
  type PluginDescriptor,
  type UnitId,
  type UnitParamId,
  type UnitSpec,
  type UnitState,
} from '../../platform';
import { availablePlugins, setActiveSlot } from './instrument';
import { withAt } from './instrument-slots';
import { keepUnitMeters } from '../units/unit-meters';
import { rigChainsReported } from './rig-recall';
// whether a unit is applied in its chain: the record's placement reads it too
import { isApplied } from './rig-record';

/**
 * The chain per slot as the UI knows it (`docs/unit-contract.md` §2): each slot's units in order, as the
 * feed's `chains` last reported them (invariant 3: the row draws from this alone; a gesture sends a call
 * or a command and the feed shows the outcome), and the calls the row makes. Only while
 * `platform.chain.available` (the DEV browser fake today, the native engine after its next step); else
 * the row draws today's fixed topology and nothing here is used.
 */

const EMPTY: ChainState = { units: [], latency: 0, held: false };
const [chains, setChains] = createSignal<[ChainState, ChainState]>([EMPTY, EMPTY]);
const [refusals, setRefusals] = createSignal<[string | null, string | null]>([null, null]);
const [editors, setEditors] = createSignal<ReadonlySet<UnitId>>(new Set());
const [scanned, setScanned] = createSignal<PluginDescriptor[] | null>(null);

/**
 * Whether the row draws the chain: the platform's chain host answers. Fixed for a page.
 * THE GATE: natively the chain host answers only under the DEV flag `lf.chainNative` (`host.tauri.ts`;
 * release refuses every chain call before any IPC), so chain mode is the DEV fake's and that flag's. What
 * it needed is built: the rig record with the v0.8.9 upgrade and its restore (`rig-recall.ts`), the named
 * pedal actions on `Toggle.Units` (native MIDI and `src/app/actions.ts`), the per-unit editor-closed event,
 * the slot's level kept. It turns on for players with what `src/ui/AGENTS.md` § Open threads (chain mode's
 * activation) still lists.
 */
export const chainShown = (): boolean => platform.chain.available;

/** Slot `slot`'s chain as last reported. */
export const slotChain = (slot: 0 | 1): ChainState => chains()[slot];

/** The feed's `chains` (the engine store's frame path, inside its batch); the rig record hears each
 * (`rigChainsReported`: written once the launch's restore finished). */
export function applyChains(next: [ChainState, ChainState]): void {
  setChains(next);
  rigChainsReported(next);
  forgetShownPicks(next);
  const ids = new Set(next.flatMap((c) => c.units.map((u) => u.id)));
  keepUnitMeters(ids);
  // an editor's unit that left the chain, or no longer sounds (parked, withdrawn), has no window any more
  const live = new Set(next.flatMap((c) => c.units.filter((u) => u.state === 'seated' || u.state === 'leaving').map((u) => u.id)));
  if ([...editors()].some((id) => !live.has(id))) setEditors(new Set([...editors()].filter((id) => live.has(id))));
}

// The player closed a plugin's window by its own close (or it went with its unit): the plate's editor
// press reads closed again (unit contract §2, `plugin:editor-closed` by unit).
if (platform.chain.available)
  platform.chain.onEditorClosed((unit) => {
    if (editors().has(unit)) setEditors(new Set([...editors()].filter((id) => id !== unit)));
  });


/** The unit that heads slot `slot`'s chain as applied (unit contract §4: a source there makes it a source
 * chain), or undefined. */
export const appliedHead = (slot: 0 | 1): UnitState | undefined => chains()[slot].units.find(isApplied);

/** Whether slot `slot`'s chain takes its input (no applied source heads it), so it offers its input pick
 * and LIVE: a synth still loading or held for a take does not take LIVE away before it applies. */
export function chainTakesInput(slot: 0 | 1): boolean {
  return appliedHead(slot)?.source !== true;
}

/** The short reason the slot's last chain call was refused, for the tag beside its `+`. */
export const chainRefusal = (slot: 0 | 1): string | null => refusals()[slot];

let refusalTimers: [ReturnType<typeof setTimeout> | undefined, ReturnType<typeof setTimeout> | undefined] = [undefined, undefined];
/** Show `reason` as the slot's tag for a while. */
function tag(slot: 0 | 1, reason: string): void {
  setRefusals((prev) => withAt(prev, slot, short(reason)));
  clearTimeout(refusalTimers[slot]);
  refusalTimers = withAt(refusalTimers, slot, setTimeout(() => setRefusals((prev) => withAt(prev, slot, null)), 8000));
}
function refuse(slot: 0 | 1, e: unknown, what: string): void {
  console.error(`[chain] ${what} refused`, e);
  tag(slot, e instanceof Error ? e.message : String(e));
}
function clearRefusal(slot: 0 | 1): void {
  if (refusals()[slot] !== null) setRefusals((prev) => withAt(prev, slot, null));
}
/** The contract's refusals in the tag's few words; any other reason as the engine gave it. */
function short(reason: string): string {
  if (/holds \d+ units/.test(reason)) return `chain full: ${reason.match(/\d+/)?.[0]} units`;
  if (/ahead of the source/.test(reason)) return 'the source heads the chain';
  if (/only heads its chain/.test(reason)) return 'a source heads its chain';
  return reason;
}

/** At most this many units in a chain (unit contract §1 `MAX_UNITS`). */
export const MAX_CHAIN = 8;

/** Why an add of `spec` is refused for certain, judged as the host judges it (unit contract §1, §4): a
 * ninth unit that cannot resolve as the source replacing the applied one. A plugin's role is known only at
 * activation, so a plugin on a source chain is the host's to resolve. `null`: the host decides. */
export function addRefusal(slot: 0 | 1, spec: UnitSpec): string | null {
  const units = chains()[slot].units;
  if (units.length < MAX_CHAIN) return null;
  const mayReplace = (spec.kind === 'plugin' || spec.kind === 'synth') && appliedHead(slot)?.source === true;
  return mayReplace ? null : `the chain holds ${MAX_CHAIN} units`;
}

/** Why moving `unit` to index `to` of its chain is refused for certain (unit contract §4: a source only
 * heads its chain, an effect never goes ahead of one), or that it is no move at all. `null`: send it.
 * Judged as the host judges a move: against the chain as it will be once the edits already sent apply
 * (a move waits behind them, then is judged again, §7): a unit on its way out is gone by then, a source
 * waiting for its add heads it; what only an unanswered load could settle is the host's. */
export function moveRefusal(unit: UnitId, to: number): string | null {
  const slot = slotOf(unit);
  if (slot === null) return 'the unit is no longer in the chain';
  const units = chains()[slot].units;
  const at = units.findIndex((u) => u.id === unit);
  const u = units[at];
  if (u.state !== 'seated') return `${u.name} is not seated`;
  if (to < 0 || to >= units.length || to === at) return 'no move';
  if (u.source) return to === 0 ? null : 'a source only heads its chain';
  const sourceHeads = units.some(
    (o) => o.id !== unit && o.source && ((isApplied(o) && o.state !== 'leaving') || (o.state === 'waiting' && o.reloading !== true)),
  );
  if (to === 0 && sourceHeads) return 'an effect cannot go ahead of the source';
  return null;
}

const slotOf = (unit: UnitId): 0 | 1 | null =>
  chains()[0].units.some((u) => u.id === unit) ? 0 : chains()[1].units.some((u) => u.id === unit) ? 1 : null;
const unitOf = (unit: UnitId): UnitState | undefined => [...chains()[0].units, ...chains()[1].units].find((u) => u.id === unit);
/** Whether `unit` is seated now: what a gesture on it may reach (the batch drops a command for any other). */
export const unitSeated = (unit: UnitId): boolean => unitOf(unit)?.state === 'seated';

/** Add a unit at the chain's end (a source lands at the head: the host places it). A refusal known here
 * is the slot's tag at once, nothing sent. */
export async function addUnit(slot: 0 | 1, spec: UnitSpec): Promise<void> {
  clearRefusal(slot);
  const why = addRefusal(slot, spec);
  if (why) return tag(slot, why);
  try {
    await platform.chain.add(slot, mintUnitId(), spec, chains()[slot].units.length);
  } catch (e) {
    refuse(slot, e, 'add');
  }
}

/** Remove `unit` (a failed plate included). */
export async function removeUnit(unit: UnitId): Promise<void> {
  const slot = slotOf(unit);
  const u = unitOf(unit);
  if (!u || u.state === 'leaving') return;
  if (slot !== null) clearRefusal(slot);
  try {
    await platform.chain.remove(unit);
  } catch (e) {
    if (slot !== null) refuse(slot, e, 'remove');
    else console.error('[chain] remove refused', e);
  }
}

/** Move `unit` to index `to` of its own chain: one call; the plates move when the feed says so. Judged
 * against the chain as it is now (a menu or a drag may be older than the last report): a refusal known
 * here is the slot's tag, nothing sent; no move sends nothing. */
export async function moveUnit(unit: UnitId, to: number): Promise<void> {
  const slot = slotOf(unit);
  if (slot === null) return;
  clearRefusal(slot);
  const why = moveRefusal(unit, to);
  if (why === 'no move') return;
  if (why) return tag(slot, why);
  try {
    await platform.chain.move(unit, to);
  } catch (e) {
    refuse(slot, e, 'move');
  }
}

/** The plate's lamp: the engine switches the bypass from its own value (unit contract §2 `ToggleUnit`), so
 * two quick presses are two switches and a pedal and a click never undo each other. Only a seated unit's:
 * the batch drops one for any other. */
export function toggleUnit(unit: UnitId): void {
  if (unitSeated(unit)) void sendEngine({ Action: { ToggleUnit: unit } });
}

// ── SYNTH's pick (unit contract §4): an instrument choice on a slot a SYNTH heads is a pick on it ───────

/**
 * The picks sent to each SYNTH that the feed has not shown yet, in the order sent, and `shown`, the pick
 * the feed last showed while they were on their way. The router applies them in that order, so the last
 * one sent is where the unit ends: a choice is skipped only when it equals that, or, with none on its way,
 * the feed's pick (judged against the feed alone, Lead, Bass, Lead sent before the feed says Bass would
 * skip the second Lead). A pick the router did not take is struck out (`pickSynth`).
 */
const picksSent = new Map<UnitId, { shown: number; sent: { instrument: number }[] }>();

/**
 * The feed's pick of each SYNTH with picks on their way acknowledges the picks it shows applied: the
 * earliest place in the order sent that reads it (the pick shown before them counts as the first), with
 * every pick before it. Never later: a frame may show one pick of several alike (Bass, Pad, Bass with the
 * first Bass applied) and must not strike out one still on its way. A frame that skipped picks (two applied
 * between frames, or two the queue coalesced) may leave applied ones listed: they end where the unit is,
 * so no choice is judged wrong by them. A unit that is no SYNTH in the chain any more takes its picks along.
 */
function forgetShownPicks(next: [ChainState, ChainState]): void {
  if (picksSent.size === 0) return;
  const units = new Map(next.flatMap((c) => c.units.map((u) => [u.id, u] as const)));
  for (const [unit, track] of picksSent) {
    const u = units.get(unit);
    if (u?.kind !== 'synth') {
      picksSent.delete(unit);
      continue;
    }
    const shown = u.params?.instrument ?? 0;
    if (shown === track.shown) continue;
    const at = track.sent.findIndex((p) => p.instrument === shown);
    if (at < 0) continue;
    track.sent.splice(0, at + 1);
    track.shown = shown;
    if (track.sent.length === 0) picksSent.delete(unit);
  }
}

/** Picks sent so far, as a running count: a keyboard hold stamps it at its press (`picksSentSoFar`). */
let pickSerial = 0;
/** The count of picks sent so far; a pick's number is the count once it was sent. */
export const picksSentSoFar = (): number => pickSerial;
const pickTakers = new Set<(serial: number) => void>();
/** Call `fn` with a pick's number once the router took it: it released every note held before that pick
 * was sent (`Keyboard.tsx` drops their bookkeeping then, keeping any made after). Returns the unsubscribe. */
export function onPickTaken(fn: (serial: number) => void): () => void {
  pickTakers.add(fn);
  return () => pickTakers.delete(fn);
}

/** The pick of the SYNTH that heads slot `slot`'s chain as applied, as the feed shows it (an index in
 * `SYNTHS`; an add that asked none plays 0), or null when no SYNTH heads it. */
export function headSynthPick(slot: 0 | 1): number | null {
  const head = appliedHead(slot);
  return head?.kind === 'synth' ? (head.params?.instrument ?? 0) : null;
}

/**
 * Built-in instrument `instrument` (an index in `SYNTHS`) chosen for slot `slot`: with a SYNTH seated at
 * the head of its chain, a pick on it through the note router (one bank: what sounds is released and a
 * released note rings out, as a legacy synth switch does), sent only when neither the feed nor a pick
 * still on its way already says it; the slot becomes the MIDI slot either way, as a legacy synth pick
 * makes it (natively the pick has selected it already, so this changes nothing there). False when no
 * SYNTH is seated there: nothing sent, the caller's own path runs.
 */
export function pickSynth(slot: 0 | 1, instrument: number): boolean {
  const head = appliedHead(slot);
  if (head?.kind !== 'synth' || head.state !== 'seated') return false;
  const unit = head.id;
  const shown = head.params?.instrument ?? 0;
  const track = picksSent.get(unit);
  if ((track?.sent.at(-1)?.instrument ?? shown) !== instrument) {
    const pick = { instrument };
    if (track) track.sent.push(pick);
    else picksSent.set(unit, { shown, sent: [pick] });
    const serial = ++pickSerial;
    void input.pickSynth(slot, unit, instrument).then((took) => {
      if (took) {
        for (const fn of pickTakers) fn(serial);
        return;
      }
      // a pick the router did not take (lost, or refused there) is not on its way
      const now = picksSent.get(unit);
      const at = now?.sent.indexOf(pick) ?? -1;
      if (now && at >= 0) now.sent.splice(at, 1);
      if (now?.sent.length === 0) picksSent.delete(unit);
    });
  }
  setActiveSlot(slot);
  return true;
}

/** The `+` menu's built-in instrument: a pick on the SYNTH seated at the slot's head (`pickSynth`), else a
 * SYNTH added with that pick (it lands at the head, replacing a source there). */
export function chooseSynth(slot: 0 | 1, instrument: number): void {
  if (!pickSynth(slot, instrument)) void addUnit(slot, { kind: 'synth', params: { instrument } });
}

/** A built-in unit's parameter, while it is seated; the feed's `chains` brings back the value it took. */
export function setUnitParam(unit: UnitId, param: UnitParamId, value: number): void {
  if (unitSeated(unit)) void sendEngine({ SetUnitParam: [unit, param, value] });
}

/** Whether a plugin unit's editor window is open, as far as the UI asked for it. */
export const editorOpen = (unit: UnitId): boolean => editors().has(unit);

/** Open or close a plugin unit's editor; a refusal (D29 by unit, no editor) is the slot's tag. Opening
 * needs the unit seated; closing goes out whatever its state. */
export async function toggleEditor(unit: UnitId): Promise<void> {
  const slot = slotOf(unit);
  const open = editorOpen(unit);
  if (!open && !unitSeated(unit)) return;
  try {
    if (open) await platform.chain.closeEditor(unit);
    else await platform.chain.openEditor(unit);
    setEditors((prev) => {
      const next = new Set(prev);
      if (open) next.delete(unit);
      else next.add(unit);
      return next;
    });
  } catch (e) {
    if (slot !== null) refuse(slot, e, open ? 'close editor' : 'open editor');
    else console.error('[chain] editor refused', e);
  }
}

/** A plugin unit's parameter through its owner, while it is seated. */
export function setPluginParam(unit: UnitId, paramId: number, value: number): void {
  if (!unitSeated(unit)) return;
  platform.chain.setParameter(unit, paramId, value).catch((err: unknown) => console.error('[chain] plugin parameter refused', err));
}

/**
 * The plugins `+` offers: the scan's (natively the boot scan fills `availablePlugins`); the browser build
 * scans no host of its own, so it asks the platform once (the fake's canned plugins).
 */
export function pickerPlugins(): PluginDescriptor[] {
  const native = availablePlugins();
  if (native.length > 0) return native;
  if (scanned() === null) {
    setScanned([]);
    platform.pluginHost
      .scanPlugins(false)
      .then((list) => setScanned(list))
      .catch((e: unknown) => console.error('[chain] plugin list failed', e));
  }
  return scanned() ?? [];
}

/** A unit's state words: what its plate says under its name, and its accessible name's tail. */
export function unitStatus(u: UnitState, held: boolean): string | null {
  switch (u.state) {
    case 'loading':
      return 'loading…';
    case 'waiting':
      return held ? 'waits for the take' : 'waiting…';
    case 'leaving':
      return u.held ? 'removed when the take ends' : 'removing…';
    case 'failed':
      return `${u.parked ? 'parked' : 'failed'}: ${u.error ?? 'no reason given'}`;
    default:
      if (u.held) return 'moves when the take ends';
      return u.bypass ? 'bypassed' : null;
  }
}
