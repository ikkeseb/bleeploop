import { For, onCleanup, onMount } from 'solid-js';
import type { UnitState } from '../../platform';
import { FaceKnob, UnitFace, type KnobDef } from './UnitFace';
import { DB_LINES, SPAN, levelShare, registerReverbGlass, reverbGeometry } from './reverb-loop';
import './reverb-face.css';

/**
 * The REVERB's own face ("the room") on the family's frame (`UnitFace.tsx`: its home under the plate, the
 * move and the resize, the name, the power switch, the close). The glass is the room's tail (`reverb-loop.ts`):
 * the unit's output over the last six seconds, newest at NOW, L above the centre line and R below, so a dry
 * note is a thin spike and a wet one a long soft cloud, read against the dB lines and the seconds ruler.
 * The unit has one param, so the face has one knob: LEVEL, the hero, the wet level, drawn big under the glass
 * (every control sends `SetUnitParam` and shows the feed's value). On the narrow face (900x382) the glass is
 * shorter and the knob smaller, the legends at full size.
 */

const KNOBS: readonly KnobDef[] = [
  { param: 'reverbLevel', label: 'LEVEL', name: 'level', def: 0.5, hero: true, wide: { x: 600, y: 386, size: 172 }, narrow: { x: 520, y: 275, size: 96 } },
];
/** The glass inside its bezel, full and narrow (CSS pixels). */
const GLASS = { w: 852, wide: 232, narrow: 178 };

export function ReverbFace(props: { unit: () => UnitState; slotLetter: () => string; close: (keyboard: boolean) => void }) {
  return (
    <UnitFace unit={props.unit} slotLetter={props.slotLetter} close={props.close} kind="reverb" name="REVERB">
      {(narrow) => (
        <>
          <Glass unit={props.unit().id} narrow={narrow()} />
          <For each={KNOBS}>{(k) => <FaceKnob def={k} narrow={narrow} unit={props.unit} unitName="REVERB" />}</For>
        </>
      )}
    </UnitFace>
  );
}

/** The glass: its canvas (the loop draws the body, the lines and the ruler) and its print: NOW, the seconds
 * back, the dB lines, the channels. */
function Glass(props: { unit: number; narrow: boolean }) {
  let canvas!: HTMLCanvasElement;
  onMount(() => onCleanup(registerReverbGlass(canvas, props.unit)));
  const G = () => reverbGeometry(GLASS.w, props.narrow ? GLASS.narrow : GLASS.wide);
  const X = (ago: number) => G().X1 - (ago / SPAN) * (G().X1 - G().X0);
  const dbTop = (db: number, dir: -1 | 1) => G().YC + dir * levelShare(db) * G().HALF;
  return (
    <div class="uface__bezel">
      <div class="uface__glass">
        <canvas ref={canvas} class="uface__canvas reverb__canvas" aria-hidden="true" />
        <For each={DB_LINES}>
          {(db) => (
            <For each={[-1, 1] as const}>
              {(dir) => (
                <span class="reverb__db" classList={{ 'is-zero': db === 0 }} style={{ top: `${dbTop(db, dir)}px` }} aria-hidden="true">
                  {db === 0 ? '0' : `−${-db}`}
                </span>
              )}
            </For>
          )}
        </For>
        <span class="reverb__ago reverb__now" style={{ left: `${G().X1}px`, top: `${G().print}px` }} aria-hidden="true">
          NOW
        </span>
        <For each={[2, 4, 6]}>
          {(s) => (
            <span class="reverb__ago" style={{ left: `${X(s)}px`, top: `${G().print}px` }} aria-hidden="true">
              −{s}
              <span class="uface__u">s</span>
            </span>
          )}
        </For>
        <span class="reverb__ch" style={{ left: `${G().X1 + 17}px`, top: `${G().YC - G().HALF / 2 - 6}px` }} aria-hidden="true">
          L
        </span>
        <span class="reverb__ch" style={{ left: `${G().X1 + 17}px`, top: `${G().YC + G().HALF / 2 - 6}px` }} aria-hidden="true">
          R
        </span>
      </div>
    </div>
  );
}
