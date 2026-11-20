/**
 * The wave's looks: the one switch over how `waveform.ts` draws a take, and the three looks beside its
 * own smooth body (`plain`, drawn there). The wave is a half-open point (the owner keeps several on the
 * table and judges them on his own takes), so the app draws all four behind one key (`transport-keys.ts`)
 * and keeps the pick in `localStorage`; nothing here changes what a state looks like, only how a take's
 * level is drawn:
 *
 * - `stripes` (the default): stripes on the musical grid, the meter's beats a bar times a power of two
 *   per bar, the count whose pitch lands nearest `STRIPE_PITCH` CSS px, so every bar and beat line falls
 *   in a gap in any meter (3/4, 6/8 too); each stripe
 *   `STRIPE_DUTY` of its pitch, as tall as the peak of its slice, square-ended, mirrored on the centre
 *   line and snapped to device px, a tick for a silent slice; one vertical gradient per lane, full colour
 *   on the centre line easing along a smoothstep toward the glass at the tallest reach. No trail.
 * - `dense`: fine rounded bars on a 3 CSS px pitch, the envelope smoothed, each bar its own light: a
 *   bright core over the level falling off to a soft dim tip at the peak; a short trail burns behind the
 *   playhead.
 * - `bars`: light bars, a rounded bar on a 5 CSS px pitch, the dim peak with the bright level inside it.
 *
 * No element reaches past the recorded boundary (`WaveInput.end`): a slice the take only partly covers
 * is sampled over what it holds, and its element drawn once it fits inside the take (`fit`).
 *
 * Every look draws the part to come the same take turned down (`PART_AHEAD`) and the part already played
 * lit (`PART_ON`); `waveform.ts` keeps both bitmaps and shows each on its side of the playhead, the cut
 * snapped so the element under the head counts as played (`cutAt`). An overdub's played part shows the
 * layer being laid (`WaveInput.nu`) as a warm core where the look has one, never the mixed level: audio
 * the dub left unchanged keeps the state's own colour.
 *
 * The feed carries only min/max bins of `PEAK_FRAMES` frames (no RMS): a slice's peak is the largest
 * |min|/|max| of the bins it covers, its level the body's level (`waveform.ts` `level`). A slice narrower
 * than a bin takes the monotone cubic through the bin centres at its own centre, as the plain body's
 * columns do, so a short bar at a fast tempo keeps its stripe pitch and never stair-steps.
 *
 * Plain arithmetic and canvas calls only: `waveform.ts` calls this from its cached-bitmap path, never
 * per frame, and it reads no signal (invariant 6; `verify/guards/stage-draw.mjs` follows this import).
 */

export type WaveLook = 'stripes' | 'dense' | 'bars' | 'plain';
const LOOKS: readonly WaveLook[] = ['stripes', 'dense', 'bars', 'plain'];
/** A look as its toast names it. */
const NAMES: Readonly<Record<WaveLook, string>> = { stripes: 'Stripes', dense: 'Dense', bars: 'Light bars', plain: 'Plain' };
const LOOK_KEY = 'lf.waveLook';

function storedLook(): WaveLook {
  try {
    const v = localStorage.getItem(LOOK_KEY);
    return LOOKS.find((l) => l === v) ?? 'stripes';
  } catch {
    return 'stripes';
  }
}
let look: WaveLook = storedLook();

/** The look every lane draws now: a plain read, so the draw loop's dirty check may call it. */
export function waveLook(): WaveLook {
  return look;
}

/** Step to the next look, wrapping, and keep it; returns its name for the toast. Every lane redraws on
 * its next frame (its dirty check sees the look change). */
export function cycleWaveLook(): string {
  look = LOOKS[(LOOKS.indexOf(look) + 1) % LOOKS.length];
  try {
    localStorage.setItem(LOOK_KEY, look);
  } catch {
    // Storage full or off: the look holds for this session.
  }
  return NAMES[look];
}

/** What a take is drawn as: its state's tone. */
export const TONE_PLAY = 0;
export const TONE_REC = 1;
export const TONE_DUB = 2;
export const TONE_STOPPED = 3;
export const TONE_MUTED = 4;
/** Which bitmap: the part played, the part to come, the dense look's trail at full burn. */
export const PART_ON = 0;
export const PART_AHEAD = 1;
export const PART_HOT = 2;

