import { For, Show, createEffect, createResource, createSignal, on, onCleanup, onMount } from 'solid-js';
import { platform, type PluginParamDesc, type UnitParamId, type UnitState } from '../../platform';
import {
  addUnit,
  appliedHead,
  chainRefusal,
  chooseSynth,
  editorOpen,
  moveRefusal,
  moveUnit,
  pickerPlugins,
  removeUnit,
  setPluginParam,
  setUnitParam,
  slotChain,
  toggleEditor,
  toggleUnit,
  unitStatus,
} from '../state/chain';
import { engineInputSends } from '../state/engine-store';
import { SYNTHS } from '../state/instruments';
import { pluginPickerLabel } from '../state/plugin-descriptor';
import { slotLetter } from '../state/slot-tones';
import { formatVal } from '../looper/FxPanel';
import type { FxParamDef } from '../state/fx-metadata';
import { Icon } from '../instrument/icons';
import { PlateMenu, type PlateMenuItem } from '../instrument/PlateMenu';
import { RowPopoverPanel, createRowPopover } from '../instrument/RowPopover';
import { chainCompact, chainScroll } from '../instrument/row-fit';
import { faceOpen, registerPlate, toggleFace, type FaceKind } from './face-store';
import { registerOttMini } from './ott-loop';
import '../looper/fxpanel.css';
import '../transport/input-fx.css';
import './units.css';

/**
 * A slot's chain on its signal line (`InstrumentSlot.tsx` draws it in place of the fixed topology while
 * `platform.chain.available`): one plate per unit in chain order, then the `+` bay. Every plate has the
 * row's one anatomy (`slot.css` `.plate`): its lamp (the bypass, `ToggleUnit`), its name, a small mark
 * (a plugin's format and its editor glyph, a built-in's miniature) and the menu chevron (Editor,
 * Parameters, Move left, Move right, Remove). A unit not seated says so under its name, never in a new
 * colour. The plates are drawn from the feed's `chains` alone: a drag shows the plate following the
 * pointer and a drop marker, and the order changes when the feed says so (one `move` per drop).
 */
