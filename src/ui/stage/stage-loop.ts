import { DPR_CAP, readPalette, resetLight, stepLight, type StageViewDef, type Surface, type ViewDraw } from './visual';

/**
 * OWNS: the stage's one requestAnimationFrame loop and its canvas. Started when the stage view mounts
 * and stopped when it unmounts; the canvas is sized on resize (a ResizeObserver, and the window's
 * resize for a zoom or a monitor change), never per frame, at a device-pixel ratio capped at `DPR_CAP`.
 *
 * Invariant 6: `stageFrame` and everything it reaches read the plain feed (`stage-feed.ts`) and the
 * looper's non-reactive getters only, and allocate nothing in the steady state. Solid lives in
 * `StageView.tsx`, which writes the feed from effects. `verify/guards/stage-draw.mjs` holds the line.
 */

export interface StageHandle {
  /** Switch the look; the loop and the canvas stay. */
  setView(def: StageViewDef): void;
  /** The lane under a pointer (CSS px in the canvas), or -1. */
  hit(x: number, y: number): number;
  stop(): void;
}

interface Run {
  canvas: HTMLCanvasElement;
  /** The stage root: the count-in numeral's anchor is written onto it as CSS variables. */
  host: HTMLElement;
  surface: Surface;
  def: StageViewDef;
  view: ViewDraw;
}

let run: Run | null = null;
let rafId = 0;

function stageFrame(now: number): void {
  rafId = requestAnimationFrame(stageFrame);
  if (run === null) return;
  stepLight(now);
  run.view.draw(run.surface, now);
}

/** Lay the view out on the surface and hand the numeral's anchor to the DOM. */
function place(r: Run): void {
  r.view.layout(r.surface);
  const { countX, countY, countSize } = r.surface;
  r.host.style.setProperty('--sv-count-x', `${countX.toFixed(1)}px`);
  r.host.style.setProperty('--sv-count-y', `${countY.toFixed(1)}px`);
  r.host.style.setProperty('--sv-count-size', `${countSize.toFixed(1)}px`);
}

/** Size the backing store to the element and the capped device-pixel ratio, when either moved. */
function measure(): void {
  const r = run;
  if (!r) return;
  const s = r.surface;
  const w = r.canvas.clientWidth;
  const h = r.canvas.clientHeight;
  const dpr = Math.min(self.devicePixelRatio || 1, DPR_CAP);
  if (w === s.w && h === s.h && dpr === s.dpr) return;
  s.w = w;
  s.h = h;
  s.dpr = dpr;
  s.dw = Math.max(1, Math.round(w * dpr));
  s.dh = Math.max(1, Math.round(h * dpr));
  r.canvas.width = s.dw;
  r.canvas.height = s.dh;
  place(r);
}

/** Start drawing `def` on `canvas`. One stage at a time: a second start replaces the first. */
export function startStage(canvas: HTMLCanvasElement, host: HTMLElement, def: StageViewDef): StageHandle | null {
  const ctx = canvas.getContext('2d', { alpha: false });
  if (!ctx) {
    console.warn('[stage] 2D context unavailable; the stage view draws nothing');
    return null;
  }
  run?.view.dispose();
  const surface: Surface = { ctx, pal: readPalette(canvas), w: -1, h: -1, dw: 1, dh: 1, dpr: 0, countX: 0, countY: 0, countSize: 0 };
  const mine: Run = { canvas, host, surface, def, view: def.create() };
  run = mine;
  const observer = new ResizeObserver(measure);
  observer.observe(canvas);
  window.addEventListener('resize', measure);
  measure();
  resetLight();
  // The lane numbers are text in a cached bitmap: lay out again once the fonts are in.
  void document.fonts.ready.then(() => {
    if (run === mine) place(mine);
  });
  if (rafId === 0) rafId = requestAnimationFrame(stageFrame);
  return {
    setView(next) {
      if (run !== mine || next === mine.def) return;
      mine.view.dispose();
      mine.def = next;
      mine.view = next.create();
      place(mine);
    },
    hit: (x, y) => (run === mine ? mine.view.hit(x, y) : -1),
    stop() {
      observer.disconnect();
      window.removeEventListener('resize', measure);
      mine.view.dispose();
      if (run !== mine) return;
      run = null;
      cancelAnimationFrame(rafId);
      rafId = 0;
    },
  };
}
