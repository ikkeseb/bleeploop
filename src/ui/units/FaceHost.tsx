import { For, Match, Switch, createEffect } from 'solid-js';
import { Portal } from 'solid-js/web';
import type { UnitState } from '../../platform';
import { slotChain } from '../state/chain';
import { stageOpen } from '../stage/stage-store';
import { EchoFace } from './EchoFace';
import { OttFace } from './OttFace';
import { closeFace, openFaces, plateOf, type OpenFace } from './face-store';

/**
 * The open unit faces (`face-store.ts`), drawn once for the app, apart from the slot row: a face outlives
 * its plate's line (a slot folded under it), and the row's popovers stay its own. Its layer sits over the
 * row and the lanes and under the row's popovers; it takes no pointer itself, so everything beside a face
 * plays as ever. A face closes when its unit stops being seated or leaves its chain (the focus that was in
 * it goes to the plate's menu, the control that survives every state), and when the stage view opens over
 * the row.
 */
export function FaceHost() {
  const find = (unit: number): { u: UnitState; slot: 0 | 1 } | null => {
    for (const slot of [0, 1] as const) {
      const u = slotChain(slot).units.find((x) => x.id === unit);
      if (u) return { u, slot };
    }
    return null;
  };
  createEffect(() => {
    for (const f of openFaces()) {
      const at = find(f.unit);
      if (at?.u.state === 'seated' && !stageOpen()) continue;
      const inside = document.activeElement?.closest(`#${f.kind}-face-${f.unit}`) != null;
      closeFace(f.unit);
      if (inside) queueMicrotask(() => plateOf(f.unit)?.focus('menu'));
    }
  });
  return (
    <Portal>
      <div class="uface-layer">
        <For each={openFaces()}>{(f) => <Face face={f} find={find} />}</For>
      </div>
    </Portal>
  );
}

function Face(props: { face: OpenFace; find: (unit: number) => { u: UnitState; slot: 0 | 1 } | null }) {
  const f = props.face;
  // the unit as the feed last told it (a face closing on its unit's departure draws its last state)
  let last: { u: UnitState; slot: 0 | 1 } | null = null;
  const at = () => (last = props.find(f.unit) ?? last);
  const unit = () => at()!.u;
  const slotLetter = () => (at()?.slot === 1 ? 'B' : 'A');
  const close = (keyboard: boolean) => {
    closeFace(f.unit);
    if (keyboard) plateOf(f.unit)?.focus('name');
  };
  return (
    <Switch>
      <Match when={!!at() && f.kind === 'ott'}>
        <OttFace unit={unit} slotLetter={slotLetter} close={close} />
      </Match>
      <Match when={!!at() && f.kind === 'echo'}>
        <EchoFace unit={unit} slotLetter={slotLetter} close={close} />
      </Match>
    </Switch>
  );
}
