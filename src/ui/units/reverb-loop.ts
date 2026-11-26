import { OUT_HISTORY, unitMeters, type UnitMeterView } from './unit-meters';

/**
 * The REVERB face's draw loop: its glass is the room's tail. What came out of the unit runs right to left
 * from NOW over the last six seconds: the feed's `unitMeters[].out`, its peak per channel per frame, drawn
 * as one body, L above the centre line and R below (a mono sound is a mirrored shape; what differs between
 * the channels shows). A dry note is a thin spike; a wet one has a long soft cloud behind it, and the
 * cloud's fall is the room's decay, read against the dB lines. Older sound fades into the room.
 *
 * The body's points are tied to fixed slices of time (2 px each, the peak over its slice): as the glass
 * runs each point slides left whole and is never recomputed from other frames, so the edge never shimmers.
 * A frame covers the time since the frame before it (its arrival stamped in the mirror), and the glass is
 * drawn `LAG` behind the clock, so the newest slice is always one a frame has covered.
 *
 * Invariant 6: it reads no signal and writes none (`verify/guards/stage-draw.mjs` walks it); Solid only
 * registers and unregisters the glass. A glass is drawn again only while it holds something lit, or when its
 * meters or its size moved: a silent REVERB costs nothing.
 */

interface Glass {
  canvas: HTMLCanvasElement;
  unit: number;
  drawn: number;
  /** The mirror's view the glass was last drawn from. */
  src: UnitMeterView | undefined;
  /** The size and scale the glass was last drawn at. */
  laid: string;
  /** The last draw lit something, or a frame is still ahead of the glass's time: the sound still runs off it. */
  lit: boolean;
}

const glasses = new Set<Glass>();
let rafId = 0;

/** The glass runs this far behind the clock (s): two feed frames, so the newest slice has its frame. */
const LAG = 0.034;
/** The longest time one frame covers (s): after a gap (a unit held out), a frame paints no more. */
const MAX_COVER = 0.05;
/** The body's floor (dBFS) at the centre line; 0 dBFS reaches the glass's edge. */
const FLOOR = -60;
/** Seconds of sound on the glass, and the pixels of one slice. */
export const SPAN = 6;
const SLICE_PX = 2;
/** Under this many pixels a slice is a speck on the centre line, not a sound. */
const SPECK = 0.75;

const WARM: readonly number[] = [255, 243, 224];
const rgba = (c: readonly number[], a: number): string => `rgba(${c[0]},${c[1]},${c[2]},${a})`;

/** The glass's geometry in CSS pixels, full (852x232) or narrow (852x178): the body from `X0` to NOW at
 * `X1`, the centre line `YC` with 0 dBFS `HALF` from it. The face's print uses it too. */
export function reverbGeometry(W: number, H: number) {
  const narrow = H < 200;
  const YC = narrow ? 80 : 110;
  const HALF = narrow ? 56 : 84;
  return {
    X0: 46,
    X1: W - 34,
    YC,
    HALF,
    /** The time ruler's line under the body, and its print under that (NOW at its right end). */
    rule: YC + HALF + (narrow ? 8 : 9),
    print: H - (narrow ? 20 : 21),
  };
}
/** The dB lines the glass prints (dBFS): 0 is the glass's edge. */
export const DB_LINES: readonly number[] = [0, -20, -40];
/** A level's height from the centre line, as a share of `HALF`. */
export const levelShare = (db: number): number => Math.max(0, Math.min(1, (db - FLOOR) / -FLOOR));

