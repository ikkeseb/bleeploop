import { barSeconds, beatSeconds, repeatGain, stepSeconds, type EchoTempo } from './echo-time';
import { OUT_HISTORY, unitMeters, type UnitMeterView } from './unit-meters';

/**
 * The ECHO face's draw loop: its glass is the delay line drawn as tape (the accepted face B). What came out
 * of the unit runs right to left from NOW (the write head): the feed's `unitMeters[].out`, its peak per
 * channel per frame, L above the centre line and R below, over the last two bars. The read heads stand at
 * 1, 2, 3 ... times TIME behind NOW, each lit as loud as its repeat; the ruler counts the beats back from
 * NOW; OUT is the output's two ladders. The heads, the ruler and the span read the face's params and the
 * tempo (a plain `EchoView` the face's Solid side writes); only the trace and OUT read the meters.
 *
 * The trace's columns are tied to fixed slices of time (2 px columns on a 3 px pitch, each the peak over
 * its slice): as the tape runs each column slides left whole and is never recomputed from other frames, so
 * it never shimmers. A frame covers the time since the frame before it (its arrival stamped in the mirror),
 * and the tape is drawn `LAG` behind the clock, so the newest slice is always one a frame has covered.
 *
 * Invariant 6: it reads no signal and writes none (`verify/guards/stage-draw.mjs` walks it); Solid only
 * registers and unregisters the glass and writes its view. A glass is drawn again only while its tape holds
 * something lit, or when its meters, its view or its size moved: a silent ECHO costs nothing.
 */

/** What the face's Solid side tells the loop: TIME (the face's step), FEEDBACK, LEVEL, the tempo. */
export interface EchoView {
  step: number;
  feedback: number;
  level: number;
  tempo: EchoTempo;
}

interface Glass {
  canvas: HTMLCanvasElement;
  unit: number;
  view: EchoView;
  drawn: number;
  /** The view, size and scale the glass was last drawn at. */
  laid: string;
  /** The last draw lit something (a column or a ladder cell): the tape still runs. */
  lit: boolean;
}

const glasses = new Set<Glass>();
let rafId = 0;

/** The tape runs this far behind the clock (s): two feed frames, so the newest slice has its frame. */
const LAG = 0.034;
/** The longest time one frame covers (s): after a gap (a unit held out), a frame paints no more. */
const MAX_COVER = 0.05;
/** The trace's floor (dBFS) at the centre line; 0 dBFS reaches the tape's edge. */
const FLOOR = -54;
/** Bars of tape. */
const BARS = 2;

const WARM: readonly number[] = [255, 243, 224];
const MET = { g: [79, 217, 143], y: [255, 176, 46], r: [255, 74, 90] } as const;
const MET_OFF = { g: '#1b2620', y: '#2e2716', r: '#4a2026' } as const;
const rgba = (c: readonly number[], a: number): string => `rgba(${c[0]},${c[1]},${c[2]},${a})`;

/** The glass's geometry in CSS pixels, full (852x232) or narrow (852x178): the tape from `X0` to NOW at
 * `X1`, OUT at `MX`, the centre line `YC` with 0 dBFS `HALF` from it. The face's print uses it too. */
export function echoGeometry(W: number, H: number) {
  const narrow = H < 200;
  const MX = W - 46;
  const X1 = MX - 22;
  const YC = narrow ? 86 : 118;
  const HALF = narrow ? 50 : 74;
  return {
    X0: 40,
    X1,
    MX,
    YC,
    HALF,
    /** The heads' notches, and the TIME bracket from NOW to the first head. */
    notch: YC - HALF - 14,
    bracket: YC - HALF - 12,
    /** The ruler's line under the tape, and the print under it. */
    rule: YC + HALF + (narrow ? 8 : 10),
    print: H - (narrow ? 20 : 22),
    /** The span's read-out and OUT's caption over the tape. */
    spanTop: narrow ? 7 : 14,
    outTop: narrow ? 9 : 16,
    /** The channels' letters left of the tape. */
    lTop: YC - (narrow ? 32 : 46),
    rTop: YC + (narrow ? 20 : 38),
  };
}

/** Draw an ECHO face's glass for `unit` while it is mounted; `view` is read on every frame. */
export function registerEchoGlass(canvas: HTMLCanvasElement, unit: number, view: EchoView): () => void {
  const g: Glass = { canvas, unit, view, drawn: -1, laid: '', lit: false };
  glasses.add(g);
  if (rafId === 0) rafId = requestAnimationFrame(frame);
  return () => glasses.delete(g);
}