export function ChainLine(props: { slot: 0 | 1; live: boolean; returnFocus?: (el: HTMLElement | undefined) => void }) {
  const chain = () => slotChain(props.slot);
  const letter = slotLetter(props.slot);
  let line!: HTMLDivElement;
  let wrap!: HTMLDivElement;
  const [drag, setDrag] = createSignal<Drag | null>(null);
  const head = () => appliedHead(props.slot);
  const focusBack = (el: HTMLElement | undefined) => (props.returnFocus ?? ((x) => x?.focus()))(el);

  // ── focus: a report that moves or removes the focused plate, or a fold that unmounts the line, hands
  // focus to a surviving control (the same control on the moved plate, else its neighbour's menu, else `+`)
  let lastFocus: { unit: number; cls: string; index: number } | null = null;
  // whether the focus is the line's: in a plate, in a plate's portal (its menu, its Parameters popover:
  // Solid's delegation brings their focus events through the plate), or on the `+`
  let owned = false;
  let addButton: HTMLButtonElement | undefined;
  const onFocusIn = (e: FocusEvent) => {
    owned = true;
    const t = e.target as HTMLElement;
    const p = t.closest<HTMLElement>('.unit');
    if (!p) {
      lastFocus = null;
      return;
    }
    const cls = ['plate__lamp', 'unit__name', 'plate__ed', 'plate__menu'].find((c) => t.classList.contains(c)) ?? 'plate__menu';
    lastFocus = { unit: Number(p.dataset.unit), cls, index: chain().units.findIndex((u) => u.id === Number(p.dataset.unit)) };
  };
  // Chromium blurs a focused node a report moves or removes (focusout, no related target): that loss, just
  // now, is what is restored; focus taken elsewhere is never pulled back
  let lostAt = -Infinity;
  const onFocusOut = (e: FocusEvent) => {
    const to = e.relatedTarget as Node | null;
    if (!to) {
      lostAt = performance.now();
      return;
    }
    // focus going elsewhere: the focusin that follows says whether it is still the line's
    owned = false;
    if (!wrap.contains(to)) lastFocus = null;
  };
  const focusLost = () => !document.activeElement || document.activeElement === document.body;
  const restoreFocus = () => {
    const f = lastFocus;
    if (!f || !focusLost() || !wrap.isConnected || performance.now() - lostAt > 1000) return;
    const same = line.querySelector<HTMLElement>(`.unit[data-unit="${f.unit}"] .${f.cls}:not(:disabled)`);
    const plates = [...line.querySelectorAll<HTMLElement>(':scope > .unit')];
    const near = plates[Math.min(f.index, plates.length - 1)]?.querySelector<HTMLElement>('.plate__menu');
    const to = same ?? line.querySelector<HTMLElement>(`.unit[data-unit="${f.unit}"] .plate__menu`) ?? near ?? wrap.parentElement?.querySelector<HTMLElement>('.unit-add');
    if (to) focusBack(to);
  };
  createEffect(
    on(
      () => chain().units.map((u) => `${u.id}:${u.state}`).join(),
      () => queueMicrotask(restoreFocus),
      { defer: true },
    ),
  );
  onCleanup(() => {
    // the slot folded under the focus (a portal of a plate is gone already, its focus just lost): its fold
    // plate takes it
    const a = document.activeElement;
    const held = a !== null && (wrap.contains(a) || a === addButton);
    if (owned && (held || (focusLost() && performance.now() - lostAt < 1000))) {
      const slot = wrap.closest('.slot');
      queueMicrotask(() => {
        const fold = slot?.querySelector<HTMLElement>('.slot__fold');
        if (fold && focusLost()) focusBack(fold);
      });
    }
  });

  // ── what changed, said: a plate's label changing is not announced, so its transitions are ──────────
  const [said, setSaid] = createSignal('');
  // what the line found when it mounted (a slot unfolded) is no change
  let before = new Map(chain().units.map((u) => [u.id, u]));
  createEffect(
    on(
      () => chain().units,
      (units) => {
        const words: string[] = [];
        const now = new Map(units.map((u) => [u.id, u]));
        for (const u of units) {
          const w = transition(before.get(u.id), u, chain().held);
          if (w) words.push(w);
        }
        for (const [id, u] of before) if (!now.has(id)) words.push(`${plateName(u)} removed`);
        before = now;
        if (words.length === 0) return;
        // a repeat of the same words is said again
        setSaid('');
        queueMicrotask(() => setSaid(`Slot ${letter}: ${words.join('. ')}`));
      },
      { defer: true },
    ),
  );

  return (
    <>
      <div class="unit-chain" classList={{ 'unit-chain--scroll': chainScroll() }} ref={wrap} onFocusIn={onFocusIn} onFocusOut={onFocusOut}>
        <i class="unit-more unit-more--l" aria-hidden="true">
          <Icon name="left" size={14} />
        </i>
        <div
          class="unit-line"
          classList={{ 'unit-line--compact': chainCompact(), 'unit-line--scroll': chainScroll(), 'unit-line--drag': drag() !== null }}
          role="list"
          aria-label={`Slot ${letter}'s chain`}
          ref={line}
        >
          {/* keyed by id: a report's new objects never remount a plate (its open face, its drag) */}
          <For each={chain().units.map((x) => x.id)}>
            {(id, i) => {
              let last = chain().units[i()];
              const u = (): UnitState => (last = chain().units.find((x) => x.id === id) ?? last);
              return (
                <>
                  <Show when={i() > 0}>
                    <i class="slot__sig" classList={{ 'is-dim': !sounding(u(), props.live, head()) }} aria-hidden="true" />
                  </Show>
                  <UnitPlate
                    slot={props.slot}
                    unit={u}
                    index={i}
                    count={() => chain().units.length}
                    held={() => chain().held}
                    live={props.live}
                    line={() => line}
                    drag={drag}
                    setDrag={setDrag}
                    returnFocus={props.returnFocus}
                  />
                </>
              );
            }}
          </For>
          <Show when={drag()?.gap != null}>
            <i class="unit-drop" aria-hidden="true" style={{ left: `${dropX(line, drag()!.gap!)}px` }} />
          </Show>
        </div>
        <i class="unit-more unit-more--r" aria-hidden="true">
          <Icon name="right" size={14} />
        </i>
      </div>
      <i class="slot__sig" classList={{ 'is-dim': chain().units.length > 0 && !props.live && head()?.source !== true }} aria-hidden="true" />
      <AddBay slot={props.slot} returnFocus={props.returnFocus} button={(el) => (addButton = el)} onFocusIn={onFocusIn} onFocusOut={onFocusOut} />
      <Show when={chainRefusal(props.slot)}>
        <span class="pc__err unit-err" role="status" data-slot={props.slot} title={chainRefusal(props.slot) ?? ''}>
          {chainRefusal(props.slot)}
        </span>
      </Show>
      <span class="lf-visually-hidden unit-said" role="status" aria-live="polite" data-slot={props.slot}>
        {said()}
      </span>
    </>
  );
}

