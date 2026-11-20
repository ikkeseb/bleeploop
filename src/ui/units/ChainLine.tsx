import { For, Show, createResource, createSignal, onCleanup, onMount } from 'solid-js';
import { platform, type PluginParamDesc, type UnitParamId, type UnitState } from '../../platform';
import {
  addUnit,
  chainRefusal,
  editorOpen,
  moveUnit,
  pickerPlugins,
  removeUnit,
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
import { OttFace } from './OttFace';
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
  const [drag, setDrag] = createSignal<{ unit: number; dx: number; gap: number | null } | null>(null);
  return (
    <>
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
              <i class="slot__sig" classList={{ 'is-dim': !sounding(u(), props.live, chain().units[0]) }} aria-hidden="true" />
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
      <i class="slot__sig" classList={{ 'is-dim': chain().units.length > 0 && !props.live && chain().units[0]?.source !== true }} aria-hidden="true" />
      <AddBay slot={props.slot} returnFocus={props.returnFocus} />
    </div>
    <Show when={chainRefusal(props.slot)}>
      <span class="pc__err unit-err" role="status" data-slot={props.slot} title={chainRefusal(props.slot) ?? ''}>
        {chainRefusal(props.slot)}
      </span>
    </Show>
    </>
  );
}

/** Whether sound passes plate `u`: it is seated and on, and an input-fed chain is live. */
function sounding(u: UnitState, live: boolean, head: UnitState | undefined): boolean {
  return u.state === 'seated' && !u.bypass && (live || head?.source === true);
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

function UnitPlate(props: {
  slot: 0 | 1;
  unit: () => UnitState;
  index: () => number;
  count: () => number;
  held: () => boolean;
  live: boolean;
  line: () => HTMLElement;
  drag: () => { unit: number; dx: number; gap: number | null } | null;
  setDrag: (d: { unit: number; dx: number; gap: number | null } | null) => void;
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

  // ── the drag: pointer capture once past the threshold, a marker between plates, Escape cancels ──
  let press: { id: number; x: number; y: number; captured: boolean } | null = null;
  let swallowClick = false;
  const gapAt = (x: number): number => {
    const plates = [...props.line().querySelectorAll<HTMLElement>(':scope > .unit')];
    let gap = plates.length;
    for (let k = 0; k < plates.length; k++) {
      const r = plates[k].getBoundingClientRect();
      if (x < r.left + r.width / 2) {
        gap = k;
        break;
      }
    }
    return gap;
  };
  const cancel = () => {
    press = null;
    props.setDrag(null);
    window.removeEventListener('keydown', onKey, true);
  };
  const onKey = (e: KeyboardEvent) => {
    if (e.key !== 'Escape') return;
    e.preventDefault();
    e.stopPropagation();
    swallowClick = true;
    cancel();
  };
  onCleanup(() => window.removeEventListener('keydown', onKey, true));
  const onDown = (e: PointerEvent) => {
    // a drag's own click never outlives it: one cancelled away from the plate left no click to swallow
    swallowClick = false;
    if (e.button !== 0 || !seated() || props.count() < 2) return;
    // Solid's delegated events reach the plate from its portals too (the OTT face's knobs, a popover):
    // only a press on the plate itself is a drag
    if (!plate?.contains(e.target as Node)) return;
    // the lamp, the editor glyph and the menu are their own presses
    if ((e.target as HTMLElement).closest('.plate__lamp, .plate__ed, .plate__menu')) return;
    press = { id: e.pointerId, x: e.clientX, y: e.clientY, captured: false };
  };
  const onMove = (e: PointerEvent) => {
    if (!press || press.id !== e.pointerId) return;
    const dx = e.clientX - press.x;
    if (!press.captured) {
      if (Math.hypot(dx, e.clientY - press.y) < DRAG_THRESHOLD) return;
      press.captured = true;
      plate?.setPointerCapture(e.pointerId);
      window.addEventListener('keydown', onKey, true);
    }
    const gap = gapAt(e.clientX);
    // a gap either side of the plate itself is no move
    const i = props.index();
    props.setDrag({ unit: u().id, dx, gap: gap === i || gap === i + 1 ? null : gap });
  };
  const onUp = (e: PointerEvent) => {
    if (!press || press.id !== e.pointerId) return;
    const was = press.captured;
    const d = props.drag();
    cancel();
    if (!was) return;
    swallowClick = true;
    if (d && d.gap !== null) {
      const i = props.index();
      void moveUnit(u().id, d.gap > i ? d.gap - 1 : d.gap);
    }
  };
  const onClickCapture = (e: MouseEvent) => {
    if (!swallowClick) return;
    swallowClick = false;
    e.preventDefault();
    e.stopPropagation();
  };

  const isOtt = () => u().kind === 'ott';
  const params = () => (u().kind === 'plugin' || u().kind === 'synth' || isOtt() || u().kind in KIND_SENDS);
  const items = (): PlateMenuItem[][] => {
    const i = props.index();
    const gone = u().state === 'leaving';
    const first: PlateMenuItem[] = [];
    if (u().kind === 'plugin')
      first.push({ label: 'Editor', icon: 'win', checked: editorOpen(u().id), disabled: !seated(), onSelect: () => void toggleEditor(u().id) });
    if (params()) first.push({ label: 'Parameters', icon: 'params', checked: pop.open(), disabled: !seated() && u().state !== 'waiting', onSelect: () => pop.toggle() });
    return [
      first,
      [
        { label: 'Move left', icon: 'left', disabled: !seated() || i === 0, onSelect: () => void moveUnit(u().id, i - 1) },
        { label: 'Move right', icon: 'right', disabled: !seated() || i >= props.count() - 1, onSelect: () => void moveUnit(u().id, i + 1) },
      ],
      [{ label: 'Remove', icon: 'x', disabled: gone, onSelect: () => void removeUnit(u().id) }],
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
        'unit--open': pop.open(),
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
      onPointerMove={onMove}
      onPointerUp={onUp}
      onPointerCancel={() => cancel()}
      on:click={{ handleEvent: onClickCapture, capture: true }}
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
            ref={pop.trigger}
            class="unit__name slot-pop__trigger"
            aria-haspopup="dialog"
            aria-expanded={pop.open()}
            aria-label={`${name()}: parameters`}
            disabled={!seated() && u().state !== 'waiting'}
            onClick={(e) => {
              e.stopPropagation();
              pop.toggle();
            }}
          >
            <span class="plate__name" classList={{ 'plate__name--fx': u().kind !== 'plugin' && u().kind !== 'synth' }}>
              {name()}
            </span>
          </button>
        </Show>
        <Show when={status() && u().state !== 'seated'}>
          <span class="unit__status" title={status() ?? ''}>
            {status()}
          </span>
        </Show>
      </span>
      <Show when={!chainCompact()}>
        <UnitMark unit={u()} seated={seated()} />
      </Show>
      <PlateMenu
        id={`unit-${u().id}-menu`}
        label={`Menu for ${name()} in slot ${letter}`}
        menuLabel={`Slot ${letter}: ${name()}`}
        returnFocus={props.returnFocus}
        groups={items()}
      />
      <Show when={pop.open()}>
        <Show
          when={isOtt()}
          fallback={
            <RowPopoverPanel pop={pop} id={`unit-${u().id}-params`} label={`${name()} parameters`} role="dialog" class="infx-pop">
              <UnitParams unit={u} />
            </RowPopoverPanel>
          }
        >
          <OttFace pop={pop} unit={u} plate={() => plate} slotLetter={letter} />
        </Show>
      </Show>
    </div>
  );
}

/** The small mark at a plate's right: a plugin's format and its editor glyph; the OTT's live miniature; an
 * effect's glyph of what it does; a synth's keys. */
function UnitMark(props: { unit: UnitState; seated: boolean }) {
  const u = () => props.unit;
  return (
    <span class="plate__what">
      <Show when={u().kind === 'plugin'}>
        <span class="plate__fmt" aria-hidden="true">
          {u().kind === 'plugin' ? (u() as Extract<UnitState, { kind: 'plugin' }>).plugin.format.toUpperCase() : ''}
        </span>
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
      <Show when={u().kind === 'echo' || u().kind === 'reverb' || u().kind === 'ring'}>
        <span class="unit-mini" aria-hidden="true">
          <svg width="30" height="16" viewBox="0 0 30 16" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round">
            <path d={GLYPHS[u().kind as 'echo' | 'reverb' | 'ring']} />
          </svg>
        </span>
      </Show>
      <Show when={u().kind === 'synth'}>
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

/** ECHO, REVERB and RING MOD keep today's settings (the old sends' defs and their popover's look). */
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
                onInput={(e) => void platform.chain.setParameter(props.unit, p.id, Number(e.currentTarget.value)).catch((err: unknown) => console.error('[chain] plugin parameter refused', err))}
              />
            </label>
          )}
        </For>
      </div>
    </Show>
  );
}

/** The `+` bay at the chain's end: a menu of the built-in effects, the built-in instruments and the
 * scanned plugins; a refusal comes back as the slot's tag beside it. */
function AddBay(props: { slot: 0 | 1; returnFocus?: (el: HTMLElement | undefined) => void }) {
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
        ref={pop.trigger}
        class="unit-add slot-pop__trigger"
        aria-label={`Add to slot ${letter}'s chain`}
        aria-haspopup="menu"
        aria-expanded={pop.open()}
        aria-controls={id}
        title="Add an effect, an instrument or a plugin"
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
        <For each={SYNTHS}>{(s, i) => item(s.name, add({ kind: 'synth', params: { instrument: i() } }))}</For>
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
