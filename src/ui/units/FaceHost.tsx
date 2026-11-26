import { For, Match, Switch, createEffect, onCleanup } from 'solid-js';
import { Portal } from 'solid-js/web';
import type { UnitState } from '../../platform';
import { slotChain } from '../state/chain';
import { stageOpen } from '../stage/stage-store';
import { EchoFace } from './EchoFace';
import { suspendEchoDraw } from './echo-loop';
import { OttFace } from './OttFace';
import { suspendOttDraw } from './ott-loop';
import { ReverbFace } from './ReverbFace';
import { suspendReverbDraw } from './reverb-loop';
import { closeFace, openFaces, plateOf, type OpenFace } from './face-store';

/**
 * The open unit faces (`face-store.ts`), drawn once for the app, apart from the slot row: a face outlives
 * its plate's line (a slot folded under it), and the row's popovers stay its own. Its layer sits over the
 * row and the lanes and under the row's popovers; it takes no pointer itself, so everything beside a face
 * plays as ever. A face closes when its unit stops being seated or leaves its chain (the focus that was in
 * it goes to the plate's menu, or a surviving chain control / fold button through `returnFocus`). While the
 * stage view is open the face layer is hidden and inert and the faces' draw loops queue nothing, and the
 * same faces return on exit; under the tuner the layer is inert too.
 */
/** The slot each open face's unit last stood in (a unit gone from the feed still closes its face by it);
 * pruned to the open faces. */
const slots = new Map<number, 0 | 1>();
/** How many units' last slots are kept. @public (the unit-faces probe: a closed face keeps none) */
export const faceSlotRecords = (): number => slots.size;

export function FaceHost(props: { inert?: boolean; returnFocus?: (el: HTMLElement | undefined) => void }) {
  const find = (unit: number): { u: UnitState; slot: 0 | 1 } | null => {
    for (const slot of [0, 1] as const) {
      const u = slotChain(slot).units.find((x) => x.id === unit);
      if (u) return { u, slot };
    }
    return null;
  };
  const fallback = (slot: 0 | 1): HTMLElement | undefined => {
    const root = document.querySelector(`.slot--${slot === 1 ? 'b' : 'a'}`);
    return (
      root?.querySelector<HTMLElement>('.slot__fold:not(:disabled)') ??
      root?.querySelector<HTMLElement>('.unit .plate__menu:not(:disabled)') ??
      root?.querySelector<HTMLElement>('.unit-add:not(:disabled)') ??
      root?.querySelector<HTMLElement>('.slot__id:not(:disabled)') ??
      undefined
    );
  };
  const handFocus = (unit: number, slot: 0 | 1, to: 'name' | 'menu') => {
    const p = plateOf(unit);
    if (p?.el.isConnected) {
      p.focus(to);
      if (document.activeElement && document.activeElement !== document.body && p.el.contains(document.activeElement)) return;
    }
    const el = fallback(slot);
    if (props.returnFocus) props.returnFocus(el);
    else el?.focus();
  };
  createEffect(() => {
    const opened = openFaces();
    for (const unit of slots.keys()) if (!opened.some((f) => f.unit === unit)) slots.delete(unit);
    for (const f of opened) {
      const at = find(f.unit);
      if (at) slots.set(f.unit, at.slot);
      if (at?.u.state === 'seated') continue;
      const slot = at?.slot ?? slots.get(f.unit) ?? 0;
      slots.delete(f.unit);
      const inside = document.activeElement?.closest(`#${f.kind}-face-${f.unit}`) != null;
      closeFace(f.unit);
      if (inside) queueMicrotask(() => handFocus(f.unit, slot, 'menu'));
    }
  });
  createEffect(() => {
    const hidden = stageOpen();
    suspendEchoDraw(hidden);
    suspendOttDraw(hidden);
    suspendReverbDraw(hidden);
  });
  onCleanup(() => {
    suspendEchoDraw(false);
    suspendOttDraw(false);
    suspendReverbDraw(false);
    slots.clear();
  });
  return (
    <Portal>
      <div class="uface-layer" hidden={stageOpen()} inert={props.inert || stageOpen()}>
        <For each={openFaces()}>{(f) => <Face face={f} find={find} handFocus={handFocus} />}</For>
      </div>
    </Portal>
  );
}

function Face(props: {
  face: OpenFace;
  find: (unit: number) => { u: UnitState; slot: 0 | 1 } | null;
  handFocus: (unit: number, slot: 0 | 1, to: 'name' | 'menu') => void;
}) {
  const f = props.face;
  // the unit as the feed last told it (a face closing on its unit's departure draws its last state)
  let last: { u: UnitState; slot: 0 | 1 } | null = null;
  const at = () => (last = props.find(f.unit) ?? last);
  const unit = () => at()!.u;
  const slotLetter = () => (at()?.slot === 1 ? 'B' : 'A');
  const close = (keyboard: boolean) => {
    const slot = at()?.slot ?? 0;
    closeFace(f.unit);
    if (keyboard) props.handFocus(f.unit, slot, 'name');
  };
  return (
    <Switch>
      <Match when={!!at() && f.kind === 'ott'}>
        <OttFace unit={unit} slotLetter={slotLetter} close={close} />
      </Match>
      <Match when={!!at() && f.kind === 'echo'}>
        <EchoFace unit={unit} slotLetter={slotLetter} close={close} />
      </Match>
      <Match when={!!at() && f.kind === 'reverb'}>
        <ReverbFace unit={unit} slotLetter={slotLetter} close={close} />
      </Match>
    </Switch>
  );
}