/** A plate's change as a few words for the live region, or null when nothing worth saying changed. */
function transition(prev: UnitState | undefined, u: UnitState, held: boolean): string | null {
  const name = plateName(u);
  const status = unitStatus(u, held);
  if (!prev) return u.state === 'seated' ? `${name} added` : `${name} added, ${status}`;
  if (prev.state === u.state && prev.held === u.held && prev.parked === u.parked && prev.error === u.error) return null;
  if (u.state === 'seated' && !u.held) return prev.state === 'failed' ? `${name} back` : prev.held ? `${name} moved` : `${name} ready`;
  return status ? `${name} ${status}` : null;
}

/** Whether sound passes plate `u`: it is seated (or reloading: its old instance plays on) and on, and an
 * input-fed chain is live. */
function sounding(u: UnitState, live: boolean, head: UnitState | undefined): boolean {
  return (u.state === 'seated' || u.reloading === true) && !u.bypass && (live || head?.source === true);
}

/** The x (in the line's box) of the drop marker before plate `gap` (or after the last). */
function dropX(line: HTMLElement, gap: number): number {
  const plates = [...line.querySelectorAll<HTMLElement>(':scope > .unit')];
  const box = line.getBoundingClientRect();
  if (plates.length === 0) return 0;
  if (gap >= plates.length) {
    const r = plates[plates.length - 1].getBoundingClientRect();
    return r.right - box.left + line.scrollLeft + 4;
  }
  const r = plates[gap].getBoundingClientRect();
  return r.left - box.left + line.scrollLeft - 6;
}

/** The kind's words for a plate's accessible name. */
function kindWords(u: UnitState): string {
  if (u.kind === 'plugin') return `${u.plugin.format.toUpperCase()} plugin`;
  if (u.kind === 'synth') return 'built-in instrument';
  return 'built-in effect';
}

/** What a plate reads as its name: a synth its picked instrument, any other unit its own name. */
function plateName(u: UnitState): string {
  if (u.kind === 'synth') {
    const i = 'params' in u ? (u.params?.instrument ?? 0) : 0;
    return SYNTHS[i]?.name ?? u.name;
  }
  return u.name;
}

/** Pixels a pointer moves on a plate before it is a drag. */
const DRAG_THRESHOLD = 5;

/** A drag in flight: its unit, how far the plate follows the pointer, the gap a release would drop it in
 * (null: no move there, or one the chain refuses). */
interface Drag {
  unit: number;
  dx: number;
  gap: number | null;
}

/** The gap (0..n) the content-x `x` falls in, from the plates' middles measured before the drag moved any. */
function gapOf(mids: readonly number[], x: number): number {
  const k = mids.findIndex((m) => x < m);
  return k < 0 ? mids.length : k;
}

/** Swallow the click a drag's release makes: pointer `id`'s (never a keyboard's, never another pointer's),
 * only in this task. */
function swallowNextClick(id: number): void {
  const eat = (e: MouseEvent) => {
    if (e.detail === 0 || (e instanceof PointerEvent && e.pointerId !== id)) return;
    window.removeEventListener('click', eat, { capture: true });
    e.preventDefault();
    e.stopPropagation();
  };
  window.addEventListener('click', eat, { capture: true });
  setTimeout(() => window.removeEventListener('click', eat, { capture: true }), 0);
}