/** The colour tokens as packed 0xRRGGBB (`rgbOf`, read once per lane mount). */
export interface WaveRGB {
  play: number;
  rec: number;
  dub: number;
  dim: number;
  glass: number;
}

/**
 * One lane's take as the looks read it, filled by `waveform.ts` `rasterise` (one object per lane, reused):
 * the bins on the display's scale and the cubic's slopes (`waveform.ts` `slopes`, stride `z`), the
 * device size, and per-slice scratch this module owns.
 */
export interface WaveInput {
  dw: number;
  dh: number;
  dpr: number;
  /** The loudest peak's reach from the centre line, device px. */
  A: number;
  /** Bins held, and bins across the full width (a recording take's span in bins, else `count`). */
  count: number;
  across: number;
  hs: Float32Array;
  ls: Float32Array;
  lv: Float64Array;
  sl: Float32Array;
  z: number;
  /** While a layer sums: per bin, the level the layer adds over the loop as the dub began (display
   * scale); else null. */
  nu: Float64Array | null;
  /** The grid's bar count across the width (0: no loop yet, the stripes run free), and the meter's
   * counted beats a bar (`waveform.ts` `setGridBeats`; 6/8 counts two). */
  bars: number;
  beats: number;
  /** The recorded boundary, device px: no element reaches past it (a take that records ends short of the
   * width; else `dw`). */
  end: number;
  rgb: WaveRGB;
  /** Per slice, filled by `prepare`: the peak, the level, the layer's level (display scale). */
  pk: Float32Array;
  rm: Float32Array;
  nr: Float32Array;
  tmp: Float32Array;
  /** The look's element geometry, device px (`prepare`): pitch, and the first element's left edge. */
  pitch: number;
  width: number;
  /** The cut (`cutAt`): element j's left edge is round(j·pitch + co); it is played once j·pitch + co + cq
   * is left of the head. */
  co: number;
  cq: number;
  /** Gradient caches: the stripes' (lane-tall, rebuilt when `gradH` is not the lane's height) and the
   * dense bars' (unit height, scaled per bar, never stale). */
  stripeGrads: (CanvasGradient | undefined)[];
  gradH: number;
  denseGrads: (CanvasGradient | undefined)[];
}

export function waveInput(rgb: WaveRGB): WaveInput {
  return {
    dw: 0,
    dh: 0,
    dpr: 1,
    A: 0,
    count: 0,
    across: 0,
    hs: new Float32Array(0),
    ls: new Float32Array(0),
    lv: new Float64Array(0),
    sl: new Float32Array(0),
    z: 0,
    nu: null,
    bars: 0,
    beats: 4,
    end: 0,
    rgb,
    pk: new Float32Array(0),
    rm: new Float32Array(0),
    nr: new Float32Array(0),
    tmp: new Float32Array(0),
    pitch: 1,
    width: 1,
    co: 0,
    cq: 0,
    stripeGrads: [],
    gradH: -1,
    denseGrads: [],
  };
}

// ── colour ──────────────────────────────────────────────────────────────────────────────────────────

let probe: CanvasRenderingContext2D | null = null;
/** A CSS colour as packed 0xRRGGBB, through a 1x1 canvas (a token may be any CSS colour form); `fallback`
 * when no canvas. */
export function rgbOf(css: string, fallback: number): number {
  if (!probe) {
    const c = document.createElement('canvas');
    c.width = c.height = 1;
    probe = c.getContext('2d', { willReadFrequently: true });
    if (!probe) return fallback;
  }
  probe.clearRect(0, 0, 1, 1);
  probe.fillStyle = '#000';
  probe.fillStyle = css;
  probe.fillRect(0, 0, 1, 1);
  const d = probe.getImageData(0, 0, 1, 1).data;
  return (d[0] << 16) | (d[1] << 8) | d[2];
}

/** `c` moved a share `t` of the way to `to`. */
function mix(c: number, t: number, to: number): number {
  const r = (c >> 16) & 255;
  const g = (c >> 8) & 255;
  const b = c & 255;
  return (
    (Math.round(r + (((to >> 16) & 255) - r) * t) << 16) |
    (Math.round(g + (((to >> 8) & 255) - g) * t) << 8) |
    Math.round(b + ((to & 255) - b) * t)
  );
}
const WHITE = 0xffffff;
function css(c: number, a = 1): string {
  return `rgba(${(c >> 16) & 255},${(c >> 8) & 255},${c & 255},${a})`;
}