function frame(): void {
  rafId = 0;
  if (glasses.size === 0) return;
  const now = performance.now() / 1000;
  for (const g of glasses) {
    const v = unitMeters.get(g.unit);
    const version = v ? v.version : 0;
    const w = g.view;
    const laid = `${g.canvas.clientWidth}x${g.canvas.clientHeight}:${w.step}:${w.feedback}:${w.level}:${w.tempo.bpm}:${w.tempo.num}/${w.tempo.den}`;
    // silent and still: nothing to draw again (a silent frame moves the version and lights nothing)
    const newest = v && v.outCount > 0 ? (v.outCount - 1) % OUT_HISTORY : -1;
    const loud = v !== undefined && newest >= 0 && (v.outL[newest] > 0 || v.outR[newest] > 0);
    if (!g.lit && laid === g.laid && (version === g.drawn || !loud)) continue;
    const lit = drawGlass(g, v, now - LAG);
    // a glass not laid out yet draws on a later frame
    if (lit === null) continue;
    g.lit = lit;
    g.drawn = version;
    g.laid = laid;
  }
  rafId = requestAnimationFrame(frame);
}

/** A peak (linear) as a height from the centre line: FLOOR at 0, 0 dBFS at `half`. */
const height = (a: number, half: number): number => {
  const db = 20 * Math.log10(Math.max(1e-6, a));
  return Math.max(0, Math.min(1, (db - FLOOR) / -FLOOR)) * half;
};

/** The scratch slices the trace fills, per channel. */
let sliceL = new Float32Array(0);
let sliceR = new Float32Array(0);