function UnitPlate(props: {
  slot: 0 | 1;
  unit: () => UnitState;
  index: () => number;
  count: () => number;
  held: () => boolean;
  live: boolean;
  line: () => HTMLElement;
  drag: () => Drag | null;
  setDrag: (d: Drag | null) => void;
  returnFocus?: (el: HTMLElement | undefined) => void;
}) {
  const u = props.unit;
  const letter = slotLetter(props.slot);
  const pop = createRowPopover(props.returnFocus);
  let plate: HTMLDivElement | undefined;
  const seated = () => u().state === 'seated';
  const status = () => unitStatus(u(), props.held());
  const name = () => plateName(u());
  const label = () => `${name()}, ${kindWords(u())}${status() ? `, ${status()}` : ''}`;
  const dragging = () => props.drag()?.unit === u().id;
  const failedNote = () => (u().state === 'failed' ? status() : null);

  // ── the drag: one press per pointer, from its down to its release, cancel, capture loss or Escape ────
  // The gap is judged from the plates' places measured when the drag began (the lifted plate never moves
  // the targets), and judged again at the release: the move sent is the release's, whatever the steps.
  let press: { id: number; x: number; y: number; phase: 'pending' | 'drag' | 'cancelled'; mids: number[]; order: string; at: number } | null = null;
  const order = () => [...props.line().querySelectorAll<HTMLElement>(':scope > .unit')].map((e) => e.dataset.unit).join();
  const contentX = (clientX: number) => clientX - props.line().getBoundingClientRect().left + props.line().scrollLeft;
  /** Where a release at content-x `x` would move the plate (its index), or null for no move. */
  const target = (x: number): number | null => {
    if (!press) return null;
    const gap = gapOf(press.mids, x);
    const i = press.at;
    if (gap === i || gap === i + 1) return null;
    return gap > i ? gap - 1 : gap;
  };
  const end = () => {
    const p = press;
    press = null;
    window.removeEventListener('pointermove', onWinMove, true);
    window.removeEventListener('pointerup', onWinUp, true);
    window.removeEventListener('pointercancel', onWinCancel, true);
    window.removeEventListener('keydown', onKey, true);
    if (p && plate?.hasPointerCapture(p.id)) plate.releasePointerCapture(p.id);
    if (props.drag()?.unit === u().id) props.setDrag(null);
  };
  const onWinMove = (e: PointerEvent) => {
    if (!press || e.pointerId !== press.id || press.phase === 'cancelled') return;
    const dx = e.clientX - press.x;
    if (press.phase === 'pending') {
      if (Math.hypot(dx, e.clientY - press.y) < DRAG_THRESHOLD) return;
      const box = props.line().getBoundingClientRect();
      const plates = [...props.line().querySelectorAll<HTMLElement>(':scope > .unit')];
      press.mids = plates.map((el) => {
        const r = el.getBoundingClientRect();
        return r.left - box.left + props.line().scrollLeft + r.width / 2;
      });
      press.order = order();
      press.at = props.index();
      press.phase = 'drag';
      try {
        plate?.setPointerCapture(e.pointerId);
      } catch {
        /* the pointer is gone: its release or cancel ends the press */
      }
    }
    const to = target(contentX(e.clientX));
    const gap = to === null || moveRefusal(u().id, to) !== null ? null : gapOf(press.mids, contentX(e.clientX));
    props.setDrag({ unit: u().id, dx, gap });
  };
  const onWinUp = (e: PointerEvent) => {
    if (!press || e.pointerId !== press.id) return;
    const p = press;
    // confirmed on the release: the gap at the release's own x, on the chain the drag began with
    const to = p.phase === 'drag' && order() === p.order ? target(contentX(e.clientX)) : null;
    end();
    if (p.phase === 'pending') return;
    swallowNextClick(p.id);
    if (to !== null) void moveUnit(u().id, to);
  };
  const onWinCancel = (e: PointerEvent) => {
    if (press && e.pointerId === press.id) end();
  };
  const onKey = (e: KeyboardEvent) => {
    if (e.key !== 'Escape' || !press) return;
    if (press.phase === 'cancelled') {
      // a held Escape's repeats: the press still waits for its release, whose click it eats
      e.preventDefault();
      e.stopPropagation();
      return;
    }
    if (press.phase === 'pending') {
      // a press not yet a drag lets Escape through to whatever else it closes
      end();
      return;
    }
    e.preventDefault();
    e.stopPropagation();
    // cancelled: the marker and the lift go now; the press waits for its release, whose click it eats
    press.phase = 'cancelled';
    props.setDrag(null);
    if (plate?.hasPointerCapture(press.id)) plate.releasePointerCapture(press.id);
  };
  const onLostCapture = (e: PointerEvent) => {
    // a capture the drag did not let go of (the window lost the pointer): the drag ends, nothing sent
    if (press && e.pointerId === press.id && press.phase === 'drag') end();
  };
  onCleanup(end);
  // a report that unseats the plate (parked, withdrawn, leaving) or leaves it alone in its chain ends its
  // press at once: its capture and its listeners go with it. Read before `press`, which is no signal.
  createEffect(() => {
    const movable = seated() && props.count() >= 2;
    if (!movable && press) end();
  });
  const onDown = (e: PointerEvent) => {
    if (press || e.button !== 0 || !seated() || props.count() < 2) return;
    // Solid's delegated events reach the plate from its portals too (its menu, a popover):
    // only a press on the plate itself is a drag
    if (!plate?.contains(e.target as Node)) return;
    // the lamp, the editor glyph and the menu are their own presses
    if ((e.target as HTMLElement).closest('.plate__lamp, .plate__ed, .plate__menu')) return;
    press = { id: e.pointerId, x: e.clientX, y: e.clientY, phase: 'pending', mids: [], order: '', at: props.index() };
    window.addEventListener('pointermove', onWinMove, true);
    window.addEventListener('pointerup', onWinUp, true);
    window.addEventListener('pointercancel', onWinCancel, true);
    window.addEventListener('keydown', onKey, true);
  };

  // ── a popover open on a unit that stops being seated (withdrawn, parked, leaving) closes; focus that was
  // in it goes to the plate's menu, the control that survives every state (a face: `FaceHost.tsx`) ──
  createEffect(() => {
    if (!pop.open() || seated()) return;
    const inside = document.activeElement?.closest('.slot-pop') != null;
    pop.close(false);
    if (!inside) return;
    queueMicrotask(() => {
      const menu = plate?.querySelector<HTMLElement>('.plate__menu') ?? undefined;
      if (props.returnFocus) props.returnFocus(menu);
      else menu?.focus();
    });
  });

  // the built-in effects (OTT, ECHO, REVERB, RING MOD) open their own face (`FaceHost.tsx`), free-standing; the rest their popover
  const face = (): FaceKind | null => {
    const k = u().kind;
    return k === 'ott' || k === 'echo' || k === 'reverb' || k === 'ring' ? k : null;
  };
  const isOpen = () => (face() ? faceOpen(u().id) : pop.open());
  const openParams = () => {
    const k = face();
    if (k) toggleFace(u().id, k);
    else pop.toggle();
  };
  let nameEl: HTMLElement | undefined;
  createEffect(() => {
    const id = u().id;
    if (!plate || !face()) return;
    const el = plate;
    onCleanup(
      registerPlate(id, {
        el,
        focus: (to) => {
          const target = to === 'name' ? nameEl : (el.querySelector<HTMLElement>('.plate__menu') ?? undefined);
          if (props.returnFocus) props.returnFocus(target);
          else target?.focus();
        },
      }),
    );
  });
  const params = () => u().kind === 'plugin' || u().kind === 'synth' || face() !== null || u().kind in KIND_SENDS;
  const items = (): PlateMenuItem[][] => {
    const i = props.index();
    const gone = u().state === 'leaving';
    const first: PlateMenuItem[] = [];
    if (u().kind === 'plugin')
      first.push({ key: 'editor', label: 'Editor', icon: 'win', checked: editorOpen(u().id), disabled: !seated(), onSelect: () => void toggleEditor(u().id) });
    if (params()) first.push({ key: 'params', label: 'Parameters', icon: 'params', checked: isOpen(), disabled: !seated(), onSelect: () => seated() && openParams() });
    return [
      first,
      [
        { key: 'left', label: 'Move left', icon: 'left', disabled: !seated() || i === 0, onSelect: () => void moveUnit(u().id, props.index() - 1) },
        { key: 'right', label: 'Move right', icon: 'right', disabled: !seated() || i >= props.count() - 1, onSelect: () => void moveUnit(u().id, props.index() + 1) },
      ],
      [{ key: 'remove', label: 'Remove', icon: 'x', disabled: gone, onSelect: () => void removeUnit(u().id) }],
    ];
  };
  return (
    <div
      ref={plate}
      class="plate unit"
      classList={{
        'plate--byp': u().bypass || !seated(),
        'plate--idle': seated() && !u().bypass && !props.live && u().kind !== 'synth' && !u().source,
        'unit--busy': u().state === 'loading' || u().state === 'waiting' || u().state === 'leaving',
        'unit--failed': u().state === 'failed',
        'unit--parked': u().state === 'failed' && u().parked === true,
        'unit--held': u().held === true,
        'unit--open': isOpen(),
        'unit--dragging': dragging(),
      }}
      role="listitem"
      aria-label={label()}
      data-unit={u().id}
      data-kind={u().kind}
      data-state={u().state}
      title={(() => {
        const x = u();
        return x.kind === 'plugin' ? x.plugin.path : undefined;
      })()}
      style={dragging() ? { translate: `${props.drag()!.dx}px 0` } : undefined}
      onPointerDown={onDown}
      on:lostpointercapture={onLostCapture}
    >
      <button
        type="button"
        class="plate__lamp"
        classList={{ 'is-busy': u().state === 'loading' || u().state === 'waiting' }}
        aria-pressed={seated() && !u().bypass}
        aria-label={`${name()} on`}
        title={!seated() ? (status() ?? '') : u().bypass ? 'Bypassed: press to switch on' : 'On: press to bypass'}
        disabled={!seated()}
        onClick={(e) => {
          e.stopPropagation();
          toggleUnit(u().id);
        }}
      />
      <span class="unit__text">
        <Show when={params()} fallback={<span class="plate__name">{name()}</span>}>
          <button
            type="button"
            ref={(el) => {
              pop.trigger(el);
              nameEl = el;
            }}
            class="unit__name slot-pop__trigger"
            aria-haspopup="dialog"
            aria-expanded={isOpen()}
            aria-label={`${name()}: parameters`}
            disabled={!seated()}
            onClick={(e) => {
              e.stopPropagation();
              if (seated()) openParams();
            }}
          >
            <span class="plate__name" classList={{ 'plate__name--fx': u().kind !== 'plugin' && u().kind !== 'synth' }}>
              {name()}
            </span>
          </button>
        </Show>
        <Show when={status() && (u().state !== 'seated' || u().held)}>
          <span class="unit__status" title={status() ?? ''}>
            {status()}
          </span>
        </Show>
      </span>
      <UnitMark unit={u()} seated={seated()} compact={chainCompact()} />
      <PlateMenu
        id={`unit-${u().id}-menu`}
        label={`Menu for ${name()} in slot ${letter}`}
        menuLabel={`Slot ${letter}: ${name()}`}
        note={failedNote()}
        returnFocus={props.returnFocus}
        groups={items()}
      />
      <Show when={pop.open() && !face()}>
        <RowPopoverPanel pop={pop} id={`unit-${u().id}-params`} label={`${name()} parameters`} role="dialog" class="infx-pop">
          <UnitParams unit={u} />
        </RowPopoverPanel>
      </Show>
    </div>
  );
}

