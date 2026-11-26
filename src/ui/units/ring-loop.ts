import { OUT_HISTORY, unitMeters, type UnitMeterView } from './unit-meters';

/**
 * The RING MOD face's draw loop ("the carrier"): its glass shows the carrier oscillator as a steady,
 * phase-locked sine across the window, its drawn cycle count log-mapped over 20 Hz to 1.5 kHz so 20 Hz
 * reads as a slow wobble and 1.5 kHz as a dense metallic shimmer. The unit's output (`unitMeters[].out`,
 * per channel: L on the upper half, R on the lower) rides on the carrier as its amplitude and lit body:
 * silent, the carrier lies dim and flat on the zero axis; a note's attack lifts and lights it and it fades
 * with the note. Along the glass's foot runs a log frequency ruler (20 Hz to 1.5 kHz) with the carrier's
 * place marked, and at the right OUT's two stereo LED ladders with 1-second peak hold.
 *
 * Invariant 6: reads no Solid signal and writes none; Solid only registers/unregisters the glass and writes
 * the plain `RingView`. Redraws only on a new meter `version` or when the view or backing size moved, and
 * stays idle on silence without flicker.
 */

/** Carrier frequency bounds (Hz), matching `InputSendParam::RingFreq`. */
export const RING_FREQ_MIN = 20;
export const RING_FREQ_MAX = 1500;

/** What the face's Solid side tells the loop: FREQ (Hz) and LEVEL (0..1). */
export interface RingView {
  freq: number;
  level: number;
}

interface Glass {
  canvas: HTMLCanvasElement;
  unit: number;
  view: RingView;
  drawn: number;
  /** The mirror entry last drawn from (a new entry after a reset redraws). */
  src: UnitMeterView | undefined;
  /** The view, size and backing scale last drawn at. */
  laid: string;
  /** The last draw settled at silence (flat dim carrier, dark ladders). */
  resting: boolean;
}

const glasses = new Set<Glass>();
let rafId = 0;

/** Frames in OUT's held peak: one second at the feed's 60 frames a second. */
const PEAK_FRAMES = 60;
/** Output floor (dBFS) at rest; 0 dBFS reaches the carrier's full half-height. */
const FLOOR = -54;
/** Drawn carrier cycles across the glass: 20 Hz (slow wobble) to 1500 Hz (dense shimmer). */
const CYCLES_MIN = 1.5;
const CYCLES_MAX = 42;
/** Log frequency scale major and minor ticks (Hz). */
const MAJOR_HZ = [20, 50, 100, 200, 500, 1000, 1500] as const;
const MINOR_HZ = [30, 40, 60, 70, 80, 90, 300, 400, 600, 700, 800, 900, 1200] as const;
const A4_HZ = 440;

const WARM: readonly number[] = [255, 243, 224];
const MET = { g: [79, 217, 143], y: [255, 176, 46], r: [255, 74, 90] } as const;
const MET_OFF = { g: '#1b2620', y: '#2e2716', r: '#4a2026' } as const;
const rgba = (c: readonly number[], a: number): string => `rgba(${c[0]},${c[1]},${c[2]},${a})`;
const clamp = (x: number, lo: number, hi: number): number => Math.min(hi, Math.max(lo, x));

/** Normalized log position (0..1) of `hz` over 20 Hz to 1.5 kHz. */
export function freqFrac(hz: number): number {
  const f = clamp(hz, RING_FREQ_MIN, RING_FREQ_MAX);
  return Math.log(f / RING_FREQ_MIN) / Math.log(RING_FREQ_MAX / RING_FREQ_MIN);
}

/** The glass's geometry in CSS pixels, full (852x232) or narrow (852x178): the carrier field from `X0` to
 * `X1`, OUT at `MX`, the zero axis `YC` with 0 dBFS `HALF` from it, the log frequency ruler at `rule`.
 * `RingFace.tsx` reads it for its printed labels too. */
