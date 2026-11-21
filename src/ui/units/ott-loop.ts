import { HISTORY, LEVEL_FLOOR, unitMeters, type UnitMeterView } from './unit-meters';

/**
 * The OTT's draw loop: one requestAnimationFrame loop for every OTT face's glass and every OTT plate's
 * miniature on screen, drawn from the units' plain meter mirror (`unit-meters.ts`, written by the feed).
 * Invariant 6: it reads no signal and writes none (`verify/guards/stage-draw.mjs` walks it); Solid only
 * registers and unregisters the elements. A glass or a miniature is drawn again only when its unit's
 * meters moved, so a bypassed or silent OTT costs nothing.
 *
 * The glass is the accepted face's (`docs/redesign/ott.js`, `glass`): per band the last three seconds of
 * its gain as a lit body between the 0 dB line and the gain (warm white lifting, amber cutting), fading
 * into the past; NOW as a ladder of 1 dB cells centred on the 0 line; the band's level as a neutral
 * meter at its right with the last second's peak held. 7 px per dB, ±12 dB.
 */

interface Glass {
  canvas: HTMLCanvasElement;
  /** The three bands' gain read-outs (text written here, never a signal). */
  values: HTMLElement[];
  unit: number;
  drawn: number;
  /** The size and scale the glass was last drawn at: a reflow of the face draws it again. */
  laid: string;
}
interface Mini {
  el: HTMLElement;
  unit: number;
  drawn: number;
}

const glasses = new Set<Glass>();
const minis = new Set<Mini>();
let rafId = 0;

/** The glass's geometry in CSS pixels (the face draws it 852x232 inside its bezel, 852x178 in its narrow
 * reflow): the scale's left edge; the 0 dB line and the pixels per dB come from the canvas
 * (`data-y0`, `data-px`, set by the face's layout). */
const X0 = 40;
const LAD = 58;
const MW = 6;
const PAD = 12;
const GAP = 10;
/** Frames in the held level peak: the last second. */
const PEAK_FRAMES = 60;

const LIFT: readonly number[] = [255, 243, 224];
const CUT: readonly number[] = [255, 176, 46];
const NEUT: readonly number[] = [214, 219, 225];
const UNLIT = '#121417';
const rgba = (c: readonly number[], a: number): string => `rgba(${c[0]},${c[1]},${c[2]},${a})`;

/** Draw an OTT face's glass for `unit` while it is mounted. */
export function registerOttGlass(canvas: HTMLCanvasElement, values: HTMLElement[], unit: number): () => void {
  const g: Glass = { canvas, values, unit, drawn: -1, laid: '' };
  glasses.add(g);
  start();
  return () => {
    glasses.delete(g);
    stop();
  };
}

/** Draw an OTT plate's miniature (three band bars, `--g0`..`--g2` in dB) for `unit` while it is mounted. */
export function registerOttMini(el: HTMLElement, unit: number): () => void {
  const m: Mini = { el, unit, drawn: -1 };
  minis.add(m);
  start();
  return () => {
    minis.delete(m);
    stop();
  };
}

/** Whether the faces are hidden (the stage view): no frame is queued while they are. */
let suspended = false;

/** Stop the draws (the glasses' and the plates' miniatures) while the stage view hides the faces, and draw
 * again on `false`; what is drawn stays. */
export function suspendOttDraw(on: boolean): void {
  suspended = on;
  if (on) {
    if (rafId !== 0) cancelAnimationFrame(rafId);
    rafId = 0;
  } else if (glasses.size + minis.size > 0) start();
}

function start(): void {
  if (rafId === 0 && !suspended) rafId = requestAnimationFrame(frame);
}

function stop(): void {
  if (glasses.size === 0 && minis.size === 0 && rafId !== 0) {
    cancelAnimationFrame(rafId);
    rafId = 0;
  }
}

function frame(): void {
  rafId = 0;
  if ((glasses.size === 0 && minis.size === 0) || suspended) return;
  for (const g of glasses) {
    const v = unitMeters.get(g.unit);
    const version = v ? v.version : 0;
    // its size as drawn too: the face's own scale moves it, not its layout size
    const laid = `${g.canvas.clientWidth}x${g.canvas.clientHeight}@${Math.round(g.canvas.getBoundingClientRect().width)}:${g.canvas.dataset.y0}:${g.canvas.dataset.px}`;
    // a glass not laid out yet draws on a later frame
    if ((version !== g.drawn || laid !== g.laid) && drawGlass(g, v)) {
      g.drawn = version;
      g.laid = laid;
    }
  }
  for (const m of minis) {
    const v = unitMeters.get(m.unit);
    const version = v ? v.version : 0;
    if (version === m.drawn) continue;
    m.drawn = version;
    for (let b = 0; b < 3; b++) m.el.style.setProperty(`--g${b}`, String(v?.bands ? clamp(v.gain[b], -12, 12) : 0));
  }
  rafId = requestAnimationFrame(frame);
}