/** The small mark at a plate's right: a plugin's format and its editor glyph; the OTT's live miniature; an
 * effect's glyph of what it does; a synth's keys. */
function UnitMark(props: { unit: UnitState; seated: boolean; compact: boolean }) {
  const u = () => props.unit;
  return (
    <span class="plate__what">
      <Show when={u().kind === 'plugin'}>
        <span class="plate__fmt" aria-hidden="true">
          {u().kind === 'plugin' ? (u() as Extract<UnitState, { kind: 'plugin' }>).plugin.format.toUpperCase() : ''}
        </span>
        {/* compact: the editor stays in the plate's menu */}
        <Show when={!props.compact}>
        <button
          type="button"
          class="plate__ed"
          aria-pressed={editorOpen(u().id)}
          aria-label={`Editor for ${u().name}`}
          title={editorOpen(u().id) ? "Close the plugin's window" : "Open the plugin's window"}
          disabled={!props.seated}
          onClick={(e) => {
            e.stopPropagation();
            void toggleEditor(u().id);
          }}
        >
          <Icon name="win" size={16} />
        </button>
        </Show>
      </Show>
      {/* the OTT's live miniature stays in a compact plate: what the unit is doing, at a glance */}
      <Show when={u().kind === 'ott'}>
        <span
          class="unit-mini unit-mini--ott"
          aria-hidden="true"
          ref={(el) => onMount(() => onCleanup(registerOttMini(el, u().id)))}
        >
          <i style={{ '--b': 0 }} />
          <i style={{ '--b': 1 }} />
          <i style={{ '--b': 2 }} />
        </span>
      </Show>
      <Show when={!props.compact && (u().kind === 'echo' || u().kind === 'reverb' || u().kind === 'ring')}>
        <span class="unit-mini" aria-hidden="true">
          <svg width="30" height="16" viewBox="0 0 30 16" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round">
            <path d={GLYPHS[u().kind as 'echo' | 'reverb' | 'ring']} />
          </svg>
        </span>
      </Show>
      <Show when={!props.compact && u().kind === 'synth'}>
        <span class="unit-mini unit-mini--bare" aria-hidden="true">
          <Icon name="keys" size={18} />
        </span>
      </Show>
    </span>
  );
}