export function ringGeometry(W: number, H: number) {
  const narrow = H < 200;
  const MX = W - 46;
  const X1 = MX - 22;
  const YC = narrow ? 82 : 108;
  const HALF = narrow ? 46 : 66;
  return {
    X0: 40,
    X1,
    MX,
    YC,
    HALF,
    /** The log frequency ruler's line under the carrier, and the printed ticks under it. */
    rule: narrow ? 140 : 192,
    print: H - (narrow ? 20 : 22),
    /** Header captions over the glass. */
    headTop: narrow ? 8 : 12,
    readTop: narrow ? 6 : 9,
    /** The channels' letters left of the carrier. */
    lTop: YC - (narrow ? 28 : 40),
    rTop: YC + (narrow ? 20 : 32),
  };
}

/** Draw a RING MOD face's glass for `unit` while it is mounted; `view` is read on every frame. */
export function registerRingGlass(canvas: HTMLCanvasElement, unit: number, view: RingView): () => void {
  const g: Glass = { canvas, unit, view, drawn: -1, src: undefined, laid: '', resting: false };
  glasses.add(g);
  if (rafId === 0 && !suspended) rafId = requestAnimationFrame(frame);
  return () => {
    glasses.delete(g);
    if (glasses.size === 0 && rafId !== 0) {
      cancelAnimationFrame(rafId);
      rafId = 0;
    }
  };
}

/** Whether the glasses are hidden (the stage view): no frame is queued while they are. */
let suspended = false;

/** Stop the glasses' draws while they are hidden, and draw them again on `false`; what is drawn stays. */
export function suspendRingDraw(on: boolean): void {
  suspended = on;
  if (on) {
    if (rafId !== 0) cancelAnimationFrame(rafId);
    rafId = 0;
  } else if (glasses.size > 0 && rafId === 0) rafId = requestAnimationFrame(frame);
}

function frame(): void {
  rafId = 0;
  if (glasses.size === 0 || suspended) return;
  for (const g of glasses) {
    const v = unitMeters.get(g.unit);
    const version = v ? v.version : 0;
    const w = g.view;
    const shown = Math.round(g.canvas.getBoundingClientRect().width);
    const laid = `${g.canvas.clientWidth}x${g.canvas.clientHeight}@${shown}x${window.devicePixelRatio || 1}:${w.freq}:${w.level}`;
    if (version === g.drawn && v === g.src && laid === g.laid) continue;
    // already drawn at rest for this layout/params and no loud frame arrived since: skip silent frames
    if (g.resting && laid === g.laid && v === g.src && v !== undefined && v.loud <= g.drawn) {
      g.drawn = version;
      continue;
    }
    const res = drawRingGlass(g, v);
    if (res === null) continue;
    g.resting = res;
    g.drawn = version;
    g.src = v;
    g.laid = laid;
  }
  rafId = requestAnimationFrame(frame);
}

/** Linear output peak as normalized energy in 0..1 (FLOOR at 0, 0 dBFS at 1). */
const energyOf = (a: number): number => {
  if (a <= 0) return 0;
  const db = 20 * Math.log10(Math.max(1e-6, a));
  return clamp((db - FLOOR) / -FLOOR, 0, 1);
};

/** Smooth edge taper over the outer 2.5% of the carrier width so the sine meets the zero axis cleanly. */
const edgeWindow = (p: number): number => {
  const t = 0.025;
  if (p < t) return 0.5 - 0.5 * Math.cos((Math.PI * p) / t);
  if (p > 1 - t) return 0.5 - 0.5 * Math.cos((Math.PI * (1 - p)) / t);
  return 1;
};