/** Draw `gl` at tape time `t` (s); whether it lit anything, or null when it is not laid out yet. */
function drawGlass(gl: Glass, v: UnitMeterView | undefined, t: number): boolean | null {
  const cv = gl.canvas;
  const W = cv.clientWidth;
  const H = cv.clientHeight;
  if (W === 0 || H === 0) return null;
  // the face's own scale (its window's size step, the player's size) sharpens the backing too
  const dpr = (window.devicePixelRatio || 1) * Math.max(1, cv.getBoundingClientRect().width / W);
  const bw = Math.round(W * dpr);
  const bh = Math.round(H * dpr);
  if (cv.width !== bw || cv.height !== bh) {
    cv.width = bw;
    cv.height = bh;
  }
  const g = cv.getContext('2d');
  if (!g) return null;
  g.setTransform(dpr, 0, 0, dpr, 0, 0);
  g.clearRect(0, 0, W, H);
  const G = echoGeometry(W, H);
  const { X0, X1, MX, YC, HALF } = G;
  const view = gl.view;
  const beat = beatSeconds(view.tempo);
  const span = BARS * barSeconds(view.tempo);
  const T = stepSeconds(view.tempo, view.step);
  const pxs = (X1 - X0) / span;
  const X = (ago: number): number => X1 - ago * pxs;

  // the ruler: a tick per beat behind NOW, a bar's longer and brighter; the centre line between L and R
  const beats = Math.round(span / beat);
  for (let b = 0; b <= beats; b++) {
    const bar = b % view.tempo.beats === 0;
    g.fillStyle = bar ? 'rgba(255,255,255,.24)' : 'rgba(255,255,255,.13)';
    g.fillRect(Math.round(X(b * beat)), G.rule, 1, bar ? 7 : 4);
  }
  g.fillStyle = 'rgba(255,255,255,.08)';
  g.fillRect(X0, G.rule, X1 - X0, 1);
  g.fillStyle = 'rgba(255,255,255,.07)';
  g.fillRect(X0, YC, X1 - X0, 1);

  // the read heads: a fine line across the tape and a notch over it, each as bright as its repeat
  const heads = Math.floor(span / T + 1e-6);
  for (let k = 1; k <= heads; k++) {
    const x = Math.round(X(k * T)) + 0.5;
    const db = 20 * Math.log10(Math.max(1e-9, repeatGain(view.level, view.feedback, k)));
    const f = Math.max(0, 1 + db / 60);
    g.fillStyle = rgba(WARM, 0.04 + 0.16 * f * f);
    g.fillRect(x - 0.5, YC - HALF - 6, 1, 2 * HALF + 12);
    g.fillStyle = rgba(WARM, 0.08 + 0.92 * f * f);
    g.beginPath();
    g.moveTo(x - 4, G.notch - 3);
    g.lineTo(x + 4, G.notch - 3);
    g.lineTo(x, G.notch + 3);
    g.closePath();
    g.fill();
  }
  // the write head at NOW, and TIME measured from it to the first head
  g.fillStyle = rgba(WARM, 0.6);
  g.fillRect(X1, YC - HALF - 6, 1, 2 * HALF + 12);
  const xa = Math.round(X(T)) + 0.5;
  const xb = X1 + 0.5;
  const yb = G.bracket + 0.5;
  g.strokeStyle = rgba(WARM, 0.55);
  g.lineWidth = 1;
  g.beginPath();
  g.moveTo(xa, yb - 4);
  g.lineTo(xa, yb + 4);
  g.moveTo(xb, yb - 4);
  g.lineTo(xb, yb + 4);
  g.moveTo(xa, yb);
  g.lineTo(xb, yb);
  g.stroke();

  let lit = false;
  // the trace: each slice's peak, from the frames that cover it
  const sliceS = 3 / pxs;
  const b1 = Math.floor(t / sliceS);
  const n = Math.ceil((X1 - X0) / 3) + 2;
  const b0 = b1 - n + 1;
  if (sliceL.length < n) {
    sliceL = new Float32Array(n);
    sliceR = new Float32Array(n);
  }
  sliceL.fill(0, 0, n);
  sliceR.fill(0, 0, n);
  const count = v ? Math.min(v.outCount, OUT_HISTORY) : 0;
  if (v && count > 0) {
    const tStart = b0 * sliceS;
    for (let s = 0; s < count; s++) {
      const i = (v.outCount - 1 - s) % OUT_HISTORY;
      const a1 = v.outAt[i] / 1000;
      if (a1 < tStart) break;
      const before = s + 1 < count ? v.outAt[(v.outCount - 2 - s) % OUT_HISTORY] / 1000 : a1 - 1 / 60;
      const a0 = Math.max(before, a1 - MAX_COVER);
      const L = v.outL[i];
      const R = v.outR[i];
      if (L <= 0 && R <= 0) continue;
      const first = Math.max(b0, Math.floor(a0 / sliceS));
      const last = Math.min(b1, Math.floor(a1 / sliceS));
      for (let b = first; b <= last; b++) {
        const j = b - b0;
        if (L > sliceL[j]) sliceL[j] = L;
        if (R > sliceR[j]) sliceR[j] = R;
      }
    }
  }
  g.save();
  g.beginPath();
  g.rect(X0, 0, X1 - X0, H);
  g.clip();
  for (let j = n - 1; j >= 0; j--) {
    const b = b0 + j;
    const x = X1 - (t - b * sliceS) * pxs - 2;
    if (x < X0 - 3) break;
    const u = Math.max(0, (x - X0) / (X1 - X0));
    const f = 0.28 + 0.72 * Math.pow(u, 1.1);
    for (const [p, dir] of [
      [sliceL[j], -1],
      [sliceR[j], 1],
    ] as const) {
      const h = height(p, HALF);
      // under a pixel and a half a column is a speck on the centre line, not a sound
      if (h < 1.5) continue;
      lit = true;
      const ye = YC + dir * h;
      const gr = g.createLinearGradient(0, YC, 0, ye);
      gr.addColorStop(0, rgba(WARM, 0.08 * f));
      gr.addColorStop(0.6, rgba(WARM, 0.42 * f));
      gr.addColorStop(1, rgba(WARM, 0.9 * f));
      g.fillStyle = gr;
      if (dir < 0) g.fillRect(x, ye, 2, h);
      else g.fillRect(x, YC + 1, 2, h);
    }
  }
  g.restore();

  // OUT: two ladders, FLOOR..0 dBFS over the tape's height; green, amber from -12, red from -3; the last
  // second's peak held
  const top = YC - HALF;
  const bot = YC + HALF;
  const nc = Math.floor((bot - top) / 4);
  const dbc = -FLOOR / nc;
  for (let ch = 0; ch < 2; ch++) {
    let now = 0;
    let pk = 0;
    if (v && count > 0) {
      const newest = v.outAt[(v.outCount - 1) % OUT_HISTORY] / 1000;
      for (let s = 0; s < count; s++) {
        const i = (v.outCount - 1 - s) % OUT_HISTORY;
        const age = newest - v.outAt[i] / 1000;
        if (age > 1) break;
        const a = ch === 0 ? v.outL[i] : v.outR[i];
        if (age <= 0.05 && a > now) now = a;
        if (a > pk) pk = a;
      }
      // a meter stops reading what the tape has already carried away
      if (t - newest > 0.25) now = pk = 0;
    }
    const c0 = (20 * Math.log10(Math.max(1e-6, now)) - FLOOR) / dbc;
    const cp = Math.floor((20 * Math.log10(Math.max(1e-6, pk)) - FLOOR) / dbc);
    const x = MX + 3 + ch * 15;
    for (let k = 0; k < nc; k++) {
      const y = bot - (k + 1) * 4 + 1;
      const db = FLOOR + k * dbc;
      const zone = db >= -3 ? 'r' : db >= -12 ? 'y' : 'g';
      const f = Math.max(0, Math.min(1, c0 - k));
      g.fillStyle = MET_OFF[zone];
      g.fillRect(x, y, 6, 3);
      if (f > 0.03) {
        g.fillStyle = rgba(MET[zone], 0.3 + 0.7 * f);
        g.fillRect(x, y, 6, 3);
        lit = true;
      } else if (k === cp && pk > 0) {
        g.fillStyle = rgba(MET[zone], 0.8);
        g.fillRect(x, y, 6, 3);
        lit = true;
      }
    }
  }
  return lit;
}
