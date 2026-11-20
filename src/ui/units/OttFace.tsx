import { For, onCleanup, onMount } from 'solid-js';
import type { UnitState } from '../../platform';
import type { RowPopover } from '../instrument/RowPopover';
import { FaceKnob, UnitFace, type KnobDef } from './UnitFace';
import { registerOttGlass } from './ott-loop';
import './ott-face.css';

/**
 * The OTT's own face (the accepted `docs/redesign/ott-open.html`) on the family's frame (`UnitFace.tsx`:
 * its home under the plate, the move and the resize, the name, the power switch, the close). The glass on
 * top shows each band's gain over the last three seconds, NOW and its level, from the feed's `unitMeters`
 * (drawn by `ott-loop.ts`, never through a signal); under it the four programmed knobs, DEPTH the hero.
 * On the narrow face (900x382): a shorter glass (5 px a dB), smaller knobs, the same legends at full size.
 */

/** The glass's 0 dB line and pixels per dB, full and narrow (`ott-loop.ts` reads them off the canvas). */
const GLASS = { wide: { y0: 120, px: 7 }, narrow: { y0: 92, px: 5 } };
const KNOBS: Record<'wide' | 'narrow', readonly KnobDef[]> = {
  wide: [
    { param: 'ottDepth', label: 'DEPTH', name: 'depth', def: 0.5, x: 318, y: 386, size: 172, hero: true },
    { param: 'ottTime', label: 'TIME', name: 'time', def: 0.5, x: 528, y: 424, size: 92 },
    { param: 'ottUpward', label: 'UP', name: 'upward', def: 1, x: 690, y: 424, size: 92, tri: 'up' },
    { param: 'ottDownward', label: 'DOWN', name: 'downward', def: 1, x: 812, y: 424, size: 92, tri: 'dn' },
  ],
  narrow: [
    { param: 'ottDepth', label: 'DEPTH', name: 'depth', def: 0.5, x: 300, y: 275, size: 96, hero: true },
    { param: 'ottTime', label: 'TIME', name: 'time', def: 0.5, x: 500, y: 287, size: 60 },
    { param: 'ottUpward', label: 'UP', name: 'upward', def: 1, x: 655, y: 287, size: 60, tri: 'up' },
    { param: 'ottDownward', label: 'DOWN', name: 'downward', def: 1, x: 790, y: 287, size: 60, tri: 'dn' },
  ],
};

export function OttFace(props: {
  pop: RowPopover;
  unit: () => UnitState;
  /** The plate's element: the face hangs from it. */
  plate: () => HTMLElement | undefined;
  slotLetter: string;
}) {
  return (
    <UnitFace pop={props.pop} unit={props.unit} plate={props.plate} slotLetter={props.slotLetter} kind="ott" name="OTT">
      {(narrow) => {
        const g = () => GLASS[narrow() ? 'narrow' : 'wide'];
        return (
          <>
            <Glass unit={props.unit().id} y0={g().y0} px={g().px} />
            <For each={KNOBS[narrow() ? 'narrow' : 'wide']}>{(k) => <FaceKnob def={k} unit={props.unit} unitName="OTT" />}</For>
          </>
        );
      }}
    </UnitFace>
  );
}

/** The glass: its canvas, the printed scale and band names, the three read-outs the loop writes. */
function Glass(props: { unit: number; y0: number; px: number }) {
  const values: HTMLElement[] = [];
  let canvas!: HTMLCanvasElement;
  onMount(() => onCleanup(registerOttGlass(canvas, values, props.unit)));
  // the scale and the bands' places, as the loop draws them (ott-loop.ts: X0 40, the 0 dB line at `y0`,
  // `px` a dB)
  const W = 852;
  const band = (i: number) => 40 + ((W - 6 - 40) / 3) * i;
  const dbs = [12, 6, 0, -6, -12];
  return (
    <div class="uface__bezel">
      <div class="uface__glass">
        <canvas ref={canvas} class="uface__canvas ott__canvas" data-y0={props.y0} data-px={props.px} aria-hidden="true" />
        <For each={dbs}>
          {(d) => (
            <span class="ott__db" classList={{ 'is-zero': d === 0 }} style={{ top: `${props.y0 - d * props.px}px` }} aria-hidden="true">
              {d > 0 ? `+${d}` : d < 0 ? `−${-d}` : '0'}
            </span>
          )}
        </For>
        <For each={['LOW', 'MID', 'HIGH']}>
          {(name, i) => (
            <>
              <span class="ott__band" style={{ left: `${Math.round(band(i())) + 12}px` }} aria-hidden="true">
                {name}
              </span>
              <span
                class="ott__gv"
                style={{ left: `${Math.round(band(i() + 1)) - 12 - 6 - 10}px` }}
                ref={(el) => (values[i()] = el)}
                title={`${name.toLowerCase()} band gain now`}
              >
                <span>0.0</span>
                <span class="uface__u">dB</span>
              </span>
            </>
          )}
        </For>
        <For each={[['88', 'Hz'], ['2.5', 'kHz']] as const}>
          {([v, u], i) => (
            <span class="ott__hz" style={{ left: `${Math.round(band(i() + 1))}px` }} aria-hidden="true">
              {v}
              <span class="uface__u">{u}</span>
            </span>
          )}
        </For>
      </div>
    </div>
  );
}