/** Draw `gl`; returns whether the glass is at rest (silent), or null when not laid out yet. */
function drawRingGlass(gl: Glass, v: UnitMeterView | undefined): boolean | null {
  const cv = gl.canvas;
  const W = cv.clientWidth;
  const H = cv.clientHeight;
  if (W === 0 || H === 0) return null;
  // the face's own scale (its window's size step, or a resize) sharpens the backing too
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

  const narrow = H < 200;
  const G = ringGeometry(W, H);
  const { X0, X1, MX, YC, HALF } = G;
  const spanW = X1 - X0;

  // ── 1. Oscilloscope graticule: ±1 rails, faint ±0.5 guides, quarter verticals, engraved zero axis ──
  g.fillStyle = 'rgba(255,255,255,.055)';
  g.fillRect(X0, YC - HALF, spanW, 1);
  g.fillRect(X0, YC + HALF, spanW, 1);
  g.fillStyle = 'rgba(255,255,255,.028)';
  g.fillRect(X0, Math.round(YC - HALF * 0.5), spanW, 1);
  g.fillRect(X0, Math.round(YC + HALF * 0.5), spanW, 1);
  for (let q = 1; q < 4; q++) {
    g.fillRect(Math.round(X0 + (spanW * q) / 4), YC - HALF, 1, 2 * HALF);
  }
  // engraved zero line (OTT's 0 dB recipe)
  g.fillStyle = '#000';
  g.fillRect(X0, YC + 1, spanW, 1);
  g.fillStyle = 'rgba(255,255,255,.18)';
  g.fillRect(X0, YC, spanW, 1);

  // ── 2. Read output energy and 1-second peak hold per channel from the mirror ──────────────────────
  const count = v ? Math.min(v.outCount, OUT_HISTORY) : 0;
  let nowL = 0;
  let nowR = 0;
  let pkL = 0;
  let pkR = 0;
  if (v && count > 0) {
    const newest = (v.outCount - 1) % OUT_HISTORY;
    nowL = v.outL[newest];
    nowR = v.outR[newest];
    const pkN = Math.min(count, PEAK_FRAMES);
    for (let s = 0; s < pkN; s++) {
      const i = (v.outCount - 1 - s) % OUT_HISTORY;
      if (v.outL[i] > pkL) pkL = v.outL[i];
      if (v.outR[i] > pkR) pkR = v.outR[i];
    }
  }
  const envL = energyOf(nowL);
  const envR = energyOf(nowR);
  const uFreq = freqFrac(gl.view.freq);
  const cycles = CYCLES_MIN * Math.pow(CYCLES_MAX / CYCLES_MIN, uFreq);
  const restAmp = narrow ? 5 : 7;
  const ampL = restAmp + envL * (HALF - restAmp);
  const ampR = restAmp + envR * (HALF - restAmp);

  // ── 3. Envelope peak guides when sounding ────────────────────────────────────────────────────────
  if (envL > 0.02) {
    g.fillStyle = rgba(WARM, 0.14 * envL);
    g.fillRect(X0 + 8, Math.round(YC - ampL), spanW - 16, 1);
  }
  if (envR > 0.02) {
    g.fillStyle = rgba(WARM, 0.14 * envR);
    g.fillRect(X0 + 8, Math.round(YC + ampR), spanW - 16, 1);
  }

  // ── 4. Carrier energy body: crisp 2 px columns on a 3 px pitch filling each lit lobe (L up, R down)
  if (envL > 0.015 || envR > 0.015) {
    for (let x = 0; x + 2 <= spanW; x += 3) {
      const p = (x + 1) / spanW;
      const w = edgeWindow(p);
      const s = Math.sin(p * cycles * Math.PI * 2);
      if (s > 0.01 && envL > 0.015) {
        const h = ampL * s * w;
        if (h >= 1.5) {
          const ye = YC - h;
          const gr = g.createLinearGradient(0, YC, 0, ye);
          gr.addColorStop(0, rgba(WARM, 0.05 * envL));
          gr.addColorStop(0.55, rgba(WARM, 0.34 * envL));
          gr.addColorStop(1, rgba(WARM, 0.78 * envL));
          g.fillStyle = gr;
          g.fillRect(X0 + x, ye, 2, h);
        }
      } else if (s < -0.01 && envR > 0.015) {
        const h = ampR * -s * w;
        if (h >= 1.5) {
          const ye = YC + h;
          const gr = g.createLinearGradient(0, YC, 0, ye);
          gr.addColorStop(0, rgba(WARM, 0.05 * envR));
          gr.addColorStop(0.55, rgba(WARM, 0.34 * envR));
          gr.addColorStop(1, rgba(WARM, 0.78 * envR));
          g.fillStyle = gr;
          g.fillRect(X0 + x, YC + 1, 2, h);
        }
      }
    }
  }

  // ── 5. Carrier sine trace: positive half-waves carry L brightness, negative carry R brightness ─────
  // Drawn in two clipped halves around the zero axis so dissimilar L and R each read on their own half,
  // while symmetric stereo joins seamlessly across zero into one continuous sine.
  const drawHalf = (upper: boolean, amp: number, env: number) => {
    g.save();
    g.beginPath();
    if (upper) g.rect(X0 - 1, YC - HALF - 3, spanW + 2, HALF + 4);
    else g.rect(X0 - 1, YC - 1, spanW + 2, HALF + 4);
    g.clip();
    g.beginPath();
    const steps = Math.max(spanW, Math.ceil(cycles * 24));
    for (let i = 0; i <= steps; i++) {
      const p = i / steps;
      const x = X0 + p * spanW;
      const s = Math.sin(p * cycles * Math.PI * 2);
      const y = YC - amp * s * edgeWindow(p);
      if (i === 0) g.moveTo(x, y);
      else g.lineTo(x, y);
    }
    g.strokeStyle = rgba(WARM, 0.18 + 0.78 * env);
    g.lineWidth = 1.25 + 0.85 * env;
    g.lineJoin = 'round';
    g.stroke();
    g.restore();
  };
  drawHalf(true, ampL, envL);
  drawHalf(false, ampR, envR);

  // ── 6. Log frequency ruler (20 Hz – 1.5 kHz) and carrier position marker ─────────────────────────
  const xOfHz = (hz: number) => X0 + freqFrac(hz) * spanW;
  const xFreq = xOfHz(gl.view.freq);
  g.fillStyle = 'rgba(255,255,255,.10)';
  g.fillRect(X0, G.rule, spanW, 1);
  // quiet warm-white track up to the carrier frequency
  g.fillStyle = rgba(WARM, 0.36);
  g.fillRect(X0, G.rule, Math.max(0, xFreq - X0), 2);
  g.fillStyle = 'rgba(255,255,255,.14)';
  for (const hz of MINOR_HZ) g.fillRect(Math.round(xOfHz(hz)), G.rule, 1, 4);
  g.fillStyle = 'rgba(255,255,255,.30)';
  for (const hz of MAJOR_HZ) g.fillRect(Math.round(xOfHz(hz)), G.rule, 1, 7);
  // subtle A4 (440 Hz) reference pip above the rail
  g.fillStyle = rgba(WARM, 0.28);
  g.fillRect(Math.round(xOfHz(A4_HZ)), G.rule - 4, 1, 4);
  // carrier marker: warm-white downward caret perched on the ruler + vertical index tick
  const mx = Math.round(xFreq) + 0.5;
  g.fillStyle = rgba(WARM, 0.96);
  g.beginPath();
  g.moveTo(mx - 4.5, G.rule - 7);
  g.lineTo(mx + 4.5, G.rule - 7);
  g.lineTo(mx, G.rule);
  g.closePath();
  g.fill();
  g.fillRect(mx - 0.5, G.rule, 1, 7);

  // ── 7. OUT stereo LED ladders (FLOOR..0 dBFS, green / amber from -12 / red from -3, 1 s peak) ────
  const top = YC - HALF;
  const bot = YC + HALF;
  const nc = Math.floor((bot - top) / 4);
  const dbc = -FLOOR / nc;
  let meterLit = false;
  for (let ch = 0; ch < 2; ch++) {
    const now = ch === 0 ? nowL : nowR;
    const pk = ch === 0 ? pkL : pkR;
    const c0 = now > 0 ? (20 * Math.log10(Math.max(1e-6, now)) - FLOOR) / dbc : -1;
    const cp = pk > 0 ? Math.floor((20 * Math.log10(Math.max(1e-6, pk)) - FLOOR) / dbc) : -1;
    const x = MX + 3 + ch * 15;
    for (let k = 0; k < nc; k++) {
      const y = bot - (k + 1) * 4 + 1;
      const db = FLOOR + k * dbc;
      const zone = db >= -3 ? 'r' : db >= -12 ? 'y' : 'g';
      const f = clamp(c0 - k, 0, 1);
      g.fillStyle = MET_OFF[zone];
      g.fillRect(x, y, 6, 3);
      if (f > 0.03) {
        g.fillStyle = rgba(MET[zone], 0.3 + 0.7 * f);
        g.fillRect(x, y, 6, 3);
        meterLit = true;
      } else if (k === cp && pk > 0) {
        g.fillStyle = rgba(MET[zone], 0.8);
        g.fillRect(x, y, 6, 3);
        meterLit = true;
      }
    }
  }
  return envL <= 0.015 && envR <= 0.015 && !meterLit;
}