/** A built-in effect's glyph: ECHO three fading taps, REVERB a tail, RING MOD a carrier. */
const GLYPHS = {
  echo: 'M5 3v10M13 5.5v5M21 7v2',
  reverb: 'M4 3v10M4 8c5 0 8 2 22 4',
  ring: 'M3 8c2.5-6 5-6 7.5 0s5 6 7.5 0 5-6 7.5 0',
} as const;

/** ECHO, REVERB and RING MOD take today's settings (the old sends' defs); REVERB and RING MOD show them in
 * the row's popover, ECHO on its own face. */
const KIND_SENDS: Record<string, 'echo' | 'reverb' | 'ring'> = { echo: 'echo', reverb: 'reverb', ring: 'ring' };
const BLURBS: Record<string, string> = {
  echo: 'Repeats in time with the tempo',
  reverb: 'A room around the note',
  ring: 'Metallic, bell-like overtones',
  synth: 'Each instrument’s level',
  plugin: 'The plugin’s first parameters',
};

interface ParamRow {
  key: UnitParamId;
  def: FxParamDef;
}
const synthRows = (): ParamRow[] =>
  SYNTHS.map((s) => ({
    key: `gain${s.name}` as UnitParamId,
    def: { label: s.name, min: 0, max: 1.5, step: 0.01, default: 1 } as FxParamDef,
  }));

