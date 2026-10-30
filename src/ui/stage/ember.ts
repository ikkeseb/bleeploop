import { PEAK_FRAMES, looper } from '../state/audio';
import { LANES, feed, type LaneKind } from './stage-feed';
import {
  ALPHAS,
  SPANS,
  TURN_STEPS,
  clamp,
  createStamp,
  decay,
  env,
  holdsAudio,
  hudMsg,
  hudPad,
  laneChanged,
  light,
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
 * EMBER: five rings of light on black, each lane's loop wrapped once around as a waveform (lane 1
 * outermost, twelve o'clock is the loop start): fine needles of light, one per column at its own share
 * of the peak, over a smooth body. Its state is one hue: ice plays (a pale, glacial take on the play
 * colour), amber writes (an overdub's band, the loop's ice a thread inside it), red records, a stopped
 * ring is a silver band and a muted one a thin ghost line. A white-hot star rides each sounding ring at
 * the playhead, and the instrument streams in from beyond the window as a braided filament in a cloud of
 * dust that lands on the pen: the write head while a lane overdubs or records (amber, red; a take's head
 * is held to the end of its drawn band), else the lane about to write (armed, listening), else the
 * selected lane, under the playhead (steel). It is as bright as the input meter, and a silent input
 * draws no stream. The selected ring carries a warm-white rail at its band's inner fringe.
 *
 * Baked, and redrawn only when a lane's form changes (`laneChanged`) or on layout: the ground (the
 * floor, its haze, rays from the centre and dust, all under the census floor), one window-sized bitmap
 * of the ground under every ring's band (a change redraws only that ring's annulus, from the ground),
 * and a quarter-scale atlas of each sounding ring's bloom, blurred once, its band widened before the far
 * blur into a halo (a take in flight re-bakes its bloom at most every GLOW_MS; a muted or stopped ring
 * has none). A frame is blits plus the moving light: each sounding ring's bloom at its own level,
 * blitted only over its annulus (tiles laid out on layout), a wake behind the playhead, the stars
 * (sprites made on layout, their white-hot cores laid over the light rather than added), the filament
 * and its dust (a few strokes and rects), an overdub's thread and the selection's rail. No blur, shadow
 * or gradient is made in a frame; every size passed to the canvas is whole device px (the filament's
 * whole quarter px under a fixed scale), every alpha, rotation and arc span comes from `visual.ts`'s
 * tables, and the frame's own fractions live in a typed array, so this module's frame code allocates
 * nothing in steady playback (the stage-view probe's budget case).
 *
 * The additive light stays under warm white (SELECTED): ice keeps its red under its blue and the hot
 * cores that land on it carry little red, amber and red keep their blue low.
 *
 * The filament reads the input meter (`light.input`), not the engine's scope columns: its waves are
 * motion, its brightness and swing are the level, and that costs the engine nothing.
 */

const TAU = Math.PI * 2;
const TOP = -Math.PI / 2;
const MAX_N = 1600; // columns on a ring at most
const SPACING = 2; // device px between a ring's columns
const DS = 4; // the bloom atlas is this many times smaller than the window
const TILE = 12; // a bloom tile's height, atlas px
const GLOW_MS = 120; // a take in flight re-bakes its bloom at most this often
const K = 64; // the filament's segments
const STRANDS = 4;
const SWEEP = 0.55; // how far (radians) the filament trails the head at its far end
// The strands: waves along the filament, their speed toward the head (rad/s), phase and share of the swing.
const WAVE = Float64Array.of(2.3, 3.4, 5.2, 1.6);
const SPEED = Float64Array.of(4.6, 6.3, 8.1, 3.4);
const PHASE = Float64Array.of(0, 2.2, 4.1, 1.2);
const SHARE = Float64Array.of(1, 0.7, 0.48, 0.85);
// The dust the filament trails: where each mote starts, its speed in (filaments a second), its offset
// from the stream (most close in, a few far out: a cloud, densest at the pen) and whether it is a speck.
const SPARKS = 180;
const SPARK_U = new Float64Array(SPARKS);
const SPARK_V = new Float64Array(SPARKS);
const SPARK_J = new Float64Array(SPARKS);
const SPARK_BIG = new Uint8Array(SPARKS);
{
  let seed = 11;
  const rnd = (): number => (seed = (seed * 16807) % 2147483647) / 2147483647;
  for (let k = 0; k < SPARKS; k++) {
    SPARK_U[k] = rnd();
    SPARK_V[k] = 0.12 + 0.3 * rnd();
    const j = rnd() * 2 - 1;
    SPARK_J[k] = j * Math.abs(j);
    SPARK_BIG[k] = rnd() < 0.3 ? 1 : 0;
  }
}
// The needles a band's peaks are drawn as: per column a share of the peak, fixed and widely spread, so a
// burst reads as a spray of fine spikes of many heights over the smooth body, never a filled shape.
const NEEDLE = new Float32Array(MAX_N + 1);
{
  let seed = 3;
  for (let j = 0; j <= MAX_N; j++) NEEDLE[j] = 0.14 + 0.86 * ((seed = (seed * 16807) % 2147483647) / 2147483647) ** 1.3;
}
const DUST = 460; // baked specks round the rings, at 1920x1080 (fewer on a smaller window)
const RAYS = 56; // baked rays from the centre
// A ring's tone, and per tone: its needles' alpha and reach (a share of the band's), its body's alpha,
// its core line's, and a soft band under it all (a stopped ring's, which a muted ring's ghost line lacks).
const NONE = 0;
const ICE = 1;
const EMBER = 2;
const REC = 3;
const MUTED = 4;
const STOPPED = 5;
const NEEDLE_A = Float64Array.of(0, 0.86, 0.92, 0.9, 0.5, 0.6);
const NEEDLE_K = Float64Array.of(0, 1, 1, 1, 0.3, 0.9);
const BODY_A = Float64Array.of(0, 0.8, 0.86, 0.86, 0, 0.66);
const CORE_A = Float64Array.of(0, 0.75, 0.9, 0.9, 0.45, 0.45);
const SOFT_A = Float64Array.of(0, 0, 0, 0, 0, 0.36);
// A band's shapes (`band`).
const ENVELOPE = 0;
const BODY = 1;
const TIPS = 2;
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

/** A star's light, `size` px square, made on layout: a soft halo in `color`, a long streak along the
 * ring (the sprite's y, since it is drawn turned to the playhead) and a short one across it, hot in
 * `hot` at the middle. Added to the light; its white-hot core is a `dot` drawn over it. */
function star(color: string, hot: string, size: number, glow: number): HTMLCanvasElement {
  const cv = document.createElement('canvas');
  cv.width = cv.height = size;
  const x = cv.getContext('2d');
  if (!x) return cv;
  const m = size / 2;
  x.globalCompositeOperation = 'screen';
  const halo = x.createRadialGradient(m, m, 0, m, m, m);
  halo.addColorStop(0, withAlpha(color, 0.4 * glow));
  halo.addColorStop(0.1, withAlpha(color, 0.22 * glow));
  halo.addColorStop(0.32, withAlpha(color, 0.07 * glow));
  halo.addColorStop(1, withAlpha(color, 0));
  x.fillStyle = halo;
  x.fillRect(0, 0, size, size);
  const flare = x.createRadialGradient(0, 0, 0, 0, 0, m);
  flare.addColorStop(0, withAlpha(hot, 1));
  flare.addColorStop(0.22, withAlpha(color, 0.4));
  flare.addColorStop(1, withAlpha(color, 0));
  x.fillStyle = flare;
  for (const [sx, sy] of [[0.035, 1], [0.55, 0.03]]) {
    x.setTransform(sx, 0, 0, sy, m, m);
    x.beginPath();
    x.arc(0, 0, m, 0, TAU);
    x.fill();
  }
  x.setTransform(1, 0, 0, 1, 0, 0);
  const core = x.createRadialGradient(m, m, 0, m, m, size * 0.09);
  core.addColorStop(0, withAlpha(hot, 0.8));
  core.addColorStop(1, withAlpha(hot, 0));
  x.fillStyle = core;
  x.fillRect(0, 0, size, size);
  return cv;
}

/** A soft disc `size` px square in `color`, solid at its middle: a star's white-hot core, laid over the
 * light rather than added to it, so it shows its own colour (never warm white) whatever lies under it. */
function dot(color: string, size: number): HTMLCanvasElement {
  const cv = document.createElement('canvas');
  cv.width = cv.height = size;
  const x = cv.getContext('2d');
  if (!x) return cv;
  const m = size / 2;
  const g = x.createRadialGradient(m, m, 0, m, m, m);
  g.addColorStop(0, withAlpha(color, 1));
  g.addColorStop(0.3, withAlpha(color, 0.92));
  g.addColorStop(1, withAlpha(color, 0));
  x.fillStyle = g;
  x.fillRect(0, 0, size, size);
  return cv;
}

/** Lane `i`'s tone: what its band, bloom and star are drawn in. */
function toneOf(i: number): number {
  const kind = feed.kind[i];
  if (kind === 'rec') return REC;
  if (kind === 'dub') return EMBER;
  if (mutedLook(i)) return MUTED;
  return kind === 'play' ? ICE : kind === 'stop' ? STOPPED : NONE;
}

interface Wakes {
  ice: CanvasGradient;
  ember: CanvasGradient;
  ghost: CanvasGradient;
}

function createEmber(): ViewDraw {
  const ground = document.createElement('canvas');
  const gctx = ground.getContext('2d');
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
  const n = new Int32Array(LANES); // columns around each ring
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
  // Each ring's bloom tiles, x, y, w, h in atlas px from its cell's corner: `tiles[tileAt[i]..tileAt[i + 1])`.
  let tiles = new Int32Array(0);
  const tileAt = new Int32Array(LANES + 1);
  const fx = new Int32Array(STRANDS * (K + 1)); // the filament, quarter device px
  const fy = new Int32Array(STRANDS * (K + 1));
  const recAt = new Int32Array(LANES); // a take in flight's head this frame, in turn steps (held to its band, `draw`)
  let dpr = 1;
  let cx = 0;
  let cy = 0;
  let R = 0; // ring 1's radius
  let S = 0; // ring spacing
  let sw = 0; // a band's reach at full scale, each way
  let least = 1; // and at silence
  let reach = 0; // the filament's far end: past the window's farthest corner
  let hair = 1;
  let line = 2;
  let bold = 2;
  let zs = 0; // half a star sprite
  let zf = 0; // half a pen flare
  let zd = 0; // half a star's core, and of the pen's (`zdf`)
  let zdf = 0;
  let glow4 = 4; // the filament's line widths, quarter px
  let mid4 = 4;
  let core4 = 4;
  let spark4 = 4;
  let mote4 = 4;
  let haloW = 2; // the halo's widening of a band before its far blur, atlas px
  let wakeSteps = TURN_STEPS >> 4; // a wake's span: a beat, in turn steps
  let dirty = true;
  let full = true;
  let beats = -1;
  let wakes: Wakes | null = null;
  // Colours and their light, made on layout from the tokens: per tone (`toneOf`) the band, its body,
  // its core line and its bloom.
  let bandC: string[] = [];
  let bodyC: string[] = [];
  let coreC: string[] = [];
  let glowC: string[] = [];
  let ice = '';
  let amber = '';
  let blurNear = 'none';
  let blurFar = 'none';
  let blurSoft = 'none';
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
  // The cores, one per tone (`toneOf`; NONE's is the steel pen's, the ghost tones' are not drawn).
  let dots: HTMLCanvasElement[] = [];

  function layout(s: Surface): void {
    const pal = s.pal;
    dpr = s.dpr;
    // The stack fills the height left above the message line, never more than 0.47 of the width a side.
    const top = 6;
    const bottom = hudPad(s.h) + hudMsg(s.h) * 1.3 + 6;
    const half = Math.max(40, Math.min(0.47 * s.w, (s.h - top - bottom) / 2));
    // Outside ring 1 only its band's reach (0.46 S, 0.08 R) and its bloom.
    const Rcss = half / 1.08;
    R = Math.round(Rcss * dpr);
    S = Math.round(0.175 * R);
    sw = Math.round(0.46 * S);
    least = Math.max(1, Math.round(0.6 * dpr));
    for (let i = 0; i < LANES; i++) {
      r[i] = R - i * S;
      n[i] = clamp(Math.round((TAU * r[i]) / SPACING), 90, MAX_N);
    }
    hair = Math.max(1, Math.round(dpr));
    line = Math.max(1, Math.round(1.5 * dpr));
    bold = Math.max(2, Math.round(2 * dpr));
    cx = Math.round((s.w / 2) * dpr);
    cy = Math.round((top + half) * dpr);
    ground.width = base.width = s.dw;
    ground.height = base.height = s.dh;
    reach = Math.ceil(Math.hypot(Math.max(cx, s.dw - cx), Math.max(cy, s.dh - cy)) + 40 * dpr);

    // The bloom atlas: a cell per ring, wide enough for its widened far blur (the halo), and the tiles
    // that cover the ring's annulus in it (a row of TILE atlas px: one span across, or two where it
    // passes the hole).
    haloW = Math.max(2, Math.round(2.5 * dpr));
    const margin = Math.round(48 * dpr) + DS * haloW;
    let x = 0;
    for (let i = 0; i < LANES; i++) {
      cell[i] = 2 * Math.ceil((r[i] + sw + margin) / DS);
      cellX[i] = x;
      x += cell[i] + 2;
    }
    atlas.width = x;
    atlas.height = cell[0];
    scratch.width = scratch.height = cell[0];
    let rows = 0;
    for (let i = 0; i < LANES; i++) rows += Math.ceil(cell[i] / TILE);
    tiles = new Int32Array(8 * rows);
    let t = 0;
    for (let i = 0; i < LANES; i++) {
      tileAt[i] = t;
      const g = cell[i];
      const m = g / 2;
      const ro = (r[i] + sw + margin) / DS;
      const ri = (r[i] - sw - margin) / DS;
      for (let y0 = 0; y0 < g; y0 += TILE) {
        const y1 = Math.min(g, y0 + TILE);
        const near = y0 <= m && y1 >= m ? 0 : Math.min(Math.abs(y0 - m), Math.abs(y1 - m));
        const far = Math.max(Math.abs(y0 - m), Math.abs(y1 - m));
        if (near >= ro) continue;
        const xo = Math.min(m, Math.ceil(Math.sqrt(ro * ro - near * near)) + 1);
        const xi = ri > 0 && far < ri ? Math.floor(Math.sqrt(ri * ri - far * far)) - 1 : 0;
        if (xi <= 0) {
          tiles.set([m - xo, y0, 2 * xo, y1 - y0], t);
          t += 4;
        } else {
          tiles.set([m - xo, y0, xo - xi, y1 - y0, m + xi, y0, xo - xi, y1 - y0], t);
          t += 8;
        }
      }
    }
    tileAt[LANES] = t;
    blurNear = `blur(${(1.6 * dpr).toFixed(2)}px)`;
    blurFar = `blur(${(4.4 * dpr).toFixed(2)}px)`;
    blurSoft = `blur(${(1.2 * dpr).toFixed(2)}px)`;

    // Ice is the play colour paled toward glacier, still play green to the eye that counts it (the
    // needles palest, the body a shade greener under them); its hot core carries less red than its blue,
    // so the light summed on it whitens and never reaches warm white.
    ice = mix(pal.play, '#ffffff', 0.74);
    amber = pal.dub;
    const steel = mix(pal.text, pal.cyan, 0.45);
    const cold = mix(pal.play, '#38fff0', 0.55); // the light's white-hot core over ice: next to no red
    bandC = ['', ice, amber, pal.rec, pal.faint, pal.dim];
    bodyC = ['', mix(pal.play, '#ffffff', 0.64), mix(amber, '#ffe08a', 0.35), mix(pal.rec, '#ff8a7a', 0.3), pal.faint, pal.dim];
    coreC = ['', mix(pal.play, '#cafff0', 0.85), mix(amber, '#fff2c0', 0.6), mix(pal.rec, '#ffffff', 0.1), pal.faint, pal.text];
    glowC = ['', ice, amber, pal.rec, pal.faint, pal.dim];
    zs = Math.max(10, Math.round(1.1 * S));
    zf = Math.max(14, Math.round(1.7 * S));
    zd = Math.max(3, Math.round(0.07 * S));
    zdf = Math.max(4, Math.round(0.1 * S));
    // The hot light keeps the tone's guard: over ice next to no red, amber and red little blue; the cores
    // are white-hot, each a hair off warm white toward its tone.
    const emberHot = mix(amber, '#fff0a0', 0.6);
    const recHot = mix(pal.rec, '#ffffff', 0.2);
    starIce = star(ice, cold, 2 * zs, 1);
    starEmber = star(amber, emberHot, 2 * zs, 1);
    starRec = star(pal.rec, recHot, 2 * zs, 1);
    starGhost = star(pal.faint, pal.faint, 2 * zs, 0.5);
    flareSteel = star(steel, cold, 2 * zf, 1);
    flareEmber = star(amber, emberHot, 2 * zf, 1);
    flareRec = star(pal.rec, recHot, 2 * zf, 1);
    const iceDot = dot('#e2f8ff', 2 * zdf);
    dots = [iceDot, iceDot, dot('#fff0a0', 2 * zdf), dot('#ffa8a8', 2 * zdf), iceDot, iceDot];
    glow4 = 4 * Math.max(4, Math.round(18 * dpr));
    mid4 = 4 * Math.max(2, Math.round(5 * dpr));
    core4 = 4 * Math.max(1, Math.round(1.6 * dpr));
    spark4 = 4 * Math.max(2, Math.round(2.6 * dpr));
    mote4 = 4 * Math.max(1, Math.round(1.6 * dpr));

    // The ground: the token background, a faint cool haze round the rings and rays from the centre (all
    // of it under the census floor of 40, so it lights no state), the corners sinking to black, and dust:
    // specks mostly just outside ring 1, a few between the rings and in the field.
    const gc = gctx;
    if (gc) {
      let seed = 7;
      const rnd = (): number => (seed = (seed * 16807) % 2147483647) / 2147483647;
      gc.setTransform(1, 0, 0, 1, 0, 0);
      gc.globalCompositeOperation = 'source-over';
      gc.globalAlpha = 1;
      gc.fillStyle = pal.bg;
      gc.fillRect(0, 0, s.dw, s.dh);
      const air = mix(pal.text, ice, 0.3);
      const haze = gc.createRadialGradient(cx, cy, 0.25 * R, cx, cy, 1.5 * R);
      haze.addColorStop(0, withAlpha(air, 0));
      haze.addColorStop(0.5, withAlpha(air, 0.06));
      haze.addColorStop(0.75, withAlpha(air, 0.045));
      haze.addColorStop(1, withAlpha(air, 0));
      gc.fillStyle = haze;
      gc.fillRect(0, 0, s.dw, s.dh);
      // The rays, drawn at the atlas's quarter scale and laid in stretched, which softens them.
      const q = document.createElement('canvas');
      q.width = Math.ceil(s.dw / DS);
      q.height = Math.ceil(s.dh / DS);
      const qc = q.getContext('2d');
      if (qc) {
        const qx = cx / DS;
        const qy = cy / DS;
        const qR = R / DS;
        const fade = qc.createRadialGradient(qx, qy, 0.3 * qR, qx, qy, 1.9 * qR);
        fade.addColorStop(0, withAlpha(air, 0));
        fade.addColorStop(0.45, withAlpha(air, 1));
        fade.addColorStop(0.7, withAlpha(air, 0.5));
        fade.addColorStop(1, withAlpha(air, 0));
        qc.fillStyle = fade;
        for (let k = 0; k < RAYS; k++) {
          const a = TAU * rnd();
          const w = 0.003 + 0.014 * rnd();
          const len = qR * (1.15 + 0.75 * rnd());
          qc.globalAlpha = 0.006 + 0.022 * rnd() ** 2;
          qc.beginPath();
          qc.moveTo(qx, qy);
          qc.lineTo(qx + Math.cos(a - w) * len, qy + Math.sin(a - w) * len);
          qc.lineTo(qx + Math.cos(a + w) * len, qy + Math.sin(a + w) * len);
          qc.closePath();
          qc.fill();
        }
        gc.globalCompositeOperation = 'lighter';
        gc.drawImage(q, 0, 0, q.width * DS, q.height * DS);
        gc.globalCompositeOperation = 'source-over';
      }
      q.width = q.height = 0;
      const vignette = gc.createRadialGradient(cx, cy, 0.5 * Math.max(cx, cy), cx, cy, reach);
      vignette.addColorStop(0, 'rgba(0,0,0,0)');
      vignette.addColorStop(1, 'rgba(0,0,0,0.6)');
      gc.fillStyle = vignette;
      gc.fillRect(0, 0, s.dw, s.dh);
      const pale = mix(pal.text, ice, 0.35);
      const warm = mix(amber, pal.text, 0.3);
      const count = Math.round((DUST * s.w * s.h) / (1920 * 1080));
      for (let k = 0; k < count; k++) {
        const pick = rnd();
        const u = rnd();
        const rho = pick < 0.62 ? R + sw + 1.1 * S * u * u : pick < 0.9 ? r[LANES - 1] + (R - r[LANES - 1]) * u : 1.5 * R * (0.4 + u);
        const a = TAU * rnd();
        const px = Math.round(cx + Math.cos(a) * rho);
        const py = Math.round(cy + Math.sin(a) * rho);
        const big = rnd() < 0.12 ? 2 : 1;
        gc.fillStyle = rnd() < 0.2 ? warm : pale;
        gc.globalAlpha = 0.14 + 0.6 * rnd() ** 3;
        gc.fillRect(px, py, big * hair, big * hair);
      }
      gc.globalAlpha = 1;
    }
    const fc = s.ctx;
    // The filament's light falls off past the stack (built in the quarter-px space it is drawn in).
    const fall = (color: string): CanvasGradient => {
      const g = fc.createRadialGradient(4 * cx, 4 * cy, 0, 4 * cx, 4 * cy, 4 * reach);
      g.addColorStop(0, withAlpha(color, 1));
      g.addColorStop(Math.min(0.95, (R + S) / reach), withAlpha(color, 0.95));
      g.addColorStop(1, withAlpha(color, 0.3));
      return g;
    };
    strandSteel = fall(steel);
    strandEmber = fall(amber);
    strandRec = fall(pal.rec);

    s.countX = s.w / 2;
    s.countY = top + half;
    // Inside ring 5's band (0.22 R) at every size: a digit is about 0.6 em wide and 0.72 em tall.
    s.countSize = Math.min(clamp(0.36 * s.h, 140, 480), 0.5 * Rcss);
    stale.fill(1);
    glowDue.fill(1);
    glowKind.fill('');
    wakes = null;
    dirty = true;
    full = true;
  }

  /** A wake: the lane's light rising from nothing a beat behind the playhead to full at it, drawn
   * turned to a beat before the head (`wakeSteps`), so it runs from 0 to `span`. */
  function wake(c: CanvasRenderingContext2D, color: string, span: number): CanvasGradient {
    const g = c.createConicGradient(0, 0, 0);
    const end = span / TAU;
    g.addColorStop(0, withAlpha(color, 0));
    g.addColorStop(end * 0.55, withAlpha(color, 0.3));
    g.addColorStop(end, withAlpha(color, 1));
    g.addColorStop(Math.min(1, end + 0.0005), withAlpha(color, 0));
    g.addColorStop(1, withAlpha(color, 0));
    return g;
  }

  /**
   * Lane `i`'s band round (x0, y0) at `radius` as the current path, by `shape`: its ENVELOPE (out by the
   * peak, in by the trough), its BODY (out and in by the smoothed level) or its TIPS (where its needles
   * end), at least `floor` each way. A loop that fills its ring closes into an annulus (two loops,
   * opposite ways), so it has no seam. Bake only.
   */
  function band(c: CanvasRenderingContext2D, x0: number, y0: number, radius: number, reachPx: number, i: number, shape: number, floor: number): void {
    const N = n[i];
    const k = cols[i];
    const h = hi[i];
    const l = lo[i];
    const b = body[i];
    const ring = k === N;
    c.beginPath();
    for (let j = 0; j < k; j++) {
      const a = TOP + (TAU * (j + 0.5)) / N;
      const out = radius + Math.max(floor, reachPx * (shape === BODY ? 0.5 * b[j] : shape === TIPS ? h[j] * NEEDLE[j] : h[j]));
      if (j === 0) c.moveTo(x0 + Math.cos(a) * out, y0 + Math.sin(a) * out);
      else c.lineTo(x0 + Math.cos(a) * out, y0 + Math.sin(a) * out);
    }
    if (ring) c.closePath();
    for (let j = k - 1; j >= 0; j--) {
      const a = TOP + (TAU * (j + 0.5)) / N;
      const inn = radius - Math.max(floor, reachPx * (shape === BODY ? 0.42 * b[j] : shape === TIPS ? -l[j] * NEEDLE[MAX_N - j] : -l[j]));
      if (ring && j === k - 1) c.moveTo(x0 + Math.cos(a) * inn, y0 + Math.sin(a) * inn);
      else c.lineTo(x0 + Math.cos(a) * inn, y0 + Math.sin(a) * inn);
    }
    c.closePath();
  }

  /** Lane `i`'s peaks as the current path: per column one radial needle from its trough to its peak,
   * each its own share of them (`NEEDLE`), at least `least` each way. Bake only. */
  function needles(c: CanvasRenderingContext2D, reachPx: number, i: number): void {
    const N = n[i];
    const k = cols[i];
    const h = hi[i];
    const l = lo[i];
    const radius = r[i];
    c.beginPath();
    for (let j = 0; j < k; j++) {
      const a = TOP + (TAU * (j + 0.5)) / N;
      const co = Math.cos(a);
      const si = Math.sin(a);
      const out = radius + Math.max(least, reachPx * h[j] * NEEDLE[j]);
      const inn = radius - Math.max(least, reachPx * -l[j] * NEEDLE[MAX_N - j]);
      c.moveTo(cx + co * inn, cy + si * inn);
      c.lineTo(cx + co * out, cy + si * out);
    }
  }

  /** Sample lane `i` again: its envelope, its body smoothed both ways (a level that swells and ebbs, no
   * attack's edge), the loop's end eased into its start where it fills the ring (no cut at twelve
   * o'clock), a take's start eased in from silence over a long stretch (a taper, no cap or lump). */
  function sample(i: number): void {
    looper.peaksInto(i, peaks);
    const span = stamp.span[i];
    const N = n[i];
    const held = holdsAudio(feed.kind[i]) ? sampleEnvelope(N, span > 0 ? span / PEAK_FRAMES : peaks.count) : 0;
    const h = hi[i];
    const l = lo[i];
    const b = body[i];
    // A softer curve than the level: a loop's quiet stretches still read as a band between its hits.
    for (let k = 0; k < held; k++) {
      h[k] = Math.pow(env.hi[k], 0.7);
      l[k] = -Math.pow(-env.lo[k], 0.7);
      b[k] = Math.pow(env.body[k], 0.7);
    }
    for (let k = 1; k < held; k++) b[k] = 0.18 * b[k] + 0.82 * b[k - 1];
    for (let k = held - 2; k >= 0; k--) b[k] = 0.18 * b[k] + 0.82 * b[k + 1];
    cols[i] = held;
    stale[i] = 0;
    if (held === N) {
      const m = Math.min(40, held >> 3);
      for (let q = 0; q < m; q++) {
        const w = 0.5 * (1 - q / m);
        const e = N - 1 - q;
        for (const arr of [h, l, b]) {
          const s0 = arr[q];
          arr[q] += (arr[e] - s0) * w;
          arr[e] += (s0 - arr[e]) * w;
        }
      }
    } else {
      const m = Math.min(80, held >> 2);
      for (let q = 0; q < m; q++) {
        const f = ((q + 1) / (m + 1)) ** 2;
        h[q] *= f;
        l[q] *= f;
        b[q] *= f;
      }
    }
  }

  /** Lane `i` on the bitmap: the hairline or the tape where it holds no audio, and its band: a stopped
   * ring's soft envelope, the needles, the body over them, a core line (a muted ring: short needles and
   * its line, a ghost). */
  function paintLane(c: CanvasRenderingContext2D, pal: Palette, i: number, master: number): void {
    const kind = feed.kind[i];
    if (stale[i]) sample(i);
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
      const t = toneOf(i);
      if (SOFT_A[t] > 0) {
        c.fillStyle = bandC[t];
        c.globalAlpha = SOFT_A[t];
        c.filter = blurSoft;
        band(c, cx, cy, r[i], 0.8 * sw, i, ENVELOPE, least);
        c.fill();
        c.filter = 'none';
      }
      c.strokeStyle = bandC[t];
      c.globalAlpha = NEEDLE_A[t];
      c.lineWidth = hair;
      needles(c, NEEDLE_K[t] * sw, i);
      c.stroke();
      if (BODY_A[t] > 0) {
        c.fillStyle = bodyC[t];
        c.globalAlpha = BODY_A[t];
        band(c, cx, cy, r[i], sw, i, BODY, least);
        c.fill();
      }
      c.strokeStyle = coreC[t];
      c.globalAlpha = CORE_A[t];
      c.lineWidth = hair;
      c.beginPath();
      c.arc(cx, cy, r[i], TOP, end);
      c.stroke();
    }
  }

  /**
   * Redraw the bitmap. `full` (a layout): the ground and every ring. Otherwise only the rings whose form
   * changed, each in its own annulus (half a spacing each way, which its band never leaves), over the
   * ground.
   */
  function rasterise(pal: Palette): void {
    const c = bctx;
    if (!c) return;
    const master = looper.masterFramesValue();
    c.setTransform(1, 0, 0, 1, 0, 0);
    c.globalCompositeOperation = 'source-over';
    c.globalAlpha = 1;
    if (full) {
      c.drawImage(ground, 0, 0);
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
        c.globalAlpha = 1;
        c.drawImage(ground, 0, 0);
        paintLane(c, pal, i, master);
        c.restore();
      }
    }
    c.globalAlpha = 1;
  }

  /** Lane `i`'s bloom cell: its envelope at a quarter scale in its tone's colour, blurred near, then
   * widened and blurred far (the halo), then cut back where the needles stand, so the bloom glows round
   * the band and does not wash it white. A ghost ring (muted, stopped) has none: never blitted,
   * never baked. */
  function bakeGlow(i: number): void {
    const a = actx;
    const x = sctx;
    glowDue[i] = 0;
    glowKind[i] = feed.kind[i];
    glowMuted[i] = feed.muted[i];
    const t = toneOf(i);
    if (!a || !x || t === MUTED || t === STOPPED) return;
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
      x.fillStyle = glowC[t];
      band(x, m, m, r[i] / DS, sw / DS, i, ENVELOPE, 0.5 * dpr);
      x.fill();
      a.filter = blurNear;
      a.globalAlpha = 0.7;
      a.drawImage(scratch, 0, 0, g, g, cellX[i], 0, g, g);
      x.strokeStyle = glowC[t];
      x.lineWidth = haloW;
      x.lineJoin = 'round';
      x.stroke();
      a.filter = blurFar;
      a.globalCompositeOperation = 'screen';
      a.globalAlpha = 1;
      a.drawImage(scratch, 0, 0, g, g, cellX[i], 0, g, g);
      // The cut: where the needles stand, just inside their tips (at this scale their spread, so the
      // bloom thins in among the tips and opens no dark wedge round a burst, nor a seam at its edge).
      x.clearRect(0, 0, g, g);
      x.fillStyle = '#000';
      band(x, m, m, r[i] / DS, (0.92 * sw) / DS, i, TIPS, 0.3 * dpr);
      x.fill();
      a.filter = 'none';
      a.globalCompositeOperation = 'destination-out';
      a.globalAlpha = 0.92;
      a.drawImage(scratch, 0, 0, g, g, cellX[i], 0, g, g);
    }
    a.restore();
  }

  /** The filament from beyond the window to the pen on ring `p` at `step`, into `fx`/`fy` (quarter
   * device px): four strands braided round a curve that leaves the head outward and trails behind it,
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

  /** The dust, drifting in along the filament to ring `p`'s pen at `step` (quarter px): a cloud round
   * the stream, slowing and gathering as it nears the pen, brightest there. */
  function sparks(c: CanvasRenderingContext2D, p: number, step: number): void {
    const t = mem[SEC];
    const sway = mem[SWAY];
    const v = mem[GLOW];
    const th = TOP + (TAU * step) / TURN_STEPS;
    const r0 = r[p];
    const span = reach - r0;
    for (let k = 0; k < SPARKS; k++) {
      let w = SPARK_U[k] - t * SPARK_V[k];
      w -= Math.floor(w);
      const u = w * w;
      const j = k % STRANDS;
      const rho = r0 + span * u;
      const phi = th - SWEEP * u * u;
      const co = Math.cos(phi);
      const si = Math.sin(phi);
      const taper = (u < 0.16 ? u / 0.16 : 1) * sway;
      const d = taper * (0.55 * Math.sin(3.1 * u + 0.9 * t) + SHARE[j] * Math.sin(TAU * WAVE[j] * u + SPEED[j] * t + PHASE[j])) + SPARK_J[k] * (4 + 150 * u) * dpr;
      const z = SPARK_BIG[k] ? spark4 : mote4;
      c.globalAlpha = A[(v * (1.2 - 0.8 * u) * 255) | 0];
      c.fillRect((((cx + co * rho - si * d) * 4) | 0) - (z >> 1), (((cy + si * rho + co * d) * 4) | 0) - (z >> 1), z, z);
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
    if (feed.beatsPerLoop !== beats) {
      beats = feed.beatsPerLoop;
      wakeSteps = Math.round(TURN_STEPS / (beats > 0 ? beats : 16));
      wakes = null;
    }
    if (dirty) {
      rasterise(pal);
      dirty = false;
    }
    for (let i = 0; i < LANES; i++) {
      if (glowDue[i] && (feed.kind[i] !== glowKind[i] || feed.muted[i] !== glowMuted[i] || now - glowAt[i] >= GLOW_MS)) {
        glowAt[i] = now;
        bakeGlow(i);
      }
    }
    if (!wakes) {
      const span = SPANS[wakeSteps];
      wakes = { ice: wake(c, ice, span), ember: wake(c, amber, span), ghost: wake(c, pal.faint, span) };
    }

    const still = feed.reduced;
    const dim = light.dim;
    const move = light.move;
    const turn = (light.phase * TURN_STEPS) | 0;
    const beat = still ? 0 : decay(now, light.beatAt, 280) * (light.beatBar ? 1 : 0.6);
    mem[SEC] = now / 1000;
    // A take in flight's head: the engine's, held to the end of the band drawn for it (the take's
    // newest peaks come in deliveries and the head runs on between them), so its star and the pen sit
    // on the take, never past it.
    for (let i = 0; i < LANES; i++) {
      if (feed.kind[i] !== 'rec') continue;
      const at = looper.recHeadFrac(i);
      const head = at >= 0 ? (at * TURN_STEPS) | 0 : master > 0 ? turn : 0;
      const end = ((cols[i] * TURN_STEPS) / n[i]) | 0;
      recAt[i] = head < end ? head : end;
    }

    c.setTransform(1, 0, 0, 1, 0, 0);
    c.globalCompositeOperation = 'source-over';
    if (dim < 0.995) {
      c.globalAlpha = 1;
      c.fillStyle = pal.bg;
      c.fillRect(0, 0, s.dw, s.dh);
    }
    c.globalAlpha = A[(dim * 255) | 0];
    c.drawImage(base, 0, 0);

    // Light: each sounding ring's bloom at its own level (a fixed share of it under reduced motion),
    // over its annulus only.
    c.globalCompositeOperation = 'screen';
    for (let i = 0; i < LANES; i++) {
      const g = dim * light.lit[i];
      // A ring that goes silent (muted, stopped) loses its bloom at once: a ghost does not glow.
      if (g <= 0.004 || cols[i] === 0 || mutedLook(i) || feed.kind[i] === 'stop') continue;
      const L = still ? 0.6 : light.amp[i] * (feed.fading[i] ? 0.5 : 1);
      c.globalAlpha = A[(g * (0.4 + 0.6 * L) * 255) | 0];
      const ox = cx - ((cell[i] * DS) >> 1);
      const oy = cy - ((cell[i] * DS) >> 1);
      const sx = cellX[i];
      for (let t = tileAt[i]; t < tileAt[i + 1]; t += 4) {
        c.drawImage(atlas, sx + tiles[t], tiles[t + 1], tiles[t + 2], tiles[t + 3], ox + tiles[t] * DS, oy + tiles[t + 1] * DS, tiles[t + 2] * DS, tiles[t + 3] * DS);
      }
    }

    // The playhead: a wake behind it on every ring that plays, a star on each.
    if (master > 0 && move > 0.004) {
      if (!still) {
        turnTo(c, cx, cy, (turn - wakeSteps + TURN_STEPS) % TURN_STEPS);
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
          c.arc(0, 0, r[i], 0, SPANS[wakeSteps]);
          c.stroke();
        }
      }
      turnTo(c, cx, cy, turn);
      for (let i = 0; i < LANES; i++) {
        const kind = feed.kind[i];
        if (kind !== 'play' && kind !== 'dub') continue;
        const muted = mutedLook(i);
        const a = dim * move * (muted ? 0.3 : (0.75 + 0.25 * light.amp[i]) * (1 + 0.3 * beat));
        c.globalAlpha = A[(a * 255) | 0];
        c.drawImage(muted ? starGhost : kind === 'dub' ? starEmber : starIce, r[i] - zs, -zs);
      }
      c.setTransform(1, 0, 0, 1, 0, 0);
    }
    // A take in flight has its own head, where its band ends (a first take's runs around its ring).
    for (let i = 0; i < LANES; i++) {
      if (feed.kind[i] !== 'rec') continue;
      turnTo(c, cx, cy, recAt[i]);
      c.globalAlpha = A[(dim * (0.75 + 0.25 * beat) * 255) | 0];
      c.drawImage(starRec, r[i] - zs, -zs);
      c.setTransform(1, 0, 0, 1, 0, 0);
    }

    // The instrument: the filament to the pen (the write head, else the lane about to write, else the
    // selected lane, under the playhead), as bright as the input; red for a moment on a clip.
    const v0 = still ? 0 : dim * Math.pow(light.input, 0.7);
    let pen = -1; // the ring the filament lands on, its tone, where, and how bright
    let penTone = NONE;
    let penStep = 0;
    let penA = 0;
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
      if (!writes) {
        if (own === 'armed' || own === 'listening') p = feed.selected;
        else {
          for (let i = 0; i < LANES; i++) {
            const kind = feed.kind[i];
            if (kind === 'armed' || kind === 'listening') {
              p = i;
              break;
            }
          }
        }
        if (p < 0) p = feed.selected;
      }
      const kind = feed.kind[p];
      const step = kind === 'rec' ? recAt[p] : turn;
      const v1 = (writes ? 1.6 : kind === 'armed' || kind === 'listening' ? 1.5 : 1.2) * v0;
      const v = v1 > 1 ? 1 : v1;
      mem[SWAY] = (0.3 + 0.7 * light.inputGeo) * 0.85 * S;
      const red = light.clip || kind === 'rec';
      const grad = red ? strandRec : writes ? strandEmber : strandSteel;
      strandPath(p, step);
      c.setTransform(0.25, 0, 0, 0.25, 0, 0);
      if (grad) {
        c.strokeStyle = grad;
        c.fillStyle = grad;
      }
      c.lineJoin = 'round';
      c.lineWidth = glow4;
      for (let j = 0; j < STRANDS; j++) {
        c.globalAlpha = A[(0.11 * v * 255) | 0];
        strandStroke(c, j);
      }
      c.lineWidth = mid4;
      for (let j = 0; j < STRANDS; j++) {
        c.globalAlpha = A[(0.24 * v * 255) | 0];
        strandStroke(c, j);
      }
      c.lineWidth = core4;
      for (let j = 0; j < STRANDS; j++) {
        c.globalAlpha = A[(v * (0.85 - 0.12 * j) * 255) | 0];
        strandStroke(c, j);
      }
      c.lineJoin = 'miter';
      mem[GLOW] = v;
      sparks(c, p, step);
      turnTo(c, cx, cy, step);
      penA = v * (0.75 + 0.25 * beat);
      c.globalAlpha = A[(penA * 255) | 0];
      c.drawImage(red ? flareRec : writes ? flareEmber : flareSteel, r[p] - zf, -zf);
      c.setTransform(1, 0, 0, 1, 0, 0);
      pen = p;
      penTone = red ? REC : writes ? EMBER : NONE;
      penStep = step;
    }
    c.globalCompositeOperation = 'source-over';

    // The white-hot cores, laid over the light: each star's, a take in flight's, the pen's.
    if (master > 0 && move > 0.004) {
      turnTo(c, cx, cy, turn);
      for (let i = 0; i < LANES; i++) {
        const kind = feed.kind[i];
        if ((kind !== 'play' && kind !== 'dub') || mutedLook(i)) continue;
        c.globalAlpha = A[(dim * move * 255) | 0];
        c.drawImage(dots[kind === 'dub' ? EMBER : ICE], r[i] - zd, -zd, 2 * zd, 2 * zd);
      }
      c.setTransform(1, 0, 0, 1, 0, 0);
    }
    for (let i = 0; i < LANES; i++) {
      if (feed.kind[i] !== 'rec') continue;
      turnTo(c, cx, cy, recAt[i]);
      c.globalAlpha = A[(dim * 255) | 0];
      c.drawImage(dots[REC], r[i] - zd, -zd, 2 * zd, 2 * zd);
      c.setTransform(1, 0, 0, 1, 0, 0);
    }
    if (pen >= 0) {
      turnTo(c, cx, cy, penStep);
      c.globalAlpha = A[(penA * 255) | 0];
      c.drawImage(dots[penTone], r[pen] - zdf, -zdf);
      c.setTransform(1, 0, 0, 1, 0, 0);
    }

    // Per lane: an overdub's thread (the loop's ice, just inside its band's middle), the armed or
    // listening ring, a pending loop-end stop, a refusal's flash.
    for (let i = 0; i < LANES; i++) {
      const kind = feed.kind[i];
      if (kind === 'dub') {
        c.strokeStyle = bodyC[ICE];
        c.globalAlpha = A[(0.85 * dim * 255) | 0];
        c.lineWidth = line;
        ring(c, r[i] - bold - line);
      }
      if (kind === 'armed' || kind === 'listening') {
        const breathe = still ? 0.45 : kind === 'armed' ? 0.3 + 0.35 * decay(now, light.beatAt, 300) : 0.4 + 0.12 * Math.sin((TAU * now) / 1200);
        c.strokeStyle = waitColor(pal, kind);
        c.globalAlpha = A[(dim * breathe * 255) | 0];
        c.lineWidth = line;
        ring(c, r[i]);
      }
      if (feed.stopping[i] && !feed.fading[i] && master > 0) {
        // A stop waits for the loop end: the stretch still to play is traced just outside the band.
        turnTo(c, cx, cy, turn);
        c.strokeStyle = pal.dim;
        c.globalAlpha = 0.35;
        c.lineWidth = hair;
        c.beginPath();
        c.arc(0, 0, r[i] + sw + bold, 0, SPANS[TURN_STEPS - turn]);
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

    // The selected lane: a warm-white rail at its band's inner fringe (off its middle, whose light it
    // would wash), moved with no transition.
    c.strokeStyle = pal.engaged;
    c.globalAlpha = 0.9;
    c.lineWidth = line;
    ring(c, r[feed.selected] - (sw >> 1));
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
      for (const cv of [ground, base, atlas, scratch, starIce, starEmber, starRec, starGhost, flareSteel, flareEmber, flareRec, ...dots]) cv.width = cv.height = 0;
    },
  };
}

export const ember: StageViewDef = { id: 'ember', name: 'EMBER', create: createEmber };
