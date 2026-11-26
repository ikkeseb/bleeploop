import { For, createEffect, onCleanup, onMount } from 'solid-js';
import type { UnitState } from '../../platform';
import { noteOf } from '../tuner/pitch';
import { FaceKnob, UnitFace, paramOf, type KnobDef, type KnobFormat } from './UnitFace';
import { RING_FREQ_MAX, RING_FREQ_MIN, freqFrac, registerRingGlass, ringGeometry, type RingView } from './ring-loop';
import './ring-face.css';

/**
 * The RING MOD's own face ("the carrier") on the family's frame (`UnitFace.tsx`: its home under the plate,
 * the move and the resize, the name, the power switch, the close). The glass shows the carrier sine at FREQ
 * (log-mapped across the window so 20 Hz reads as a slow wobble and 1.5 kHz as a dense metallic shimmer)
 * with the unit's output (`unitMeters[].out`, L above the zero axis and R below) riding on it, a log
 * frequency scale from 20 Hz to 1.5 kHz with the carrier's place marked, and stereo OUT ladders
 * (`ring-loop.ts`, never through a signal). Under it FREQ, the hero (log 20–1500 Hz, printed in whole Hz with
 * the nearest note at A4 = 440), and LEVEL (percent).
 */

/** The engine's defaults (`InputSendParam::RingFreq`, `RingLevel`). */
const DEF = { freq: 440, level: 0.5 };
/** The glass inside its bezel, full and narrow (CSS pixels). */
const GLASS = { w: 852, wide: 232, narrow: 178 };
/** Printed ticks along the log frequency scale (Hz). */
const SCALE_TICKS: readonly { hz: number; value: string; unit?: string; align?: 'left' | 'right' }[] = [
  { hz: 20, value: '20', unit: 'Hz', align: 'left' },
  { hz: 50, value: '50' },
  { hz: 100, value: '100' },
  { hz: 200, value: '200' },
  { hz: 500, value: '500' },
  { hz: 1000, value: '1', unit: 'kHz' },
  { hz: 1500, value: '1.5', unit: 'kHz', align: 'right' },
];

/** Format a carrier frequency in whole Hz with its nearest 12-TET note (A4 = 440 Hz). */
function formatFreq(hz: number): KnobFormat {
  const n = noteOf(hz);
  const noteName = n ? `${n.name}${n.octave}` : '';
  // whole hertz, as the knob sends them, so the figure and its note always agree (a kHz figure rounded to
  // 10 Hz could print a note the true value is not nearest)
  const value = String(Math.round(hz));
  const unit = 'Hz';
  return {
    value,
    unit,
    note: noteName ? `· ${noteName}` : undefined,
    aria: noteName ? `${value} ${unit}, ${noteName}` : `${value} ${unit}`,
  };
}

const KNOBS: readonly KnobDef[] = [
  {
    param: 'ringFreq',
    label: 'FREQ',
    name: 'frequency',
    def: DEF.freq,
    min: RING_FREQ_MIN,
    max: RING_FREQ_MAX,
    log: true,
    quant: Math.round,
    format: formatFreq,
    wide: { x: 560, y: 386, size: 172 },
    narrow: { x: 520, y: 275, size: 96 },
    hero: true,
  },
  {
    param: 'ringLevel',
    label: 'LEVEL',
    name: 'level',
    def: DEF.level,
    wide: { x: 786, y: 424, size: 92 },
    narrow: { x: 745, y: 287, size: 60 },
  },
];

export function RingFace(props: { unit: () => UnitState; slotLetter: () => string; close: (keyboard: boolean) => void }) {
  const freq = () => paramOf(props.unit(), 'ringFreq', DEF.freq);
  const level = () => paramOf(props.unit(), 'ringLevel', DEF.level);
  // the draw loop's view: written here, read plain on its frames (invariant 6)
  const view: RingView = { freq: freq(), level: level() };
  createEffect(() => {
    view.freq = freq();
    view.level = level();
  });
  return (
    <UnitFace unit={props.unit} slotLetter={props.slotLetter} close={props.close} kind="ring" name="RING MOD">
      {(narrow) => (
        <>
          <Glass unit={props.unit().id} view={view} narrow={narrow()} freq={freq()} />
          <For each={KNOBS}>{(k) => <FaceKnob def={k} narrow={narrow} unit={props.unit} unitName="RING MOD" />}</For>
        </>
      )}
    </UnitFace>
  );
}

/** The glass: its canvas (the loop draws the carrier, ruler and ladders) and its printed labels. */
function Glass(props: { unit: number; view: RingView; narrow: boolean; freq: number }) {
  let canvas!: HTMLCanvasElement;
  onMount(() => onCleanup(registerRingGlass(canvas, props.unit, props.view)));
  const G = () => ringGeometry(GLASS.w, props.narrow ? GLASS.narrow : GLASS.wide);
  const read = () => formatFreq(props.freq);
  const xOf = (hz: number) => Math.round(G().X0 + freqFrac(hz) * (G().X1 - G().X0));
  return (
    <div class="uface__bezel">
      <div class="uface__glass">
        <canvas ref={canvas} class="uface__canvas ring__canvas" aria-hidden="true" />
        <span class="ring__cap" style={{ left: `${G().X0}px`, top: `${G().headTop}px` }} aria-hidden="true">
          CARRIER
        </span>
        <span class="ring__read" style={{ left: `${G().X1}px`, top: `${G().readTop}px` }} title="carrier frequency and nearest note">
          {read().value}
          <span class="uface__u">{read().unit}</span>
          <span class="ring__g-note">{read().note}</span>
        </span>
        <span class="ring__ch" style={{ left: `${G().X0 - 22}px`, top: `${G().lTop}px` }} aria-hidden="true">
          L
        </span>
        <span class="ring__ch" style={{ left: `${G().X0 - 22}px`, top: `${G().rTop}px` }} aria-hidden="true">
          R
        </span>
        <For each={SCALE_TICKS}>
          {(t) => (
            <span
              class="ring__hz"
              classList={{ 'ring__hz--left': t.align === 'left', 'ring__hz--right': t.align === 'right' }}
              style={{ left: `${xOf(t.hz)}px`, top: `${G().print}px` }}
              aria-hidden="true"
            >
              {t.value}
              {t.unit && <span class="uface__u">{t.unit}</span>}
            </span>
          )}
        </For>
        <span class="ring__out" style={{ left: `${G().MX - 1}px`, top: `${G().headTop}px` }} aria-hidden="true">
          OUT
        </span>
        <span class="ring__ch" style={{ left: `${G().MX + 6}px`, top: `${G().print}px` }} aria-hidden="true">
          L
        </span>
        <span class="ring__ch" style={{ left: `${G().MX + 21}px`, top: `${G().print}px` }} aria-hidden="true">
          R
        </span>
      </div>
    </div>
  );
}