/** A unit's parameters in the row's settings popover: a built-in's from the feed (`SetUnitParam`), a
 * plugin's through its host (`listParams`, `setParameter`). */
function UnitParams(props: { unit: () => UnitState }) {
  const u = props.unit;
  const kind = u().kind;
  const rows = (): ParamRow[] =>
    kind === 'synth'
      ? synthRows()
      : engineInputSends.params.filter((d) => d.send === KIND_SENDS[kind]).map((d) => ({ key: d.key as UnitParamId, def: d }));
  const value = (r: ParamRow): number => ('params' in u() ? (u() as { params?: Partial<Record<UnitParamId, number>> }).params?.[r.key] : undefined) ?? r.def.default;
  const show = (r: ParamRow, v: number): string => (kind === 'synth' ? `${Math.round(v * 100)} %` : formatVal(v, r.def));
  return (
    <div class="fxp infx infx--one">
      <p class="infx__head">
        <span class="infx__name">{plateName(u())}</span>
        <span class="infx__blurb">{BLURBS[kind] ?? ''}</span>
      </p>
      <Show when={kind !== 'plugin'} fallback={<PluginParamRows unit={u().id} />}>
        <div class="fxp-mod__params">
          <For each={rows()}>
            {(r) => (
              <label class="fxp-param">
                <span class="fxp-param__label">{r.def.label}</span>
                <Show
                  when={r.def.choices}
                  fallback={
                    <input
                      type="range"
                      class="lf-range fxp-param__range"
                      min={r.def.min}
                      max={r.def.max}
                      step={r.def.step}
                      value={value(r)}
                      aria-label={`${plateName(u())} ${r.def.label.toLowerCase()}`}
                      aria-valuetext={show(r, value(r))}
                      data-param={r.key}
                      onInput={(e) => setUnitParam(u().id, r.key, Number(e.currentTarget.value))}
                    />
                  }
                >
                  <select
                    class="fxp-param__select"
                    value={value(r)}
                    aria-label={`${plateName(u())} ${r.def.label.toLowerCase()}`}
                    data-param={r.key}
                    onChange={(e) => setUnitParam(u().id, r.key, Number(e.currentTarget.value))}
                  >
                    <For each={r.def.choices}>{(c, i) => <option value={i()}>{c}</option>}</For>
                  </select>
                </Show>
                <Show when={!r.def.choices}>
                  <span class="fxp-param__val">{show(r, value(r))}</span>
                </Show>
              </label>
            )}
          </For>
        </div>
      </Show>
    </div>
  );
}