/** The stripes' and dense bars' warm white for the layer being laid, and the light bars' warm amber. */
const HOT = 0xffecc8;
const BARS_HOT = 0xffc868;

// ── geometry and slices ─────────────────────────────────────────────────────────────────────────────

/** The stripes: their pitch aimed at in CSS px, a stripe's share of it, the gradient's step toward the
 * glass at the tallest reach (the layer's core a smaller one). */
const STRIPE_PITCH = 8.5;
const STRIPE_DUTY = 0.46;
const STRIPE_K = 0.66;
const STRIPE_HOT_K = 0.22;
/** The dense bars' and the light bars' pitch, CSS px. */
const DENSE_PITCH = 3;
const BARS_PITCH = 5;
const BARS_WIDTH = 3;

/** Per-slice scratch at least `n` long. */
function room(w: WaveInput, n: number): void {
  if (w.pk.length >= n) return;
  const size = Math.max(256, n, 2 * w.pk.length);
  w.pk = new Float32Array(size);
  w.rm = new Float32Array(size);
  w.nr = new Float32Array(size);
  w.tmp = new Float32Array(size);
}

/** The cubic Hermite between `y1` and `y2` (as `waveform.ts` `hermite`). */
function hermite(y1: number, y2: number, m1: number, m2: number, f: number): number {
  const f2 = f * f;
  const f3 = f2 * f;
  return (2 * f3 - 3 * f2 + 1) * y1 + (f3 - 2 * f2 + f) * m1 + (3 * f2 - 2 * f3) * y2 + (f3 - f2) * m2;
}

/**
 * Sample `n` slices `p` device px wide from the left edge into `pk`/`rm`/`nr`. Returns the slices that
 * hold the take (a recording take's region ends where its bins do).
 */
function sample(w: WaveInput, n: number, p: number): number {
  room(w, n);
  const { count, hs, ls, lv, sl, z, nu, pk, rm, nr } = w;
  const last = count - 1;
  const bp = (p / w.dw) * w.across; // bins per slice
  let drawn = 0;
  for (let j = 0; j < n; j++) {
    const b0 = j * bp;
    if (b0 >= count) break;
    let peak = 0;
    let level = 0;
    let layer = 0;
    if (bp >= 1 || last === 0) {
      const p0 = Math.floor(b0);
      let p1 = Math.ceil(b0 + bp);
      if (p1 <= p0) p1 = p0 + 1;
      if (p1 > count) p1 = count;
      for (let q = p0; q < p1; q++) {
        const a = hs[q] > -ls[q] ? hs[q] : -ls[q];
        if (a > peak) peak = a;
        level += lv[q];
        if (nu) layer += nu[q];
      }
      level /= p1 - p0;
      layer /= p1 - p0;
    } else {
      // The slice's centre in bin-centre units, held to the take: the cubic, as the plain body's columns.
      let t = b0 + bp / 2 - 0.5;
      if (t < 0) t = 0;
      else if (t > last) t = last;
      const i = t >= last ? last - 1 : Math.floor(t);
      const f = t - i;
      const hi = hermite(hs[i], hs[i + 1], sl[z + i], sl[i + 1], f);
      const lo = hermite(ls[i], ls[i + 1], sl[3 * z + i], sl[2 * z + i + 1], f);
      peak = hi > -lo ? hi : -lo;
      if (peak < 0) peak = 0;
      level = hermite(lv[i], lv[i + 1], sl[5 * z + i], sl[4 * z + i + 1], f);
      if (nu) layer = nu[i] + (nu[i + 1] - nu[i]) * f;
    }
    pk[j] = peak;
    rm[j] = level < 0 ? 0 : level < peak ? level : peak;
    nr[j] = layer < 0 ? 0 : layer;
    drawn = j + 1;
  }
  return drawn;
}

/** Of the first `n` elements, those whose right edge lies at or before the recorded boundary (`end`): a
 * slice the take only partly covers is sampled over what it holds, and its element waits until it fits. */
function fit(w: WaveInput, n: number): number {
  while (n > 0 && Math.round((n - 1) * w.pitch + w.co) + w.width > w.end) n--;
  return n;
}