const clamp = (x: number, lo: number, hi: number): number => Math.min(hi, Math.max(lo, x));

function drawGlass(gl: Glass, v: UnitMeterView | undefined): boolean {
  const cv = gl.canvas;
  const W = cv.clientWidth;
  const H = cv.clientHeight;
  if (W === 0 || H === 0) return false;
  // the face's own scale (its window's size step, or a fit) sharpens the backing too
  const dpr = (window.devicePixelRatio || 1) * Math.max(1, cv.getBoundingClientRect().width / W);
  const bw = Math.round(W * dpr);
  const bh = Math.round(H * dpr);
  if (cv.width !== bw || cv.height !== bh) {
    cv.width = bw;
    cv.height = bh;
  }
  const g = cv.getContext('2d');
  if (!g) return false;
  const Y0 = Number(cv.dataset.y0) || 120;
  const PX = Number(cv.dataset.px) || 7;
  g.setTransform(dpr, 0, 0, dpr, 0, 0);
  g.clearRect(0, 0, W, H);
  const X1 = W - 6;
  const BW = (X1 - X0) / 3;
  const Y = (db: number): number => Y0 - db * PX;
  const bx = (i: number): number => X0 + BW * i;
  const geo = (i: number) => {
    const r = Math.round(bx(i + 1)) - PAD;
    const mx = r - MW;
    const lx = mx - GAP - LAD;
    return { tx0: Math.round(bx(i)) + PAD, tx1: lx - 4, lx, mx };
  };

  // the grid: ±6 and ±12 as fine lines, the engraved 0 dB line, the band dividers, one-second ticks
  for (const d of [12, 6, -6, -12]) {
    g.fillStyle = Math.abs(d) === 12 ? 'rgba(255,255,255,.06)' : 'rgba(255,255,255,.04)';
    g.fillRect(X0 - 4, Y(d), X1 - X0 + 4, 1);
  }
  g.fillStyle = '#000';
  g.fillRect(X0 - 4, Y0 + 1, X1 - X0 + 4, 1);
  g.fillStyle = 'rgba(255,255,255,.2)';
  g.fillRect(X0 - 4, Y0, X1 - X0 + 4, 1);
  g.fillStyle = 'rgba(255,255,255,.09)';
  for (let i = 1; i < 3; i++) g.fillRect(Math.round(bx(i)), 8, 1, H - 32);
  g.fillStyle = 'rgba(255,255,255,.16)';
  for (let i = 0; i < 3; i++) {
    const { tx0, tx1 } = geo(i);
    for (let s = 0; s <= 3; s++) g.fillRect(Math.round(tx1 - ((tx1 - tx0) * s) / 3), Y(-12) + 1, 1, 4);
  }

  const n = v ? Math.min(v.count, HISTORY) : 0;
  for (let i = 0; i < 3; i++) {
    const { tx0, tx1, lx, mx } = geo(i);
    const w = tx1 - tx0;
    // the history: the frame each pixel column covers, newest at the right; columns before the first
    // frame stay dark
    if (v && n > 0) {
      const newest = v.count - 1;
      const at = (x: number): number => {
        const back = Math.floor(((w - 1 - x) / w) * HISTORY);
        return back < n ? (newest - back) % HISTORY : -1;
      };
      const fade = (x: number): number => {
        const u = x / (w - 1);
        return Math.min(1, u * 6) * (0.16 + 0.84 * Math.pow(u, 1.25));
      };
      // the body: 2 px columns on a 3 px pitch, from the 0 line (dim) to the gain (bright)
      for (let x = 0; x + 2 <= w; x += 3) {
        const k = at(x + 1);
        if (k < 0) continue;
        const hi = v.gainMax[i][k];
        const lo = v.gainMin[i][k];
        const f = fade(x + 1);
        if (hi > 0.05) {
          const ye = Y(Math.min(hi, 12));
          const gr = g.createLinearGradient(0, Y0, 0, ye);
          gr.addColorStop(0, rgba(LIFT, 0.1 * f));
          gr.addColorStop(0.55, rgba(LIFT, 0.42 * f));
          gr.addColorStop(1, rgba(LIFT, 0.85 * f));
          g.fillStyle = gr;
          g.fillRect(tx0 + x, ye, 2, Y0 - ye);
        }
        if (lo < -0.05) {
          const ye = Y(Math.max(lo, -12));
          const gr = g.createLinearGradient(0, Y0, 0, ye);
          gr.addColorStop(0, rgba(CUT, 0.1 * f));
          gr.addColorStop(0.55, rgba(CUT, 0.42 * f));
          gr.addColorStop(1, rgba(CUT, 0.85 * f));
          g.fillStyle = gr;
          g.fillRect(tx0 + x, Y0 + 1, 2, ye - Y0 - 1);
        }
      }
      // the edge: per pixel column, the gain's range lit in full, at least 1.5 px
      for (let x = 0; x < w; x++) {
        const k = at(x);
        if (k < 0) continue;
        const f = fade(x);
        const lo = v.gainMin[i][k];
        const hi = v.gainMax[i][k];
        for (const [c, a, b] of [
          [LIFT, Math.max(0, lo), hi],
          [CUT, Math.min(0, hi), lo],
        ] as const) {
          if (c === LIFT ? b <= 0.05 : b >= -0.05) continue;
          let y0 = Math.min(Y(clamp(a, -12, 12)), Y(clamp(b, -12, 12)));
          let y1 = Math.max(Y(clamp(a, -12, 12)), Y(clamp(b, -12, 12)));
          if (y1 - y0 < 1.5) {
            const m = c === LIFT ? Y(clamp(b, -12, 12)) + 0.75 : Y(clamp(b, -12, 12)) - 0.75;
            y0 = m - 0.75;
            y1 = m + 0.75;
          }
          g.fillStyle = rgba(c, 0.95 * f);
          g.fillRect(tx0 + x, y0, 1, y1 - y0);
        }
      }
    }

    // NOW: 12 cells of 1 dB each way from the 0 line; the cell the value ends in lit by its fraction
    const gv = v?.bands ? v.gain[i] : 0;
    const up = gv >= 0;
    const col = up ? LIFT : CUT;
    const mag = Math.abs(gv);
    for (let k = 1; k <= 12; k++) {
      const yu = Y0 - k * PX + 1;
      const yd = Y0 + (k - 1) * PX + 2;
      g.fillStyle = UNLIT;
      g.fillRect(lx, yu, LAD, PX - 2);
      g.fillRect(lx, yd, LAD, PX - 2);
      const f = clamp(mag - (k - 1), 0, 1);
      if (f > 0.03) {
        g.fillStyle = rgba(col, 0.1 + 0.9 * f);
        g.fillRect(lx, up ? yu : yd, LAD, PX - 2);
      }
    }

    // the band's level: a neutral meter, -60..0 dBFS over the scale's height, the last second's peak held
    const top = Y(12) + 1;
    const bot = Y(-12);
    const nc = Math.floor((bot - top) / 4);
    const dbc = 60 / nc;
    const cells = (db: number): number => (db + 60) / dbc;
    let pk = LEVEL_FLOOR;
    if (v && n > 0) for (let s = 0; s < Math.min(n, PEAK_FRAMES); s++) pk = Math.max(pk, v.levelPeak[i][(v.count - 1 - s) % HISTORY]);
    const c0 = cells(v?.bands ? v.level[i] : LEVEL_FLOOR);
    const cp = Math.floor(cells(pk));
    for (let k = 0; k < nc; k++) {
      const y = bot - (k + 1) * 4 + 1;
      const f = clamp(c0 - k, 0, 1);
      g.fillStyle = UNLIT;
      g.fillRect(mx, y, MW, 3);
      if (f > 0.03) {
        g.fillStyle = rgba(NEUT, 0.1 + 0.56 * f);
        g.fillRect(mx, y, MW, 3);
      } else if (k === cp) {
        g.fillStyle = rgba(NEUT, 0.6);
        g.fillRect(mx, y, MW, 3);
      }
    }

    // the read-out: the band's gain now, signed, in its colour
    const el = gl.values[i];
    if (el) {
      const shown = Math.abs(gv) < 0.05 ? 0 : gv;
      const text = `${shown > 0 ? '+' : shown < 0 ? '−' : ''}${Math.abs(shown).toFixed(1)}`;
      const num = el.firstChild;
      if (num && num.textContent !== text) num.textContent = text;
      el.dataset.dir = shown > 0 ? 'up' : shown < 0 ? 'dn' : '';
    }
  }
  return true;
}