/** Draw a REVERB face's glass for `unit` while it is mounted. */
export function registerReverbGlass(canvas: HTMLCanvasElement, unit: number): () => void {
  const g: Glass = { canvas, unit, drawn: -1, src: undefined, laid: '', lit: false };
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

/** Stop the glasses' draws while they are hidden, and draw them again on `false`; a glass is not unmounted
 * meanwhile, and its last picture stays. */
export function suspendReverbDraw(on: boolean): void {
  suspended = on;
  if (on) {
    if (rafId !== 0) cancelAnimationFrame(rafId);
    rafId = 0;
  } else if (glasses.size > 0 && rafId === 0) rafId = requestAnimationFrame(frame);
}

function frame(): void {
  rafId = 0;
  if (glasses.size === 0 || suspended) return;
  const now = performance.now() / 1000;
  for (const g of glasses) {
    const v = unitMeters.get(g.unit);
    const version = v ? v.version : 0;
    // its size as drawn (the face's own scale moves it, not its layout size)
    const shown = Math.round(g.canvas.getBoundingClientRect().width);
    const laid = `${g.canvas.clientWidth}x${g.canvas.clientHeight}@${shown}x${window.devicePixelRatio || 1}`;
    // silent and still: nothing to draw again (a silent frame moves the version and lights nothing; a loud
    // one since the last draw, however many silent ones followed it, is drawn)
    const fresh = v !== g.src || (v !== undefined && v.loud > g.drawn);
    if (!g.lit && laid === g.laid && !fresh) continue;
    const lit = drawRoom(g, v, now - LAG);
    // a glass not laid out yet draws on a later frame
    if (lit === null) continue;
    g.lit = lit;
    g.drawn = version;
    g.src = v;
    g.laid = laid;
  }
  rafId = requestAnimationFrame(frame);
}

/** A peak (linear) as a height from the centre line: FLOOR at 0, 0 dBFS at `half`. */
const height = (a: number, half: number): number => levelShare(20 * Math.log10(Math.max(1e-6, a))) * half;

/** The scratch the body is built in, per channel: each slice's peak, then its height; and its x. */
let sliceL = new Float32Array(0);
let sliceR = new Float32Array(0);
let hL = new Float32Array(0);
let hR = new Float32Array(0);
let xs = new Float32Array(0);

/** Draw `gl` at glass time `t` (s); whether it lit anything, or null when it is not laid out yet. */
function drawRoom(gl: Glass, v: UnitMeterView | undefined, t: number): boolean | null {
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
  const G = reverbGeometry(W, H);
  const { X0, X1, YC, HALF } = G;
  const pxs = (X1 - X0) / SPAN;
  const sliceS = SLICE_PX / pxs;

  // the slices: each one's peak per channel, from the frames that cover it
  const b1 = Math.floor(t / sliceS);
  const n = Math.ceil((X1 - X0) / SLICE_PX) + 3;
  const b0 = b1 - n + 1;
  if (sliceL.length < n) {
    sliceL = new Float32Array(n);
    sliceR = new Float32Array(n);
    hL = new Float32Array(n);
    hR = new Float32Array(n);
    xs = new Float32Array(n);
  }
  sliceL.fill(0, 0, n);
  sliceR.fill(0, 0, n);
  const count = v ? Math.min(v.outCount, OUT_HISTORY) : 0;
  // a frame newer than the glass's time (it runs `LAG` behind) is not on it yet: the glass draws again
  const ahead = v !== undefined && count > 0 && v.outAt[(v.outCount - 1) % OUT_HISTORY] / 1000 > t;
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

  // the body: one closed shape round the centre line, its edges the two channels' peaks; each slice a point
  // at its time (x moves smoothly, its height is fixed), the oldest and the newest clipped at the glass's ends
  let lit = false;
  for (let j = 0; j < n; j++) {
    xs[j] = X1 - (t - (b0 + j) * sliceS) * pxs - SLICE_PX / 2;
    hL[j] = height(sliceL[j], HALF);
    hR[j] = height(sliceR[j], HALF);
    if (hL[j] >= SPECK || hR[j] >= SPECK) lit = true;
  }
  if (lit) {
    g.save();
    g.beginPath();
    g.rect(X0, 0, X1 - X0, H);
    g.clip();
    // the soft body: dim at the centre line, taking light toward the edges where the loud sound reaches
    const body = g.createLinearGradient(0, YC - HALF, 0, YC + HALF);
    body.addColorStop(0, rgba(WARM, 0.5));
    body.addColorStop(0.5, rgba(WARM, 0.14));
    body.addColorStop(1, rgba(WARM, 0.5));
    g.fillStyle = body;
    g.beginPath();
    g.moveTo(xs[0], YC);
    for (let j = 0; j < n; j++) g.lineTo(xs[j], YC - hL[j]);
    g.lineTo(xs[n - 1], YC);
    for (let j = n - 1; j >= 0; j--) g.lineTo(xs[j], YC + hR[j]);
    g.closePath();
    g.fill();
    // its two edges, a hairline each: a spike stays sharp, the cloud has an outline to read its fall by
    g.strokeStyle = rgba(WARM, 0.8);
    g.lineWidth = 1.25;
    g.lineJoin = 'round';
    for (const [h, dir] of [
      [hL, -1],
      [hR, 1],
    ] as const) {
      g.beginPath();
      let pen = false;
      for (let j = 0; j < n; j++) {
        const on = h[j] >= SPECK || (j > 0 && h[j - 1] >= SPECK) || (j + 1 < n && h[j + 1] >= SPECK);
        if (!on) {
          pen = false;
          continue;
        }
        if (pen) g.lineTo(xs[j], YC + dir * h[j]);
        else g.moveTo(xs[j], YC + dir * h[j]);
        pen = true;
      }
      g.stroke();
    }
    // the past fades into the room
    g.globalCompositeOperation = 'destination-in';
    const fade = g.createLinearGradient(X0, 0, X1, 0);
    fade.addColorStop(0, 'rgba(0,0,0,0.22)');
    fade.addColorStop(0.5, 'rgba(0,0,0,0.62)');
    fade.addColorStop(1, 'rgba(0,0,0,1)');
    g.fillStyle = fade;
    g.fillRect(X0, 0, X1 - X0, H);
    g.restore();
  }

  // the lines the sound is read against, behind it: the dB lines both sides of the centre line, the centre
  // line, NOW, and a tick a second on the ruler under the glass (a longer one every two)
  g.globalCompositeOperation = 'destination-over';
  for (const db of DB_LINES) {
    const d = levelShare(db) * HALF;
    g.fillStyle = db === 0 ? 'rgba(255,255,255,.14)' : 'rgba(255,255,255,.075)';
    g.fillRect(X0, Math.round(YC - d), X1 - X0, 1);
    g.fillRect(X0, Math.round(YC + d), X1 - X0, 1);
  }
  g.fillStyle = 'rgba(255,255,255,.13)';
  g.fillRect(X0, YC, X1 - X0, 1);
  g.fillStyle = rgba(WARM, 0.45);
  g.fillRect(X1, YC - HALF - 6, 1, 2 * HALF + 12);
  for (let s = 0; s <= SPAN; s++) {
    g.fillStyle = s % 2 === 0 ? 'rgba(255,255,255,.26)' : 'rgba(255,255,255,.14)';
    g.fillRect(Math.round(X1 - s * pxs), G.rule, 1, s % 2 === 0 ? 7 : 4);
  }
  g.fillStyle = 'rgba(255,255,255,.09)';
  g.fillRect(X0, G.rule, X1 - X0, 1);
  g.globalCompositeOperation = 'source-over';
  return lit || ahead;
}