/** The dense look's smoothing: a gaussian of σ 1.4 slices (radius 5), normalised. */
const GAUSS = (() => {
  const k = new Float32Array(11);
  let sum = 0;
  for (let i = -5; i <= 5; i++) sum += k[i + 5] = Math.exp(-(i * i) / (2 * 1.4 * 1.4));
  for (let i = 0; i < 11; i++) k[i] /= sum;
  return k;
})();

/** Gaussian-smooth `a`'s first `n` values in place (through `tmp`), the ends held. */
function smooth(a: Float32Array, tmp: Float32Array, n: number): void {
  for (let i = 0; i < n; i++) tmp[i] = a[i];
  const last = n - 1;
  for (let i = 0; i < n; i++) {
    let s = 0;
    for (let j = -5; j <= 5; j++) {
      const q = i + j;
      s += tmp[q < 0 ? 0 : q > last ? last : q] * GAUSS[j + 5];
    }
    a[i] = s;
  }
}

/**
 * Lay out `look`'s elements over the lane and sample their slices. Returns the elements that hold the
 * take. Sets the geometry `cutAt` and `paint` read.
 */
export function prepare(lk: WaveLook, w: WaveInput): number {
  const { dw, dpr } = w;
  if (lk === 'stripes') {
    // The meter's beats a bar times a power of two, the count whose pitch lands nearest STRIPE_PITCH and
    // never under one a beat: every beat line then falls between two stripes (a 3/4 bar of 4 stripes
    // would put its beat lines under the second and third). With no loop yet (no bars), stripes at that
    // pitch across the width.
    let N: number;
    if (w.bars > 0) {
      const bar = dw / dpr / w.bars;
      let k = w.beats > 0 ? w.beats : 1;
      while (Math.abs(bar / (2 * k) - STRIPE_PITCH) < Math.abs(bar / k - STRIPE_PITCH)) k *= 2;
      N = k * w.bars;
    } else N = Math.max(1, Math.round(dw / dpr / STRIPE_PITCH));
    const p = dw / N;
    const sw = Math.max(1, Math.round(p * STRIPE_DUTY));
    w.pitch = p;
    w.width = sw;
    w.co = p / 2 - sw / 2;
    w.cq = sw / 2;
    return fit(w, sample(w, N, p));
  }
  if (lk === 'dense') {
    const P = Math.max(2, Math.round(DENSE_PITCH * dpr));
    const bw = Math.max(1, Math.round((P * 2) / 3));
    w.pitch = P;
    w.width = bw;
    w.co = 0;
    w.cq = bw;
    const n = fit(w, sample(w, Math.ceil(dw / P), P));
    // The envelope smoothed so no bar stands alone: each peak the max of it and its neighbours, then a
    // gaussian; the level and the layer the gaussian alone, never above the peak.
    const { pk, rm, nr, tmp } = w;
    for (let i = 0; i < n; i++) {
      const a = pk[i];
      const l = i > 0 ? pk[i - 1] : 0;
      const r = i + 1 < n ? pk[i + 1] : 0;
      tmp[i] = a > l ? (a > r ? a : r) : l > r ? l : r;
    }
    for (let i = 0; i < n; i++) pk[i] = tmp[i];
    smooth(pk, tmp, n);
    smooth(rm, tmp, n);
    if (w.nu) smooth(nr, tmp, n);
    for (let i = 0; i < n; i++) if (rm[i] > pk[i]) rm[i] = pk[i];
    return n;
  }
  // bars
  const P = Math.max(3, Math.round(BARS_PITCH * dpr));
  const lw = Math.max(2, Math.round(BARS_WIDTH * dpr));
  w.pitch = P;
  w.width = lw;
  w.co = P / 2 - lw / 2;
  w.cq = lw / 2;
  return fit(w, sample(w, Math.ceil(dw / P), P));
}

/** Where the played part ends for a head at `x` (device px): the left edge of the first element not yet
 * played, so the element under the head counts as played and the cut falls in a gap. */
export function cutAt(w: WaveInput, x: number): number {
  let j = Math.ceil((x - w.co - w.cq) / w.pitch);
  if (j < 0) j = 0;
  const c = Math.round(j * w.pitch + w.co);
  return c < 0 ? 0 : c > w.dw ? w.dw : c;
}

// ── the looks ───────────────────────────────────────────────────────────────────────────────────────

