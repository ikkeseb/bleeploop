import { createSignal } from 'solid-js';
import {
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
import { availablePlugins } from './instrument';
import { withAt } from './instrument-slots';
import { keepUnitMeters } from '../units/unit-meters';

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
 * THE GATE: the native host refuses every chain call today (`host.tauri.ts`), so chain mode is the DEV
 * browser fake's alone. It turns on natively only with the prerequisites in `src/ui/AGENTS.md` § Open
 * threads (the chain mode's activation): the legacy migration into the rig record, the named pedal
 * actions on `Toggle.Units`, the per-unit editor-closed event, the slot's level kept.
 */
export const chainShown = (): boolean => platform.chain.available;

/** Slot `slot`'s chain as last reported. */
export const slotChain = (slot: 0 | 1): ChainState => chains()[slot];

/** The feed's `chains` (the engine store's frame path, inside its batch). */
export function applyChains(next: [ChainState, ChainState]): void {
  setChains(next);
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

/** Whether `u` is applied in its chain: what sets the chain's role (seated, on its way out, parked in its
 * seat, or reloading: its old instance plays on until the new one lands, unit contract §3). A unit still
 * loading or waiting for its add is not, yet. */
export const isApplied = (u: UnitState): boolean =>
  u.state === 'seated' || u.state === 'leaving' || u.reloading === true || (u.state === 'failed' && u.parked === true);

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
