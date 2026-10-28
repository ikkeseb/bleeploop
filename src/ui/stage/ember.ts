import { PEAK_FRAMES, looper } from '../state/audio';
import { LANES, feed, type LaneKind } from './stage-feed';
import {
  ALPHAS,
  TURN_STEPS,
  clamp,
  contourAlpha,
  contourColor,
  createStamp,
  decay,
  env,
  holdsAudio,
  hudMsg,
  hudPad,
  laneChanged,
  light,
  lightColor,
  mutedLook,
  peaks,
  sampleEnvelope,
  turnTo,
  waitColor,
  withAlpha,
  type Palette,
  type StageViewDef,
  type Surface,
  type ViewDraw,
} from './visual';

/**
 * EMBER: five thin rings of light on black, each lane's loop wrapped once around as a fine waveform
 * (lane 1 outermost, twelve o'clock is the loop start), its state in one hue: ice plays (a glacial take
 * on the play colour), amber writes, red records, a ghost grey is muted or stopped. A star rides each
 * sounding ring at the playhead, and the instrument streams in from beyond the window as a braided
 * filament that lands on the pen: the write head while a lane overdubs or records (amber, red), else the
 * selected lane's place under the playhead (steel), always as bright as the input meter.
 *
 * Baked, and redrawn only when a lane's form changes (`laneChanged`) or on layout: one window-sized
 * bitmap (the floor, its haze, the beat ticks and every ring's contour, dim and static; a change redraws
 * only that ring's annulus) and a quarter-scale atlas of each ring's bloom, blurred once. A take in
 * flight re-bakes its bloom at most every GLOW_MS. A frame is blits plus the moving light: each sounding
 * ring's bloom at its own level, a wake behind the playhead, the stars (sprites made on layout), the
 * filament and its sparks (a few strokes and rects), the selected ring's warm-white spine. No blur,
 * shadow or gradient is made in a frame; every size passed to the canvas is whole device px (the
 * filament's whole quarter px under a fixed scale), every alpha and rotation comes from `visual.ts`'s
 * tables, and the frame's own fractions live in a typed array, so this module's frame code allocates
 * nothing in steady playback (the stage-view probe's budget case).
 *
 * The filament reads the input meter (`light.input`), not the engine's scope columns: its waves are
 * motion, its brightness and swing are the level, and that costs the engine nothing.
 */

const TAU = Math.PI * 2;
const TOP = -Math.PI / 2;
const MAX_N = 1440; // spikes on a ring at most
const SPACING = 3; // device px between a ring's spikes
const DS = 4; // the bloom atlas is this many times smaller than the window
const GLOW_MS = 120; // a take in flight re-bakes its bloom at most this often
const K = 64; // the filament's segments
const STRANDS = 3;
const SWEEP = 0.55; // how far (radians) the filament trails the head at its far end
// The strands: waves along the filament, their speed toward the head (rad/s), phase and share of the swing.
const WAVE = Float64Array.of(2.3, 3.4, 5.2);
const SPEED = Float64Array.of(4.6, 6.3, 8.1);
const PHASE = Float64Array.of(0, 2.2, 4.1);
const SHARE = Float64Array.of(1, 0.7, 0.48);
// Sparks drift in along the filament: where each starts, its speed (filaments a second) and its offset.
const SPARKS = 28;
const SPARK_U = new Float64Array(SPARKS);
const SPARK_V = new Float64Array(SPARKS);
const SPARK_J = new Float64Array(SPARKS);
{
  let seed = 11;
  const rnd = (): number => (seed = (seed * 16807) % 2147483647) / 2147483647;
  for (let k = 0; k < SPARKS; k++) {
    SPARK_U[k] = rnd();
    SPARK_V[k] = 0.2 + 0.32 * rnd();
    SPARK_J[k] = rnd() * 2 - 1;
  }
}
// The frame's fractions, read by the helpers it calls (a double passed to a call is boxed).
const SEC = 0; // the frame's time in seconds
const SWAY = 1; // the filament's swing, device px
const GLOW = 2; // the sparks' alpha
const mem = new Float64Array(3);
const DASH = [3, 5];
const NO_DASH: number[] = [];

/** `a` moved `t` of the way to `b`, both `#rrggbb` (a token in another form comes back as `a`). */
function mix(a: string, b: string, t: number): string {
  const pa = /^#([0-9a-f]{6})$/i.exec(a);
  const pb = /^#([0-9a-f]{6})$/i.exec(b);
  if (!pa || !pb) return a;
  const x = parseInt(pa[1], 16);
  const y = parseInt(pb[1], 16);
  let out = '#';
  for (const sh of [16, 8, 0]) out += Math.round(((x >> sh) & 255) * (1 - t) + ((y >> sh) & 255) * t).toString(16).padStart(2, '0');
  return out;
}