/** Draw `look`'s first `n` elements into `g` (device px, identity transform) as `tone`'s `part`. */
export function paint(lk: WaveLook, w: WaveInput, g: CanvasRenderingContext2D, n: number, tone: number, part: number): void {
  if (n < 1) return;
  if (lk === 'stripes') paintStripes(w, g, n, tone, part);
  else if (lk === 'dense') paintDense(w, g, n, tone, part);
  else paintBars(w, g, n, tone, part);
  g.setTransform(1, 0, 0, 1, 0, 0);
}

/** The stripes' colour on the centre line for `tone`'s `part`: the part to come the same colour mixed
 * toward the glass, a solid, never a see-through; silent tones grey, muted a step under stopped. */
function stripeTone(w: WaveInput, tone: number, part: number): number {
  const { rgb } = w;
  const down = (c: number, t: number): number => mix(c, t, rgb.glass);
  const ahead = part === PART_AHEAD;
  switch (tone) {
    case TONE_PLAY:
      return ahead ? down(rgb.play, 0.58) : mix(rgb.play, 0.04, WHITE);
    case TONE_DUB:
      return ahead ? down(rgb.dub, 0.55) : rgb.dub;
    case TONE_REC:
      return rgb.rec;
    case TONE_STOPPED:
      return down(rgb.dim, 0.48);
    default:
      return ahead ? down(down(rgb.dim, 0.64), 0.34) : down(rgb.dim, 0.64);
  }
}

/** One lane-tall gradient: `c` on the centre line, moved `k` toward the glass at the tallest reach along a
 * smoothstep, in 24 steps (no band). Cached per slot until the lane's height changes. */
function stripeGrad(w: WaveInput, g: CanvasRenderingContext2D, slot: number, c: number, k: number): CanvasGradient {
  if (w.gradH !== w.dh) {
    w.stripeGrads.length = 0;
    w.gradH = w.dh;
  }
  let gr = w.stripeGrads[slot];
  if (gr) return gr;
  const mid = Math.round(w.dh / 2);
  gr = g.createLinearGradient(0, mid - w.A, 0, mid + w.A);
  for (let i = 0; i <= 24; i++) {
    const t = i / 24;
    const u = Math.abs(t - 0.5) * 2;
    gr.addColorStop(t, css(mix(c, k * u * u * (3 - 2 * u), w.rgb.glass)));
  }
  w.stripeGrads[slot] = gr;
  return gr;
}

function paintStripes(w: WaveInput, g: CanvasRenderingContext2D, n: number, tone: number, part: number): void {
  const { pk, nr, A, pitch, width } = w;
  const mid = Math.round(w.dh / 2);
  const tick = Math.max(1, Math.round(w.dpr));
  const slot = tone * 3 + part;
  g.fillStyle = stripeGrad(w, g, slot, stripeTone(w, tone, part), STRIPE_K);
  for (let j = 0; j < n; j++) {
    const x = Math.round((j + 0.5) * pitch - width / 2);
    let b = Math.round(pk[j] * A);
    if (b < tick) b = tick;
    g.fillRect(x, mid - b, width, 2 * b);
  }
  // The layer being laid: a warm-white core from the centre, inside the amber stripe.
  if (tone !== TONE_DUB || part !== PART_ON || !w.nu) return;
  g.fillStyle = stripeGrad(w, g, 15, HOT, STRIPE_HOT_K);
  for (let j = 0; j < n; j++) {
    let all = Math.round(pk[j] * A);
    if (all < tick) all = tick;
    let core = Math.round(nr[j] * 1.25 * A);
    if (core < tick) core = tick;
    if (core > all - 3 * tick) core = all - 3 * tick;
    if (core <= 0 || nr[j] <= 0) continue;
    g.fillRect(Math.round((j + 0.5) * pitch - width / 2), mid - core, width, 2 * core);
  }
}

/** Dense gradient kinds: a sounding bar, the same at full trail burn, a silent (grey) bar, an overdub's
 * body, the layer's core, the core at full burn. */
const D_ON = 0;
const D_HOT = 1;
const D_GREY = 2;
const D_DUB = 3;
const D_CORE = 4;
const D_CORE_HOT = 5;
/** A bar's core-to-tip ratio, in steps of 1/32. */
const R_STEPS = 32;

