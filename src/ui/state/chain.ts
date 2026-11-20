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

/** Whether the row draws the chain: the platform's chain host answers. Fixed for a page. */
export const chainShown = (): boolean => platform.chain.available;

/** Slot `slot`'s chain as last reported. */
export const slotChain = (slot: 0 | 1): ChainState => chains()[slot];

/** The feed's `chains` (the engine store's frame path, inside its batch). */
export function applyChains(next: [ChainState, ChainState]): void {
  setChains(next);
  const ids = new Set(next.flatMap((c) => c.units.map((u) => u.id)));
  keepUnitMeters(ids);
  // an editor's unit that left the chain has no window any more
  if ([...editors()].some((id) => !ids.has(id))) setEditors(new Set([...editors()].filter((id) => ids.has(id))));
}

/** Whether slot `slot`'s chain takes its input (no source heads it), so it offers its input pick and LIVE. */
export function chainTakesInput(slot: 0 | 1): boolean {
  return chains()[slot].units[0]?.source !== true;
}

/** The short reason the slot's last chain call was refused, for the tag beside its `+`. */
export const chainRefusal = (slot: 0 | 1): string | null => refusals()[slot];

let refusalTimers: [ReturnType<typeof setTimeout> | undefined, ReturnType<typeof setTimeout> | undefined] = [undefined, undefined];
function refuse(slot: 0 | 1, e: unknown, what: string): void {
  console.error(`[chain] ${what} refused`, e);
  const reason = e instanceof Error ? e.message : String(e);
  setRefusals((prev) => withAt(prev, slot, short(reason)));
  clearTimeout(refusalTimers[slot]);
  refusalTimers = withAt(refusalTimers, slot, setTimeout(() => setRefusals((prev) => withAt(prev, slot, null)), 8000));
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

const slotOf = (unit: UnitId): 0 | 1 | null =>
  chains()[0].units.some((u) => u.id === unit) ? 0 : chains()[1].units.some((u) => u.id === unit) ? 1 : null;

/** Add a unit at the chain's end (a source lands at the head: the host places it). */
export async function addUnit(slot: 0 | 1, spec: UnitSpec): Promise<void> {
  clearRefusal(slot);
  try {
    await platform.chain.add(slot, mintUnitId(), spec, chains()[slot].units.length);
  } catch (e) {
    refuse(slot, e, 'add');
  }
}

/** Remove `unit` (a failed plate included). */
export async function removeUnit(unit: UnitId): Promise<void> {
  const slot = slotOf(unit);
  if (slot !== null) clearRefusal(slot);
  try {
    await platform.chain.remove(unit);
  } catch (e) {
    if (slot !== null) refuse(slot, e, 'remove');
    else console.error('[chain] remove refused', e);
  }
}

/** Move `unit` to index `to` of its own chain: one call; the plates move when the feed says so. */
export async function moveUnit(unit: UnitId, to: number): Promise<void> {
  const slot = slotOf(unit);
  if (slot === null) return;
  clearRefusal(slot);
  try {
    await platform.chain.move(unit, to);
  } catch (e) {
    refuse(slot, e, 'move');
  }
}

/** The plate's lamp: the engine switches the bypass from its own value (unit contract §2 `ToggleUnit`), so
 * two quick presses are two switches and a pedal and a click never undo each other. */
export function toggleUnit(unit: UnitId): void {
  void sendEngine({ Action: { ToggleUnit: unit } });
}

/** A built-in unit's parameter; the feed's `chains` brings back the value the unit took. */
export function setUnitParam(unit: UnitId, param: UnitParamId, value: number): void {
  void sendEngine({ SetUnitParam: [unit, param, value] });
}

/** Whether a plugin unit's editor window is open, as far as the UI asked for it. */
export const editorOpen = (unit: UnitId): boolean => editors().has(unit);

/** Open or close a plugin unit's editor; a refusal (D29 by unit, no editor) is the slot's tag. */
export async function toggleEditor(unit: UnitId): Promise<void> {
  const slot = slotOf(unit);
  const open = editorOpen(unit);
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
      return 'removing…';
    case 'failed':
      return `failed: ${u.error ?? 'no reason given'}`;
    default:
      return u.bypass ? 'bypassed' : null;
  }
}