/** A star `size` px square: a soft halo, a long streak along the ring (the sprite's y, since it is
 * drawn turned to the playhead), a short one across it and a hot core. Made on layout. */
function star(color: string, hot: string, size: number, glow: number): HTMLCanvasElement {
  const cv = document.createElement('canvas');
  cv.width = cv.height = size;
  const x = cv.getContext('2d');
  if (!x) return cv;
  const m = size / 2;
  x.globalCompositeOperation = 'lighter';
  const halo = x.createRadialGradient(m, m, 0, m, m, m);
  halo.addColorStop(0, withAlpha(color, 0.5 * glow));
  halo.addColorStop(0.1, withAlpha(color, 0.24 * glow));
  halo.addColorStop(0.36, withAlpha(color, 0.06 * glow));
  halo.addColorStop(1, withAlpha(color, 0));
  x.fillStyle = halo;
  x.fillRect(0, 0, size, size);
  const flare = x.createRadialGradient(0, 0, 0, 0, 0, m);
  flare.addColorStop(0, withAlpha(hot, 0.85));
  flare.addColorStop(0.2, withAlpha(color, 0.32));
  flare.addColorStop(1, withAlpha(color, 0));
  x.fillStyle = flare;
  for (const [sx, sy] of [[0.045, 1], [0.5, 0.035]]) {
    x.setTransform(sx, 0, 0, sy, m, m);
    x.beginPath();
    x.arc(0, 0, m, 0, TAU);
    x.fill();
  }
  x.setTransform(1, 0, 0, 1, 0, 0);
  const core = x.createRadialGradient(m, m, 0, m, m, size * 0.07);
  core.addColorStop(0, withAlpha(hot, 1));
  core.addColorStop(0.45, withAlpha(color, 0.6));
  core.addColorStop(1, withAlpha(color, 0));
  x.fillStyle = core;
  x.fillRect(0, 0, size, size);
  return cv;
}

interface Wakes {
  ice: CanvasGradient;
  ember: CanvasGradient;
  ghost: CanvasGradient;
}