/**
 * One unit-tall gradient (y −1..1, scaled to each bar by the transform): its stops are fractions of the
 * bar's half-height from the centre (0) to the tip (1), mirrored. Cached per tone, kind and ratio.
 */
function denseGrad(w: WaveInput, g: CanvasRenderingContext2D, tone: number, kind: number, ri: number): CanvasGradient {
  const slot = (tone * 6 + kind) * (R_STEPS + 1) + ri;
  let gr = w.denseGrads[slot];
  if (gr) return gr;
  const r = ri / R_STEPS;
  const { rgb } = w;
  const c = denseColor(rgb, tone);
  const cc = mix(c, 0.16, WHITE);
  const hot = mix(rgb.dub, 0.62, WHITE);
  // [fraction, colour, alpha] from the centre out.
  let s: [number, number, number][];
  switch (kind) {
    case D_ON:
      s = [[0, cc, 1], [r, cc, 0.82], [1, c, 0.14]];
      break;
    case D_HOT:
      s = [[0, mix(cc, 0.7, WHITE), 1], [r, cc, 1], [1, c, 0.34]];
      break;
    case D_GREY:
      s = tone === TONE_MUTED ? [[0, c, 0.42], [r, c, 0.34], [1, c, 0.1]] : [[0, c, 0.5], [r, c, 0.4], [1, c, 0.12]];
      break;
    case D_DUB:
      s = [[0, c, 0.55], [r, c, 0.55], [1, c, 0.16]];
      break;
    case D_CORE:
      s = [[0, hot, 1], [0.85, hot, 0.92], [1, hot, 0]];
      break;
    default:
      s = [[0, mix(hot, 0.5, WHITE), 1], [0.85, hot, 0.96], [1, hot, 0]];
  }
  gr = g.createLinearGradient(0, -1, 0, 1);
  for (let i = s.length - 1; i >= 0; i--) gr.addColorStop((1 - s[i][0]) / 2, css(s[i][1], s[i][2]));
  for (const [f, col, a] of s) if (f > 0) gr.addColorStop((1 + f) / 2, css(col, a));
  w.denseGrads[slot] = gr;
  return gr;
}

/** The dense look's state colour: muted a cool grey a step under stopped's. */
function denseColor(rgb: WaveRGB, tone: number): number {
  switch (tone) {
    case TONE_PLAY:
      return rgb.play;
    case TONE_REC:
      return rgb.rec;
    case TONE_DUB:
      return rgb.dub;
    case TONE_STOPPED:
      return rgb.dim;
    default:
      return mix(rgb.dim, 0.25, rgb.glass);
  }
}

const ri = (v: number): number => Math.round((v < 0 ? 0 : v > 0.97 ? 0.97 : v) * R_STEPS);

/**
 * One dense bar, rounded as the reference's (`docs/redesign/main.js` `paintA`, a 1 CSS px radius on a 2 px
 * bar: round ends): its outline `hh` px either side of the centre line `mid` at `x`, laid in device px,
 * then filled under a transform that scales the unit-tall gradient `style` (y −1..1) to `scale` px, so
 * the gradient fits the bar while its corners stay round.
 */
function denseBar(
  g: CanvasRenderingContext2D,
  x: number,
  mid: number,
  hh: number,
  width: number,
  radius: number,
  style: CanvasGradient | string,
  scale: number,
): void {
  g.setTransform(1, 0, 0, 1, 0, 0);
  g.beginPath();
  g.roundRect(x, mid - hh, width, 2 * hh, radius < hh ? radius : hh);
  g.setTransform(1, 0, 0, scale, 0, mid);
  g.fillStyle = style;
  g.fill();
}