/** Rows a plugin's parameters preview shows at most (as the old drawer's). */
const PLUGIN_PARAMS = 12;

function PluginParamRows(props: { unit: number }) {
  const [list] = createResource(
    () => props.unit,
    (unit) =>
      platform.chain.listParams(unit).catch((e: unknown): PluginParamDesc[] => {
        console.error('[chain] plugin parameters failed', e);
        return [];
      }),
  );
  return (
    <Show when={(list() ?? []).length > 0} fallback={<p class="unit-params__none">{list.loading ? 'Reading parameters…' : 'This plugin lists no parameters. Its editor has them.'}</p>}>
      <div class="fxp-mod__params">
        <For each={(list() ?? []).slice(0, PLUGIN_PARAMS)}>
          {(p) => (
            <label class="fxp-param">
              <span class="fxp-param__label" title={p.name}>
                {p.name}
              </span>
              <input
                type="range"
                class="lf-range fxp-param__range"
                min={p.minValue}
                max={p.maxValue}
                step={(p.maxValue - p.minValue) / 200}
                value={p.value}
                aria-label={p.name}
                onInput={(e) => setPluginParam(props.unit, p.id, Number(e.currentTarget.value))}
              />
            </label>
          )}
        </For>
      </div>
    </Show>
  );
}

/** The `+` bay at the chain's end: a menu of the built-in effects, the built-in instruments (a pick on the
 * SYNTH that already heads the chain, `chooseSynth`) and the scanned plugins; a refusal comes back as
 * the slot's tag beside it. */
function AddBay(props: {
  slot: 0 | 1;
  returnFocus?: (el: HTMLElement | undefined) => void;
  button: (el: HTMLButtonElement) => void;
  onFocusIn: (e: FocusEvent) => void;
  onFocusOut: (e: FocusEvent) => void;
}) {
  const pop = createRowPopover(props.returnFocus);
  const letter = slotLetter(props.slot);
  const id = `unit-add-${props.slot}`;
  const add = (spec: Parameters<typeof addUnit>[1]) => () => void addUnit(props.slot, spec);
  const item = (label: string, onSelect: () => void, extra?: string) => (
    <button
      type="button"
      class="slot-menu__item"
      role="menuitem"
      title={extra}
      onClick={(e) => {
        pop.close(e.detail === 0);
        onSelect();
      }}
    >
      <span>{label}</span>
    </button>
  );
  return (
    <>
      <button
        type="button"
        ref={(el) => {
          pop.trigger(el);
          props.button(el);
        }}
        class="unit-add slot-pop__trigger"
        aria-label={`Add to slot ${letter}'s chain`}
        aria-haspopup="menu"
        aria-expanded={pop.open()}
        aria-controls={id}
        title="Add an effect, an instrument or a plugin"
        onFocusIn={props.onFocusIn}
        onFocusOut={props.onFocusOut}
        onClick={(e) => {
          e.stopPropagation();
          pop.toggle();
        }}
      >
        <Icon name="plus" size={16} />
      </button>
      <RowPopoverPanel pop={pop} id={id} label={`Add to slot ${letter}`} role="menu" class="slot-menu unit-add-menu">
        <p class="unit-add-menu__head">Effects</p>
        {item('ECHO', add({ kind: 'echo' }))}
        {item('REVERB', add({ kind: 'reverb' }))}
        {item('RING MOD', add({ kind: 'ring' }))}
        {item('OTT', add({ kind: 'ott' }))}
        <div class="slot-menu__sep" role="separator" />
        <p class="unit-add-menu__head">Instruments</p>
        <For each={SYNTHS}>{(s, i) => item(s.name, () => chooseSynth(props.slot, i()))}</For>
        <Show when={pickerPlugins().length > 0}>
          <div class="slot-menu__sep" role="separator" />
          <p class="unit-add-menu__head">Plugins</p>
          <For each={pickerPlugins()}>
            {(p) => item(pluginPickerLabel(p, pickerPlugins()), add({ kind: 'plugin', plugin: { format: p.format, path: p.path, id: p.id } }), p.path)}
          </For>
        </Show>
      </RowPopoverPanel>
    </>
  );
}