function createEmber(): ViewDraw {
  const base = document.createElement('canvas');
  const bctx = base.getContext('2d');
  const atlas = document.createElement('canvas');
  const actx = atlas.getContext('2d');
  const scratch = document.createElement('canvas');
  const sctx = scratch.getContext('2d');
  const stamp = createStamp();
  const A = ALPHAS;
  // Each lane's envelope, kept between redraws: only a `stale` lane is sampled again.
  const hi = Array.from({ length: LANES }, () => new Float32Array(MAX_N));
  const lo = Array.from({ length: LANES }, () => new Float32Array(MAX_N));
  const body = Array.from({ length: LANES }, () => new Float32Array(MAX_N));
  const n = new Int32Array(LANES); // spikes around each ring
  const cols = new Int32Array(LANES); // how many of them hold audio
  const stale = new Uint8Array(LANES).fill(1);
  // The bloom: due since the lane's form changed; baked at once for a new state, else at most every GLOW_MS.
  const glowDue = new Uint8Array(LANES).fill(1);
  const glowAt = new Float64Array(LANES).fill(-1e9);
  const glowKind = Array.from({ length: LANES }, (): LaneKind | '' => '');
  const glowMuted = Array.from({ length: LANES }, () => false);
  // Device px, whole numbers, laid out once (`layout`).
  const r = new Int32Array(LANES); // each ring's radius
  const cell = new Int32Array(LANES); // each ring's bloom: its side in the atlas
  const cellX = new Int32Array(LANES); // and where it starts
  const fx = new Int32Array(STRANDS * (K + 1)); // the filament, quarter device px
  const fy = new Int32Array(STRANDS * (K + 1));
  let dpr = 1;
  let cx = 0;
  let cy = 0;
  let R = 0; // ring 1's radius
  let S = 0; // ring spacing
  let sw = 0; // a spike's reach at full scale, each way
  let reach = 0; // the filament's far end: past the window's farthest corner
  let hair = 1;
  let line = 2;
  let bold = 2;
  let zs = 0; // half a star sprite
  let zf = 0; // half a pen flare
  let wide4 = 4; // the filament's line widths, quarter px
  let thin4 = 4;
  let spark4 = 4;
  let dirty = true;
  let full = true;
  let beats = -1;
  let perBar = 4;
  let wakes: Wakes | null = null;
  // Colours and their light, made on layout from the tokens.
  let ice = '';
  let ember = '';
  let blurNear = 'none';
  let blurFar = 'none';
  let haze: CanvasGradient | null = null;
  let vignette: CanvasGradient | null = null;
  let strandSteel: CanvasGradient | null = null;
  let strandEmber: CanvasGradient | null = null;
  let strandRec: CanvasGradient | null = null;
  let starIce = scratch;
  let starEmber = scratch;
  let starRec = scratch;
  let starGhost = scratch;
  let flareSteel = scratch;
  let flareEmber = scratch;
  let flareRec = scratch;

  function layout(s: Surface): void {
    const pal = s.pal;
    dpr = s.dpr;
    // The stack fills the height left above the message line, never more than 0.46 of the width a side.
    const top = 6;
    const bottom = hudPad(s.h) + hudMsg(s.h) * 1.3 + 6;
    const half = Math.max(40, Math.min(0.46 * s.w, (s.h - top - bottom) / 2));
    // Outside ring 1: its spikes (0.42 S), then the beat ticks (from 0.62 S, 0.05 R long): 1.16 R.
    const Rcss = half / 1.16;
    R = Math.round(Rcss * dpr);
    S = Math.round(0.175 * R);
    sw = Math.round(0.42 * S);
    for (let i = 0; i < LANES; i++) {
      r[i] = R - i * S;
      n[i] = clamp(Math.round((TAU * r[i]) / SPACING), 90, MAX_N);
    }
    hair = Math.max(1, Math.round(dpr));
    line = Math.max(1, Math.round(1.5 * dpr));
    bold = Math.max(2, Math.round(2 * dpr));
    cx = Math.round((s.w / 2) * dpr);
    cy = Math.round((top + half) * dpr);
    base.width = s.dw;
    base.height = s.dh;
    reach = Math.ceil(Math.hypot(Math.max(cx, s.dw - cx), Math.max(cy, s.dh - cy)) + 40 * dpr);

    // The bloom atlas: a cell per ring, wide enough for its far blur.
    const margin = Math.round(64 * dpr);
    let x = 0;
    for (let i = 0; i < LANES; i++) {
      cell[i] = 2 * Math.ceil((r[i] + sw + margin) / DS);
      cellX[i] = x;
      x += cell[i] + 2;
    }
    atlas.width = x;
    atlas.height = cell[0];
    scratch.width = scratch.height = cell[0];
    blurNear = `blur(${(1.4 * dpr).toFixed(2)}px)`;
    blurFar = `blur(${(4.6 * dpr).toFixed(2)}px)`;

    // Ice is the play colour cooled toward glacier: still play green to the eye that counts it.
    ice = mix(mix(pal.play, '#ffffff', 0.32), pal.cyan, 0.12);
    ember = mix(pal.dub, pal.rec, 0.12);
    const steel = mix(pal.dim, pal.cyan, 0.3);
    zs = Math.max(8, Math.round(1.5 * sw));
    zf = Math.max(12, Math.round(2.8 * sw));
    starIce = star(ice, mix(ice, '#ffffff', 0.3), 2 * zs, 1);
    starEmber = star(ember, mix(ember, '#ffffff', 0.3), 2 * zs, 1);
    starRec = star(pal.rec, mix(pal.rec, '#ffffff', 0.25), 2 * zs, 1);
    starGhost = star(pal.faint, pal.faint, 2 * zs, 0.6);
    flareSteel = star(steel, mix(steel, '#ffffff', 0.2), 2 * zf, 0.8);
    flareEmber = star(ember, mix(ember, '#ffffff', 0.3), 2 * zf, 1);
    flareRec = star(pal.rec, mix(pal.rec, '#ffffff', 0.25), 2 * zf, 1);
    wide4 = 4 * Math.max(3, Math.round(5 * dpr));
    thin4 = 4 * Math.max(1, Math.round(1.25 * dpr));
    spark4 = 4 * Math.max(1, Math.round(1.6 * dpr));

    const bc = bctx;
    if (bc) {
      // The floor: a cool haze behind the rings, the corners sinking to black.
      haze = bc.createRadialGradient(cx, cy, 0.2 * R, cx, cy, 1.4 * R);
      const air = mix(pal.text, ice, 0.25);
      haze.addColorStop(0, withAlpha(air, 0));
      haze.addColorStop(0.5, withAlpha(air, 0.035));
      haze.addColorStop(0.8, withAlpha(air, 0.02));
      haze.addColorStop(1, withAlpha(air, 0));
      vignette = bc.createRadialGradient(cx, cy, 0.5 * Math.max(cx, cy), cx, cy, reach);
      vignette.addColorStop(0, 'rgba(0,0,0,0)');
      vignette.addColorStop(1, 'rgba(0,0,0,0.6)');
    }
    const fc = s.ctx;
    // The filament's light falls off past the stack (built in the quarter-px space it is drawn in).
    const fall = (color: string): CanvasGradient => {
      const g = fc.createRadialGradient(4 * cx, 4 * cy, 0, 4 * cx, 4 * cy, 4 * reach);
      g.addColorStop(0, withAlpha(color, 1));
      g.addColorStop(Math.min(0.95, (R + S) / reach), withAlpha(color, 0.9));
      g.addColorStop(1, withAlpha(color, 0.12));
      return g;
    };
    strandSteel = fall(steel);
    strandEmber = fall(ember);
    strandRec = fall(pal.rec);

    s.countX = s.w / 2;
    s.countY = top + half;
    // Inside the ring 5's spikes (0.23 R) at every size: a digit is about 0.6 em wide and 0.72 em tall.
    s.countSize = Math.min(clamp(0.36 * s.h, 140, 480), 0.5 * Rcss);
    stale.fill(1);
    glowDue.fill(1);
    glowKind.fill('');
    wakes = null;
    dirty = true;
    full = true;
  }

  /** A wake: the lane's light rising from nothing a beat behind the playhead to full at it. */
  function wake(c: CanvasRenderingContext2D, color: string, span: number): CanvasGradient {
    const g = c.createConicGradient(-span, 0, 0);
    const end = span / TAU;
    g.addColorStop(0, withAlpha(color, 0));
    g.addColorStop(end * 0.55, withAlpha(color, 0.3));
    g.addColorStop(end, withAlpha(color, 1));
    g.addColorStop(Math.min(1, end + 0.0005), withAlpha(color, 0));
    g.addColorStop(1, withAlpha(color, 0));
    return g;
  }

  /** The colour of lane `i`'s bloom: ice, ember, rec-red or ghost grey. */
  function glowColor(pal: Palette, i: number): string {
    const c = lightColor(pal, i);
    return c === pal.play ? ice : c === pal.dub ? ember : c;
  }

  /**
   * Lane `i`'s spikes around (x0, y0) at `radius`: per column a radial stroke out by the peak (or the
   * body) and in by the trough (or most of the body), at least `floor` each way. Bake only.
   */
  function spikes(c: CanvasRenderingContext2D, x0: number, y0: number, radius: number, reachPx: number, i: number, ofBody: boolean, floor: number): void {
    const N = n[i];
    const h = hi[i];
    const l = lo[i];
    const b = body[i];
    c.beginPath();
    for (let j = 0; j < cols[i]; j++) {
      const a = TOP + (TAU * (j + 0.5)) / N;
      const co = Math.cos(a);
      const si = Math.sin(a);
      const out = radius + Math.max(floor, reachPx * (ofBody ? b[j] : h[j]));
      const inn = radius - Math.max(floor, reachPx * (ofBody ? 0.8 * b[j] : -l[j]));
      c.moveTo(x0 + co * inn, y0 + si * inn);
      c.lineTo(x0 + co * out, y0 + si * out);
    }
    c.stroke();
  }

  /** The floor under whatever the clip leaves open: the token background, the haze, the vignette. */
  function floor(c: CanvasRenderingContext2D, pal: Palette): void {
    c.globalAlpha = 1;
    c.fillStyle = pal.bg;
    c.fillRect(0, 0, base.width, base.height);
    if (haze) {
      c.fillStyle = haze;
      c.fillRect(0, 0, base.width, base.height);
    }
    if (vignette) {
      c.fillStyle = vignette;
      c.fillRect(0, 0, base.width, base.height);
    }
  }

  /** Lane `i` on the bitmap: its envelope (sampled again only when `stale`), the hairline or the tape
   * where it holds no audio, and its contour: a core line under the peaks under the body. */
  function paintLane(c: CanvasRenderingContext2D, pal: Palette, i: number, master: number): void {
    const kind = feed.kind[i];
    if (stale[i]) {
      looper.peaksInto(i, peaks);
      const span = stamp.span[i];
      const held = holdsAudio(kind) ? sampleEnvelope(n[i], span > 0 ? span / PEAK_FRAMES : peaks.count) : 0;
      for (let k = 0; k < held; k++) {
        hi[i][k] = env.hi[k];
        lo[i][k] = env.lo[k];
        body[i][k] = env.body[k];
      }
      cols[i] = held;
      stale[i] = 0;
    }
    const N = n[i];
    const k = cols[i];
    const end = TOP + (TAU * k) / N;
    if (k < N) {
      // Where no audio is: a hairline, or the dotted rec-red tape a later take will fill.
      const tape = kind === 'rec' && master > 0;
      c.strokeStyle = tape ? pal.rec : pal.text;
      c.globalAlpha = tape ? 0.55 : 0.06;
      c.lineWidth = tape ? line : hair;
      c.setLineDash(tape ? DASH : NO_DASH);
      c.beginPath();
      c.arc(cx, cy, r[i], end, TOP + TAU);
      c.stroke();
      c.setLineDash(NO_DASH);
    }
    if (k > 0) {
      const color = contourColor(pal, i);
      // The ghost states keep their grey brighter than the bright states' dim share would leave it.
      const mul = Math.min(0.9, 1.6 * contourAlpha(i));
      c.strokeStyle = color === pal.play ? ice : color;
      c.globalAlpha = 0.5 * mul;
      c.lineWidth = hair;
      c.beginPath();
      c.arc(cx, cy, r[i], TOP, end);
      c.stroke();
      c.globalAlpha = 0.42 * mul;
      c.lineWidth = 0.9 * dpr;
      spikes(c, cx, cy, r[i], sw, i, false, 0.5 * dpr);
      c.globalAlpha = 0.95 * mul;
      c.lineWidth = 1.4 * dpr;
      spikes(c, cx, cy, r[i], sw, i, true, 0.5 * dpr);
    }
  }

  /**
   * Redraw the bitmap. `full` (a layout, a change of the beat count): the floor, the beat ticks and
   * every ring. Otherwise only the rings whose form changed, each in its own annulus (half a spacing
   * each way, which its spikes never leave).
   */
  function rasterise(pal: Palette): void {
    const c = bctx;
    if (!c) return;
    const master = looper.masterFramesValue();
    c.setTransform(1, 0, 0, 1, 0, 0);
    c.globalCompositeOperation = 'source-over';
    if (full) {
      floor(c, pal);
      // Beat ticks outside ring 1; a bar's first longer and brighter.
      c.fillStyle = pal.text;
      const from = R + Math.round(0.62 * S);
      for (let b = 0; b < beats; b++) {
        const a = TOP + (TAU * b) / beats;
        const bar = b % perBar === 0;
        c.setTransform(Math.cos(a), Math.sin(a), -Math.sin(a), Math.cos(a), cx, cy);
        c.globalAlpha = bar ? 0.32 : 0.13;
        c.fillRect(from, -dpr / 2, Math.round((bar ? 0.05 : 0.022) * R), dpr);
      }
      c.setTransform(1, 0, 0, 1, 0, 0);
      for (let i = 0; i < LANES; i++) paintLane(c, pal, i, master);
      full = false;
    } else {
      const half = S >> 1;
      for (let i = 0; i < LANES; i++) {
        if (!stale[i]) continue;
        c.save();
        c.beginPath();
        c.arc(cx, cy, r[i] + half, 0, TAU);
        c.arc(cx, cy, r[i] - half, 0, TAU, true);
        c.clip();
        floor(c, pal);
        paintLane(c, pal, i, master);
        c.restore();
      }
    }
    c.globalAlpha = 1;
  }

  /** Lane `i`'s bloom cell: its spikes at a quarter scale in its light colour, blurred near and far. */
  function bakeGlow(pal: Palette, i: number): void {
    const a = actx;
    const x = sctx;
    glowDue[i] = 0;
    glowKind[i] = feed.kind[i];
    glowMuted[i] = feed.muted[i];
    if (!a || !x) return;
    const g = cell[i];
    const m = g / 2;
    a.save();
    a.beginPath();
    a.rect(cellX[i], 0, g, g);
    a.clip();
    a.clearRect(cellX[i], 0, g, g);
    if (cols[i] > 0) {
      x.setTransform(1, 0, 0, 1, 0, 0);
      x.globalAlpha = 1;
      x.clearRect(0, 0, g, g);
      x.strokeStyle = glowColor(pal, i);
      x.lineWidth = 1.1 * dpr;
      spikes(x, m, m, r[i] / DS, sw / DS, i, false, 0.4 * dpr);
      x.globalAlpha = 0.7;
      x.lineWidth = 0.8 * dpr;
      x.beginPath();
      x.arc(m, m, r[i] / DS, TOP, TOP + (TAU * cols[i]) / n[i]);
      x.stroke();
      a.filter = blurNear;
      a.globalAlpha = 1;
      a.drawImage(scratch, 0, 0, g, g, cellX[i], 0, g, g);
      a.filter = blurFar;
      a.globalCompositeOperation = 'lighter';
      a.globalAlpha = 0.85;
      a.drawImage(scratch, 0, 0, g, g, cellX[i], 0, g, g);
    }
    a.restore();
  }

  /** The filament from beyond the window to the pen on ring `p` at `step`, into `fx`/`fy` (quarter
   * device px): three strands braided round a curve that leaves the head outward and trails behind it,
   * their waves running in toward the head, their swing tapering to nothing at it. */
  function strandPath(p: number, step: number): void {
    const t = mem[SEC];
    const sway = mem[SWAY];
    const th = TOP + (TAU * step) / TURN_STEPS;
    const r0 = r[p];
    const span = reach - r0;
    for (let k = 0; k <= K; k++) {
      const u = k / K;
      const rho = r0 + span * u;
      const phi = th - SWEEP * u * u;
      const co = Math.cos(phi);
      const si = Math.sin(phi);
      const bx = cx + co * rho;
      const by = cy + si * rho;
      const taper = (u < 0.16 ? u / 0.16 : 1) * sway;
      const drift = 0.55 * Math.sin(3.1 * u + 0.9 * t);
      for (let j = 0; j < STRANDS; j++) {
        const d = taper * (drift + SHARE[j] * Math.sin(TAU * WAVE[j] * u + SPEED[j] * t + PHASE[j]));
        const o = j * (K + 1) + k;
        fx[o] = ((bx - si * d) * 4) | 0;
        fy[o] = ((by + co * d) * 4) | 0;
      }
    }
  }

  function strandStroke(c: CanvasRenderingContext2D, j: number): void {
    const o = j * (K + 1);
    c.beginPath();
    c.moveTo(fx[o], fy[o]);
    for (let k = 1; k <= K; k++) c.lineTo(fx[o + k], fy[o + k]);
    c.stroke();
  }

  /** The sparks, drifting in along the filament to ring `p`'s pen at `step` (quarter px), brightest
   * near it. */
  function sparks(c: CanvasRenderingContext2D, p: number, step: number): void {
    const t = mem[SEC];
    const sway = mem[SWAY];
    const v = mem[GLOW];
    const th = TOP + (TAU * step) / TURN_STEPS;
    const r0 = r[p];
    const span = reach - r0;
    const h = spark4 >> 1;
    for (let k = 0; k < SPARKS; k++) {
      let u = SPARK_U[k] - t * SPARK_V[k];
      u -= Math.floor(u);
      const j = k % STRANDS;
      const rho = r0 + span * u;
      const phi = th - SWEEP * u * u;
      const co = Math.cos(phi);
      const si = Math.sin(phi);
      const taper = (u < 0.16 ? u / 0.16 : 1) * sway;
      const d = taper * (0.55 * Math.sin(3.1 * u + 0.9 * t) + SHARE[j] * Math.sin(TAU * WAVE[j] * u + SPEED[j] * t + PHASE[j])) + SPARK_J[k] * (2 + 22 * u) * dpr;
      c.globalAlpha = A[(v * (1 - u) * 255) | 0];
      c.fillRect((((cx + co * rho - si * d) * 4) | 0) - h, (((cy + si * rho + co * d) * 4) | 0) - h, spark4, spark4);
    }
  }

  function ring(c: CanvasRenderingContext2D, radius: number): void {
    c.beginPath();
    c.arc(cx, cy, radius, 0, TAU);
    c.stroke();
  }

  function draw(s: Surface, now: number): void {
    const c = s.ctx;
    const pal = s.pal;
    const master = looper.masterFramesValue();
    for (let i = 0; i < LANES; i++) {
      if (laneChanged(stamp, i)) {
        stale[i] = 1;
        glowDue[i] = 1;
        dirty = true;
      }
    }
    if (feed.beatsPerLoop !== beats || feed.beatsPerBar !== perBar) {
      beats = feed.beatsPerLoop;
      perBar = feed.beatsPerBar;
      wakes = null;
      dirty = true;
      full = true;
    }
    if (dirty) {
      rasterise(pal);
      dirty = false;
    }
    for (let i = 0; i < LANES; i++) {
      if (glowDue[i] && (feed.kind[i] !== glowKind[i] || feed.muted[i] !== glowMuted[i] || now - glowAt[i] >= GLOW_MS)) {
        glowAt[i] = now;
        bakeGlow(pal, i);
      }
    }
    if (!wakes) {
      const span = beats > 0 ? TAU / beats : TAU / 16;
      wakes = { ice: wake(c, ice, span), ember: wake(c, ember, span), ghost: wake(c, pal.faint, span) };
    }

    const still = feed.reduced;
    const dim = light.dim;
    const move = light.move;
    const turn = (light.phase * TURN_STEPS) | 0;
    const beat = still ? 0 : decay(now, light.beatAt, 280) * (light.beatBar ? 1 : 0.6);
    mem[SEC] = now / 1000;

    c.setTransform(1, 0, 0, 1, 0, 0);
    c.globalCompositeOperation = 'source-over';
    if (dim < 0.995) {
      c.globalAlpha = 1;
      c.fillStyle = pal.bg;
      c.fillRect(0, 0, s.dw, s.dh);
    }
    c.globalAlpha = A[(dim * 255) | 0];
    c.drawImage(base, 0, 0);

    // Light: each sounding ring's bloom at its own level (a fixed share of it under reduced motion).
    c.globalCompositeOperation = 'lighter';
    for (let i = 0; i < LANES; i++) {
      const g = dim * light.lit[i];
      if (g <= 0.004 || cols[i] === 0) continue;
      const L = still ? 0.6 : light.amp[i] * (feed.fading[i] ? 0.5 : 1);
      const G = cell[i] * DS;
      c.globalAlpha = A[(g * (0.3 + 0.6 * L) * 255) | 0];
      c.drawImage(atlas, cellX[i], 0, cell[i], cell[i], cx - (G >> 1), cy - (G >> 1), G, G);
    }

    // The playhead: a wake behind it on every ring that plays, a fine radial line, a star on each.
    if (master > 0 && move > 0.004) {
      turnTo(c, cx, cy, turn);
      if (!still) {
        for (let i = 0; i < LANES; i++) {
          const kind = feed.kind[i];
          if (kind !== 'play' && kind !== 'dub') continue;
          const muted = mutedLook(i);
          const a = dim * move * (0.3 + 0.6 * light.amp[i]) * (muted ? 0.45 : 1);
          if (a <= 0.004) continue;
          c.strokeStyle = muted ? wakes.ghost : kind === 'dub' ? wakes.ember : wakes.ice;
          c.globalAlpha = A[(0.16 * a * 255) | 0];
          c.lineWidth = sw;
          c.beginPath();
          c.arc(0, 0, r[i], 0, TAU);
          c.stroke();
          c.globalAlpha = A[(0.8 * a * 255) | 0];
          c.lineWidth = line;
          c.beginPath();
          c.arc(0, 0, r[i], 0, TAU);
          c.stroke();
        }
      }
      c.fillStyle = pal.text;
      c.globalAlpha = A[(0.1 * dim * move * 255) | 0];
      // `0 -`, not a unary minus: a one-px hairline would pass -0, a double boxed every frame.
      c.fillRect(r[LANES - 1] - sw, 0 - (hair >> 1), R - r[LANES - 1] + 2 * sw, hair);
      for (let i = 0; i < LANES; i++) {
        const kind = feed.kind[i];
        if (kind !== 'play' && kind !== 'dub') continue;
        const muted = mutedLook(i);
        const a = dim * move * (muted ? 0.3 : (0.55 + 0.45 * light.amp[i]) * (1 + 0.4 * beat));
        c.globalAlpha = A[(a * 255) | 0];
        c.drawImage(muted ? starGhost : kind === 'dub' ? starEmber : starIce, r[i] - zs, -zs);
      }
      c.setTransform(1, 0, 0, 1, 0, 0);
    }
    // A take in flight has its own head, where its contour ends (a first take's runs around its ring).
    for (let i = 0; i < LANES; i++) {
      if (feed.kind[i] !== 'rec') continue;
      const at = looper.recHeadFrac(i);
      turnTo(c, cx, cy, at >= 0 ? (at * TURN_STEPS) | 0 : master > 0 ? turn : 0);
      c.globalAlpha = A[(dim * (0.75 + 0.25 * beat) * 255) | 0];
      c.drawImage(starRec, r[i] - zs, -zs);
      c.setTransform(1, 0, 0, 1, 0, 0);
    }

    // The instrument: the filament to the pen (the write head, else the selected lane under the
    // playhead), as bright as the input; red for a moment on a clip.
    const v0 = still ? 0 : dim * Math.pow(light.input, 0.7);
    if (v0 > 0.004) {
      let p = -1;
      const own = feed.kind[feed.selected];
      if (own === 'rec' || own === 'dub') p = feed.selected;
      else {
        for (let i = 0; i < LANES; i++) {
          const kind = feed.kind[i];
          if (kind === 'rec' || kind === 'dub') {
            p = i;
            break;
          }
        }
      }
      const writes = p >= 0;
      if (!writes) p = feed.selected;
      const kind = feed.kind[p];
      let step = turn;
      if (kind === 'rec') {
        const at = looper.recHeadFrac(p);
        step = at >= 0 ? (at * TURN_STEPS) | 0 : master > 0 ? turn : 0;
      }
      const v = writes ? v0 : 0.6 * v0;
      mem[SWAY] = (0.25 + 0.75 * light.inputGeo) * 0.55 * S;
      const red = light.clip || kind === 'rec';
      const grad = red ? strandRec : writes ? strandEmber : strandSteel;
      strandPath(p, step);
      c.setTransform(0.25, 0, 0, 0.25, 0, 0);
      if (grad) {
        c.strokeStyle = grad;
        c.fillStyle = grad;
      }
      c.lineJoin = 'round';
      c.lineWidth = wide4;
      for (let j = 0; j < STRANDS; j++) {
        c.globalAlpha = A[(0.07 * v * 255) | 0];
        strandStroke(c, j);
      }
      c.lineWidth = thin4;
      for (let j = 0; j < STRANDS; j++) {
        c.globalAlpha = A[(v * (0.55 - 0.12 * j) * 255) | 0];
        strandStroke(c, j);
      }
      c.lineJoin = 'miter';
      mem[GLOW] = 0.8 * v;
      sparks(c, p, step);
      turnTo(c, cx, cy, step);
      c.globalAlpha = A[(v * (0.6 + 0.4 * beat) * 255) | 0];
      c.drawImage(red ? flareRec : writes ? flareEmber : flareSteel, r[p] - zf, -zf);
      c.setTransform(1, 0, 0, 1, 0, 0);
    }
    c.globalCompositeOperation = 'source-over';

    // Per lane: the armed or listening ring, a pending loop-end stop, a refusal's flash.
    for (let i = 0; i < LANES; i++) {
      const kind = feed.kind[i];
      if (kind === 'armed' || kind === 'listening') {
        const breathe = still ? 0.45 : kind === 'armed' ? 0.3 + 0.35 * decay(now, light.beatAt, 300) : 0.4 + 0.12 * Math.sin((TAU * now) / 1200);
        c.strokeStyle = waitColor(pal, kind);
        c.globalAlpha = A[(dim * breathe * 255) | 0];
        c.lineWidth = line;
        ring(c, r[i]);
      }
      if (feed.stopping[i] && !feed.fading[i] && master > 0) {
        // A stop waits for the loop end: the stretch still to play is traced just outside the spikes.
        turnTo(c, cx, cy, turn);
        c.strokeStyle = pal.dim;
        c.globalAlpha = 0.35;
        c.lineWidth = hair;
        c.beginPath();
        c.arc(0, 0, r[i] + sw + bold, 0, TAU * (1 - light.phase));
        c.stroke();
        c.setTransform(1, 0, 0, 1, 0, 0);
      }
      const cue = decay(now, feed.cueAt[i], 160);
      if (cue > 0) {
        c.strokeStyle = pal.engaged;
        c.globalAlpha = A[(0.8 * cue * 255) | 0];
        c.lineWidth = bold;
        ring(c, r[i] + sw + bold);
      }
    }

    // The selected lane: a warm-white spine through its ring, moved with no transition.
    c.strokeStyle = pal.engaged;
    c.globalAlpha = 0.9;
    c.lineWidth = bold;
    ring(c, r[feed.selected]);
    c.globalAlpha = 1;
  }

  /** The lane whose ring (half a spacing each way) holds the point; the centre and outside: none. */
  function hit(x: number, y: number): number {
    const d = Math.hypot(x * dpr - cx, y * dpr - cy);
    for (let i = 0; i < LANES; i++) if (Math.abs(d - r[i]) <= S / 2) return i;
    return -1;
  }

  return {
    layout,
    draw,
    hit,
    dispose: () => {
      for (const cv of [base, atlas, scratch, starIce, starEmber, starRec, starGhost, flareSteel, flareEmber, flareRec]) cv.width = cv.height = 0;
    },
  };
}

export const ember: StageViewDef = { id: 'ember', name: 'EMBER', create: createEmber };