function paintDense(w: WaveInput, g: CanvasRenderingContext2D, n: number, tone: number, part: number): void {
  const { pk, rm, nr, pitch, width, dpr } = w;
  const mid = Math.round(w.dh / 2);
  const silent = tone === TONE_STOPPED || tone === TONE_MUTED;
  const amp = w.A * (tone === TONE_MUTED ? 0.9 : 1);
  const hot = part === PART_HOT;
  const layer = tone === TONE_DUB && part !== PART_AHEAD && w.nu !== null;
  const white = css(mix(denseColor(w.rgb, tone), 0.78, WHITE));
  // The corner: 1 CSS px, never past half the bar's width.
  const radius = dpr < width / 2 ? dpr : width / 2;
  const alpha = g.globalAlpha;
  if (part === PART_AHEAD) g.globalAlpha = alpha * (tone === TONE_MUTED ? 0.8 : 0.5);
  for (let j = 0; j < n; j++) {
    const x = j * pitch;
    const p = pk[j] > 1e-3 ? pk[j] : 1e-3;
    let hh = pk[j] * amp;
    if (hh < dpr) hh = dpr;
    const r = rm[j] / p;
    if (layer) {
      // An overdub's played bar: the old take's amber body out to the sum, the layer's warm core inside.
      const rn = nr[j] / p;
      denseBar(g, x, mid, hh, width, radius, denseGrad(w, g, tone, D_DUB, ri((r > rn ? r : rn) + 0.1)), hh);
      const ch = rn * hh;
      if (ch >= 1) denseBar(g, x, mid, ch, width, radius, denseGrad(w, g, tone, hot ? D_CORE_HOT : D_CORE, 0), ch);
    } else {
      const kind = silent ? D_GREY : hot ? D_HOT : D_ON;
      denseBar(g, x, mid, hh, width, radius, denseGrad(w, g, tone, kind, ri(r > 0.96 ? 0.96 : r)), hh);
    }
    if (hot) {
      // At full burn a white core over the level, as the trail's peak.
      let hw = (layer ? nr[j] : rm[j]) * amp * 0.8;
      if (hw < dpr) hw = dpr;
      g.globalAlpha = alpha * 0.55;
      denseBar(g, x, mid, hw, width, radius, white, 1);
      g.globalAlpha = alpha;
    }
  }
  g.globalAlpha = alpha;
}

/** In 1/SUB device px under a 1/SUB scale, as `waveform.ts` `SUB` (every path argument a small integer). */
const SUB = 16;
const sub = (v: number): number => (v * SUB + 0.5) | 0;

/**
 * The light bars: a rounded bar per slice, the dim peak with the brighter level inside it. An overdub's
 * played part keeps the loop's level in amber and lights only the layer being laid (`nr`) as a warm core,
 * so audio the dub left unchanged never reads as new. Every tone draws the part to come a step dimmer than
 * the part played, muted included.
 */
function paintBars(w: WaveInput, g: CanvasRenderingContext2D, n: number, tone: number, part: number): void {
  const { pk, rm, nr, pitch, width, rgb } = w;
  const mid = w.dh / 2;
  const amp = w.A * (tone === TONE_MUTED ? 0.47 : 0.72);
  const half = width / 2;
  const ahead = part === PART_AHEAD;
  let c = rgb.play;
  let ao = ahead ? 0.2 : 0.28;
  let ai = ahead ? 0.56 : 0.74;
  if (tone === TONE_REC) c = rgb.rec;
  else if (tone === TONE_DUB) {
    c = rgb.dub;
    if (ahead) {
      ao = 0.26;
      ai = 0.64;
    } else ao = 0.44;
  } else if (tone === TONE_STOPPED) {
    c = rgb.dim;
    ao = 0.22;
    ai = 0.5;
  } else if (tone === TONE_MUTED) {
    c = mix(rgb.dim, 0.45, rgb.glass);
    ao = ahead ? 0.14 : 0.2;
    ai = ahead ? 0.3 : 0.42;
  }
  // The layer's core: an overdub's played part only, while the layer is known.
  const core = tone === TONE_DUB && !ahead && w.nu !== null;
  g.setTransform(1 / SUB, 0, 0, 1 / SUB, 0, 0);
  g.lineCap = 'round';
  g.lineWidth = width * SUB;
  for (let pass = 0; pass < (core ? 3 : 2); pass++) {
    g.strokeStyle = pass === 0 ? css(c, ao) : pass === 1 ? css(c, ai) : css(BARS_HOT, 1);
    const v = pass === 0 ? pk : pass === 1 ? rm : nr;
    g.beginPath();
    for (let j = 0; j < n; j++) {
      const x = sub(j * pitch + pitch / 2);
      const a = v[j] * amp;
      if (pass > 0 && a <= half) continue;
      const h = (a > half ? a : half) - half;
      g.moveTo(x, sub(mid - h));
      g.lineTo(x, sub(mid + h));
    }
    g.stroke();
  }
}
