import { PEAK_FRAMES, clock, looper, sampleRate, type PeakView, type TrackState } from '../state/audio';
import { masterBars } from './shared';

/**
 * Waveform + bar-grid + playhead renderer.
 *
 * OWNS: every looper canvas and the frame-driven chrome that rides the loop phase. Lanes are keyed by
 * canvas and each draws its track independently. (The stage view draws itself: `src/ui/stage/`.)
 *
 * A SINGLE requestAnimationFrame loop drives every registered track canvas. It reads the looper's
 * pre-computed peak arrays through non-reactive getters (`looper.peaksInto` / `phaseValue` / `stateOf`
 * / `recHeadFrac` / `recSpanFrames` / `masterFramesValue` — NO Solid signals in the per-frame path, per
 * invariant 6) and never scans raw PCM. Per track the waveform (one filled body) AND its bar grid are
 * rasterised into an off-screen canvas and cached; the bitmap is re-rasterised ONLY when the peaks change
 * (version bump), the track state changes (colour), the master loop length changes (grid geometry), a
 * recording take's span changes (bin placement), or the element resizes. Every frame the loop just
 * blits that cached bitmap and draws the playhead — so the steady-state per-frame cost is a `drawImage`
 * + a few `fillRect`s per track, holding 60fps with flat GC even with all five tracks live.
 *
 * The bar grid is derived from `masterBars()` (the shared bar-math in `shared.ts`) so it can
 * never disagree with the spoken loop length. That is the ONLY place Solid signals are read
 * (`clock.bpm()`, and the device's rate behind `sampleRate()`), and it is gated to a
 * master-length change (a rare structural event; BPM is locked for the life of a committed master)
 * inside the cached-bitmap path — never in the per-frame steady state. The meter's beats a bar arrive
 * plain, through `setGridBeats` (an effect in `Looper.tsx`); a new meter re-reads every lane's grid, since
 * two meters can share a master length (one 4/4 bar and two 6/8 bars at the same BPM). (A first take's opening span
 * reads the tempo once per take: the engine store takes it as the take starts, off the draw loop.)
 * `verify/guards/stage-draw.mjs` holds this module to that, the exception allowed by name.
 *
 * Solid only ever creates/destroys the <canvas> elements (the Looper component) and calls
 * `registerLane` / `unregisterLane`; all drawing lives here in plain TS.
 */

// Bar-grid tints — constant, not theme-dependent (based on the --surf-1 base tint).
const GRID_BEAT = 'rgba(148,168,215,.065)'; // beat lines — a whisper
const GRID_BAR = 'rgba(148,168,215,.11)'; //   bar lines — readable, a step over the beats
const GRID_NUM = 'rgba(148,168,215,.56)'; //   bar numbers, top-left of each bar: read from the chair, never brighter than a wave
/**
 * The wave is one filled, anti-aliased BODY in its colour, the take's level, its own edge the only edge
 * (no second silhouette around it). Alpha per look (of the colour): a sounding take is saturated; a
 * stopped one is a solid grey (--dim) body, quieter than any sounding take; a muted one is a darker grey
 * body under a crisp grey contour (`MUTED_EDGE`), quieter than stopped and plainly there.
 */
const LOOK_SOUNDING = 0.94;
const LOOK_STOPPED = 0.42;
const LOOK_MUTED = 0.2;
/** A muted take's contour: its alpha (of --dim) and width in CSS px, `wide` on a lane `PLAYHEAD_WIDE`
 * CSS px or wider (2 device px on a 2560 display at 100 %), so it holds from the chair. */
const MUTED_EDGE = { alpha: 0.6, width: 1.5, wide: 2 };
/** A fading take's colour over its grey silhouette: a fixed step toward grey from the moment the fade
 * begins (`drawFade`), so a fade never reads as a plain playing take. */
const FADE_STEP = 0.42;
/** A muted lane that captures keeps its capture colour, a step down (its word says REC/OVERDUB). */
const MUTED_CAPTURE = 0.6;
/** The body's height as a share of the bins' mean |peak|, smoothed over a bin each side, so the body
 * carries the take and a lone spike does not jag its edge. Never past the bins' peak envelope. */
const BODY_K = 0.88;
/** The display's zoom: a level `v` is drawn at `ZOOM`·v up to the knee, eased into full scale above it
 * (`shown`), so a take at a working level fills its row while a louder one still draws taller and a
 * full-scale one stays inside the row. */
const ZOOM = 1.3;
/** The loudest peak's reach, as a share of half the well's height: the rows of the glass display meet
 * edge to edge, so a full-scale take keeps a little clear air above and below it. */
const WAVE_REACH = 0.94;
/** The playhead's width in CSS px, one width in every state: on a lane this many CSS px wide or wider
 * (a 2560 display) `PLAYHEAD_CSS_WIDE`, else `PLAYHEAD_CSS` (`drawPlayhead`). */
const PLAYHEAD_WIDE = 1600;
const PLAYHEAD_CSS = 2;
const PLAYHEAD_CSS_WIDE = 2.5;
/** The centre line's alpha in --faint: a guide under the wave, quieter than any wave. */
const MID_ALPHA = 0.45;
/** The meter's counted beats a bar (`setGridBeats`): the grid's beat lines, a bar line every `gridBeats`. */
let gridBeats = 4;
/**
 * FADE, per lane, a plain feed a Solid effect writes (`setLaneFade`, `Looper.tsx`): the device frame the
 * fade's stop lands on (none: no fade) and the frame heard as the lane began fading. The engine ramps a
 * fading lane's level as r², r running linearly from 1 at the press to 0 at the stop (`lf-engine`
 * `looper.rs`, `render`), so the draw loop knows the level the lane sounds at now.
 */
const fadeTo: number[] = [];
const fadeFrom: number[] = [];

interface LaneColors {
  empty: string;
  recording: string;
  overdubbing: string;
  playing: string;
  stopped: string;
  playhead: string;
  rec: string;
  mid: string;
}

interface Lane {
  index: number;
  canvas: HTMLCanvasElement;
  ctx: CanvasRenderingContext2D;
  /** Off-screen cached waveform+grid bitmap (device pixels). */
  wave: HTMLCanvasElement;
  wctx: CanvasRenderingContext2D;
  colors: LaneColors;
  /** Device-pixel backing-store size and the CSS size it was derived from. */
  dw: number;
  dh: number;
  cssW: number;
  cssH: number;
  dpr: number;
  /** Dirty trackers — re-rasterise the bitmap when any of these change. */
  lastVersion: number;
  lastWaiting: boolean;
  lastState: TrackState | '';
  /** Mute at the last rasterise — a muted take is drawn dimmed so silence reads from the stage. */
  lastMuted: boolean;
  /** Master loop length at the last rasterise — grid geometry changes when this does. */
  lastMasterFrames: number;
  /** A recording take's span at the last rasterise (0: none) — its bins are placed over it. */
  lastSpan: number;
  /** Grid cache: the bar count for `gridMaster`, recomputed (reading bpm) only when master changes. */
  gridMaster: number;
  gridBars: number;
  /** Cached bar-number font string (depends on dpr and the row's height) — so rasterise allocates nothing. */
  gridFont: string;
  /** The wave's per-column scratch, 4 × `dw` (set on resize, reused by every rasterise): the outer
   * envelope's top and bottom and the body's, as signed amplitudes. */
  cols: Float32Array;
  /** Per bin, on the display's scale (`shown`), filled once per rasterise ahead of `envelope`: the body's
   * level (`level` × `BODY_K`) and the bin's max and min. They grow with the take, never shrink. Computed
   * once per bin rather than per column and per knot, and read from an array, so no double crosses a
   * call that might not inline (a boxed return was ~1.4 MB/s in a take). The cubic runs through the
   * shown values, so an attack's rise keeps its place in its bin (wave-steady). */
  levels: Float64Array;
  shownMax: Float32Array;
  shownMin: Float32Array;
  /** Per bin, the cubic's slopes into and out of each knot of those three rows (`slopes`), stride the
   * arrays' length. */
  slopes: Float32Array;
  /** A fade at the last rasterise: the take is drawn twice, grey in `wave` and sounding in `fading`. */
  lastFading: boolean;
  /** While the lane fades (`drawFade`, made on its first fade): its take in its sounding colour, no grid. */
  fading: Fade | null;
}

interface Fade {
  take: HTMLCanvasElement;
  tctx: CanvasRenderingContext2D;
}

/** The bar-number font at a given device-pixel ratio and row height (CSS px): Geist Mono, the `--mono`
 * token's stack (`src/app.css`), 9.5 px in a short row up to 14 px in a tall one (a 2560 display), so a
 * number reads from the chair without shouting over the take. */
function gridFontFor(dpr: number, cssH: number): string {
  const px = Math.min(14, Math.max(9.5, cssH * 0.075));
  return `500 ${(px * dpr).toFixed(1)}px 'Geist Mono', ui-monospace, Consolas, monospace`;
}

/**
 * Geist Mono is an `@font-face` (`src/app.css`) that loads on first use: a grid rasterised before it is
 * in draws a fallback face, and the cached bitmap keeps it until something else dirties the lane. So the
 * first lane asks for the face once, and when it lands every lane re-rasterises (a plain field write,
 * read by the draw loop's dirty check; no signal, invariant 6).
 */
let gridFontAsked = false;
function askGridFont(font: string): void {
  if (gridFontAsked) return;
  gridFontAsked = true;
  if (document.fonts.check(font)) return;
  document.fonts.load(font).then(
    () => {
      for (const lane of lanes.values()) lane.lastVersion = -2;
    },
    // A face that fails to load or decode leaves the fallback in place; a rejection here would
    // otherwise surface as an unhandled one in the release log for a numeral nobody can fix at a jam.
    () => {},
  );
}

const lanes = new Map<HTMLCanvasElement, Lane>();

/**
 * The command-bar ring dial rides the same loop: one SVG circle whose stroke-dashoffset follows the
 * plain loop phase. Registered by Transport.tsx; a signal here would be 40 writes/s from the drain
 * timer (invariant 6), so the dial is driven from this frame instead.
 */
let dial: { el: SVGCircleElement; circumference: number; last: number } | null = null;

function drawDial(): void {
  if (!dial) return;
  const offset = dial.circumference * (1 - looper.phaseValue());
  if (Math.abs(offset - dial.last) < 0.05) return;
  dial.last = offset;
  dial.el.setAttribute('stroke-dashoffset', offset.toFixed(2));
}

/**
 * The command-bar record-level meter rides the loop too: `--lvl` (0..1 over −60..0 dBFS, so a quiet
 * guitar and the AUTO threshold both land somewhere readable) on one element, `is-hot` above −1 dB,
 * aria-valuenow/aria-valuetext refreshed ~6×/s. The text says "near clipping" from that red, and
 * "clipping" only at full scale (a level of 1 or more: the store's for a frame that carried a clip).
 * Transport.tsx puts the AUTO threshold on the same scale via `meterFrac` (a low-frequency effect, not
 * this loop).
 */
let meter: { el: HTMLElement; last: number; hot: boolean; ariaTick: number } | null = null;

const METER_FLOOR_DB = -60;
/** A level at or above this (linear; −1 dBFS) reads hot: the command bar's meter turns red. */
const HOT = 0.89;

/** Linear amplitude → 0..1 across the meter's span (−60..0 dBFS); 0 and below → 0. */
export function meterFrac(linear: number): number {
  if (!(linear > 0)) return 0;
  const frac = (20 * Math.log10(linear) - METER_FLOOR_DB) / -METER_FLOOR_DB;
  return frac < 0 ? 0 : frac > 1 ? 1 : frac;
}

function drawMeter(): void {
  if (!meter) return;
  const raw = looper.levelValue();
  const lvl = meterFrac(raw);
  if (Math.abs(lvl - meter.last) >= 0.01) {
    meter.last = lvl;
    meter.el.style.setProperty('--lvl', lvl.toFixed(2));
  }
  const hot = raw >= HOT;
  if (hot !== meter.hot) {
    meter.hot = hot;
    meter.el.classList.toggle('is-hot', hot);
  }
  if (++meter.ariaTick >= 10) {
    meter.ariaTick = 0;
    meter.el.setAttribute('aria-valuenow', lvl.toFixed(2));
    const db = Math.round(METER_FLOOR_DB + -METER_FLOOR_DB * lvl);
    meter.el.setAttribute('aria-valuetext', `${db} dBFS${raw >= 1 ? ', clipping' : hot ? ', near clipping' : ''}`);
  }
}

/** How long a slot card's meter holds its peak, and its CLIP lamp after a clip (ms). */
const HOLD_MS = 1500;
/** How fast a held peak falls once its hold ends (meter fraction per ms: the whole span in 2 s). */
const PEAK_FALL = 1 / 2000;

/**
 * A live slot's input meter (`SlotControls.tsx` `SlotMeter`): that slot's own input, full-width at the
 * foot of its card, readable from where the guitarist stands. It writes `--lvl` (the level), `--pk` (a
 * peak held HOLD_MS, then falling) and `is-clip` (lit HOLD_MS after the engine reported a sample at full
 * scale, never for a level merely near it), all as style writes from this loop, never a signal
 * (invariant 6). A clip is read off the store's clip counter and a peak off its untaken peak, so one
 * that arrived between two draws still lights the lamp or holds the peak (a clip's at full scale).
 */
interface CardMeter {
  el: HTMLElement;
  slot: 0 | 1;
  /** The `--lvl` and `--pk` last written. */
  lvl: number;
  pk: number;
  /** The held peak and when it was taken: it falls from there once HOLD_MS has passed. */
  held: number;
  heldAt: number;
  /** The store's clip count last seen, and until when the lamp stays lit. */
  clips: number;
  clipUntil: number;
  clip: boolean;
}
const cardMeters: [CardMeter | null, CardMeter | null] = [null, null];

function drawCardMeter(m: CardMeter, now: number): void {
  const lvl = meterFrac(looper.levelValue(m.slot));
  if (Math.abs(lvl - m.lvl) >= 0.005) {
    m.lvl = lvl;
    m.el.style.setProperty('--lvl', lvl.toFixed(3));
  }
  const clips = looper.clipCount(m.slot);
  if (clips !== m.clips) {
    m.clips = clips;
    m.clipUntil = now + HOLD_MS;
    m.held = 1;
    m.heldAt = now;
  }
  let pk = m.held - Math.max(0, now - m.heldAt - HOLD_MS) * PEAK_FALL;
  const peak = Math.max(lvl, meterFrac(looper.takePeak(m.slot)));
  if (peak >= pk) {
    m.held = pk = peak;
    m.heldAt = now;
  }
  if (Math.abs(pk - m.pk) >= 0.003) {
    m.pk = pk;
    m.el.style.setProperty('--pk', pk.toFixed(3));
  }
  const clip = now < m.clipUntil;
  if (clip !== m.clip) {
    m.clip = clip;
    m.el.classList.toggle('is-clip', clip);
  }
}

/** Register slot `slot`'s card meter (it went live). Starts the shared rAF loop if nothing else has. */
export function registerCardMeter(slot: 0 | 1, el: HTMLElement): void {
  // Already registered (an effect re-ran): keep its hold and its lamp.
  if (cardMeters[slot]?.el === el) return;
  // A clip or a peak from before the slot went live is not this meter's.
  looper.takePeak(slot);
  cardMeters[slot] = { el, slot, lvl: -1, pk: -1, held: 0, heldAt: 0, clips: looper.clipCount(slot), clipUntil: 0, clip: false };
  if (rafId === 0) rafId = requestAnimationFrame(frame);
}

/** Unregister slot `slot`'s card meter and leave it dark; stops the loop when nothing else is registered. */
export function unregisterCardMeter(slot: 0 | 1, el: HTMLElement): void {
  if (cardMeters[slot]?.el === el) cardMeters[slot] = null;
  el.style.setProperty('--lvl', '0');
  el.style.setProperty('--pk', '0');
  el.classList.remove('is-clip');
  stopIfIdle();
}

/** The meter's counted beats a bar, from the Solid side (`Looper.tsx`): every lane's grid is drawn again
 * with them on its next frame. */
export function setGridBeats(beats: number): void {
  if (beats === gridBeats) return;
  gridBeats = beats;
  for (const lane of lanes.values()) {
    lane.gridMaster = -1;
    lane.lastMasterFrames = -1;
  }
}

/** Register the record-level meter element (Transport.tsx). */
export function registerInputMeter(el: HTMLElement): void {
  meter = { el, last: -1, hot: false, ariaTick: 0 };
  if (rafId === 0) rafId = requestAnimationFrame(frame);
}

/** Unregister the record-level meter; stops the loop if nothing else is registered. */
export function unregisterInputMeter(): void {
  meter = null;
  stopIfIdle();
}

/** Register the ring-dial arc (Transport.tsx). Starts the shared rAF loop if nothing else has. */
export function registerPhaseDial(el: SVGCircleElement, circumference: number): void {
  dial = { el, circumference, last: -1 };
  if (rafId === 0) rafId = requestAnimationFrame(frame);
}

/** Unregister the ring-dial arc; stops the loop when no lane is registered either. */
export function unregisterPhaseDial(): void {
  dial = null;
  stopIfIdle();
}

function stopIfIdle(): void {
  if (lanes.size === 0 && dial === null && meter === null && !cardMeters[0] && !cardMeters[1] && rafId !== 0) {
    cancelAnimationFrame(rafId);
    rafId = 0;
  }
}
let rafId = 0;
/** Reused across all peak reads each frame so the loop allocates nothing. */
const peakScratch: PeakView = { min: null, max: null, count: 0, version: 0 };

/** Read the theme colours off an element's computed style once (they don't change at runtime). */
function readColors(el: HTMLElement): LaneColors {
  const cs = getComputedStyle(el);
  const v = (name: string, fallback: string): string => cs.getPropertyValue(name).trim() || fallback;
  return {
    empty: v('--dim', '#bcc3d4'),
    recording: v('--rec', '#ff4757'),
    overdubbing: v('--dub', '#ffb02e'),
    playing: v('--play', '#2be48a'),
    stopped: v('--dim', '#bcc3d4'),
    playhead: v('--text', '#eef1f8'),
    rec: v('--rec', '#ff4757'),
    mid: v('--faint', '#7c8498'),
  };
}

/** Pick the waveform colour for a track state: a muted take that plays or stops is silent, so grey (the
 * lane's word says MUTED, `lane-state.ts`); a muted lane that captures keeps its capture colour. */
function waveColor(c: LaneColors, state: TrackState, muted: boolean): string {
  if (muted && (state === 'PLAYING' || state === 'STOPPED')) return c.stopped;
  switch (state) {
    case 'RECORDING':
      return c.recording;
    case 'OVERDUBBING':
      return c.overdubbing;
    case 'PLAYING':
      return c.playing;
    case 'STOPPED':
      return c.stopped;
    default:
      return c.empty;
  }
}

/** Re-measure the canvas and (re)size both backing stores to device pixels. Returns true if changed. */
function syncSize(lane: Lane): boolean {
  const cssW = lane.canvas.clientWidth;
  const cssH = lane.canvas.clientHeight;
  const dpr = self.devicePixelRatio || 1;
  if (cssW === lane.cssW && cssH === lane.cssH && dpr === lane.dpr) return false;
  lane.cssW = cssW;
  lane.cssH = cssH;
  lane.dpr = dpr;
  lane.dw = Math.max(1, Math.round(cssW * dpr));
  lane.dh = Math.max(1, Math.round(cssH * dpr));
  lane.gridFont = gridFontFor(dpr, cssH);
  lane.cols = new Float32Array(4 * lane.dw);
  lane.canvas.width = lane.dw;
  lane.canvas.height = lane.dh;
  lane.wave.width = lane.dw;
  lane.wave.height = lane.dh;
  return true;
}

/** Draw the bar/beat grid + engraved bar numbers UNDER the wave. `bars` from the shared bar-math. */
function drawGrid(lane: Lane, bars: number): void {
  if (bars <= 0) return;
  const { wctx, dw, dh, dpr } = lane;
  const total = bars * gridBeats;
  for (let i = 1; i < total; i++) {
    const gx = Math.round((i / total) * dw);
    wctx.fillStyle = i % gridBeats === 0 ? GRID_BAR : GRID_BEAT;
    wctx.fillRect(gx, 0, 1, dh);
  }
  // Each number hangs off the top of the bar line it names, as on a ruler: its left edge a hair right
  // of the line (Geist Mono: every figure one width, so 9 and 10 line up as tabular figures do).
  wctx.fillStyle = GRID_NUM;
  wctx.font = lane.gridFont;
  wctx.textBaseline = 'top';
  const pad = 1 + Math.round(4 * dpr);
  const top = Math.round(5 * dpr);
  for (let b = 0; b < bars; b++) {
    wctx.fillText(String(b + 1), Math.round((b / bars) * dw) + pad, top);
  }
}

/**
 * The slopes of the monotone cubic through row `y`'s first `count` knots (sign `s`: +1 for a top edge or
 * a level, −1 for a bottom edge), per knot into `sl`: the slope arriving from the left at `sl[into + p]`,
 * leaving to the right at `sl[out + p]`. A knot's slope is the harmonic mean of the secants either side
 * (Fritsch–Butland: zero at an extreme or a flat side, no overshoot). At a PINCH, a knot whose magnitude
 * is a strict local minimum (two lobes meeting), each side instead takes its own secant times the join's
 * depth (twice its dip below the nearer neighbour, as a share of it, at most 1), so a deep join is a clean
 * point, not a flat shoulder, and a shallow dip in a sustained tone stays round. That slope is at most
 * the secant's, so a segment still never passes its knots: no invented data. Once per row per rasterise,
 * all arithmetic inline and written to `sl`: no double crosses a call (the per-column `hermite` stays
 * small enough to inline; with the pinch inside it, it did not, ~1.3 MB/s in a take).
 */
function slopes(
  y: Float32Array | Float64Array,
  s: number,
  count: number,
  sl: Float32Array,
  into: number,
  out: number,
): void {
  const last = count - 1;
  for (let p = 0; p < count; p++) {
    const ya = y[p > 0 ? p - 1 : 0];
    const yk = y[p];
    const yb = y[p < last ? p + 1 : last];
    const dl = yk - ya;
    const dr = yb - yk;
    let mi = dl * dr <= 0 ? 0 : (2 * dl * dr) / (dl + dr);
    let mo = mi;
    const a = s * ya;
    const k = s * yk;
    const b = s * yb;
    if (k < a && k < b) {
      const w = depth(k, a < b ? a : b);
      mi = w * dl;
      mo = w * dr;
    }
    sl[into + p] = mi;
    sl[out + p] = mo;
  }
}

/** A pinch's depth, 0..1: twice the dip of its knot `k` below `near` (its nearer neighbour), as a share
 * of it, at most 1 (a knot at or under zero: 1). Called from `slopes` only, its result stored straight
 * into a typed array. */
function depth(k: number, near: number): number {
  if (k <= 0) return 1;
  const w = 2 * (1 - k / near);
  return w < 0 ? 0 : w > 1 ? 1 : w;
}

/** The cubic Hermite between knots `y1` (f = 0) and `y2` (f = 1), leaving `y1` at slope `m1` and
 * arriving at `y2` at `m2` (`slopes`). */
function hermite(y1: number, y2: number, m1: number, m2: number, f: number): number {
  const f2 = f * f;
  const f3 = f2 * f;
  return (2 * f3 - 3 * f2 + 1) * y1 + (f3 - 2 * f2 + f) * m1 + (3 * f2 - 2 * f3) * y2 + (f3 - f2) * m2;
}

/** A level (>= 0, linear) as drawn, 0..1 of the reach: `ZOOM`·v eased into 1 by a soft knee,
 * g / (1 + g⁴)^¼: proportional within 3 % up to 0.45 of full scale (about −7 dBFS), and rising all the
 * way, so a louder take always draws taller. */
function shown(v: number): number {
  const g = v * ZOOM;
  const g2 = g * g;
  return g / Math.sqrt(Math.sqrt(1 + g2 * g2));
}

/** Bin `p`'s value in `a`, the index held to the take (its first and last bins hold past its ends). */
function at(a: Float32Array, p: number, last: number): number {
  return a[p < 0 ? 0 : p > last ? last : p];
}

/** Bin `p`'s mean |peak|, (|min| + |max|) / 2: we only have min/max bins. */
function meanPeak(min: Float32Array, max: Float32Array, p: number, last: number): number {
  const lo = at(min, p, last);
  const hi = at(max, p, last);
  return ((lo < 0 ? -lo : lo) + (hi < 0 ? -hi : hi)) / 2;
}

/** The body's level at bin `p`: its mean |peak| smoothed over a bin each side (1-2-1, centred, so the
 * body neither lags nor leads the take), and never above the bin's own: smoothing pulls a spike down
 * into the outer, it never swells the body ahead of an attack. */
function level(min: Float32Array, max: Float32Array, p: number, last: number): number {
  const own = meanPeak(min, max, p, last);
  const smooth = (meanPeak(min, max, p - 1, last) + 2 * own + meanPeak(min, max, p + 1, last)) / 4;
  return smooth < own ? smooth : own;
}

/**
 * Sample the take once per device column into `out` (4 × `dw`, signed, on the display's scale: `min`,
 * `max` and `lv` come shown, `Lane.levels`): the outer
 * envelope's top at [0, dw), its bottom at [dw, 2dw), the body's top at [2dw, 3dw) and its bottom at
 * [3dw, 4dw). `across` bins span the width; the first `cols` columns hold the take. A column narrower
 * than a bin takes the monotone cubic through the bin centres at its own centre, its knots' slopes from
 * `sl` (`slopes`, stride `z`: max in/out, min in/out, level in/out); a column wider than a bin takes its
 * bins' extremes (and their mean level for the body, from `lv`, `level` per bin).
 * O(cols + count), no allocation.
 * Returns the columns written.
 */
function envelope(
  out: Float32Array,
  dw: number,
  cols: number,
  count: number,
  across: number,
  min: Float32Array,
  max: Float32Array,
  lv: Float64Array,
  sl: Float32Array,
  z: number,
): number {
  const per = across / dw; // bins per column
  const last = count - 1;
  let n = 0;
  for (let x = 0; x < cols; x++) {
    let hi: number;
    let lo: number;
    let body: number;
    if (per >= 1) {
      const p0 = Math.floor(x * per);
      if (p0 > last) break;
      let p1 = Math.floor((x + 1) * per);
      if (p1 <= p0) p1 = p0 + 1;
      if (p1 > count) p1 = count;
      hi = max[p0];
      lo = min[p0];
      body = 0;
      for (let p = p0; p < p1; p++) {
        if (max[p] > hi) hi = max[p];
        if (min[p] < lo) lo = min[p];
        body += lv[p];
      }
      body /= p1 - p0;
    } else if (last === 0) {
      hi = max[0];
      lo = min[0];
      body = lv[0];
    } else {
      // The column's centre in bin-centre units (bin b's knot sits at b + 0.5 bins), held to the take.
      let t = (x + 0.5) * per - 0.5;
      if (t < 0) t = 0;
      else if (t > last) t = last;
      const i = t >= last ? last - 1 : Math.floor(t);
      const f = t - i;
      hi = hermite(max[i], max[i + 1], sl[z + i], sl[i + 1], f);
      lo = hermite(min[i], min[i + 1], sl[3 * z + i], sl[2 * z + i + 1], f);
      body = hermite(lv[i], lv[i + 1], sl[5 * z + i], sl[4 * z + i + 1], f);
    }
    // The body never passes the outer.
    const capHi = hi > 0 ? hi : 0;
    const capLo = lo < 0 ? lo : 0;
    out[x] = hi;
    out[dw + x] = lo;
    out[2 * dw + x] = body < capHi ? body : capHi;
    out[3 * dw + x] = -body > capLo ? -body : capLo;
    n = x + 1;
  }
  return n;
}

/** Path coordinates go to the canvas in 1/`SUB` device px, as integers, under a 1/`SUB` scale: the
 * edge keeps its sub-pixel placement (the anti-aliasing sees the same shape to 1/16 px), and every
 * `lineTo` argument is a small integer, so V8 passes it unboxed. A fractional double argument is boxed
 * into a fresh heap number per call: ~7000 calls a rasterise were ~5 MB/s of garbage while a take
 * records. */
const SUB = 16;

/** `v` (device px, >= 0) in 1/`SUB` px, rounded to an integer. */
function sub(v: number): number {
  return (v * SUB + 0.5) | 0;
}

/**
 * Fill the band between two of `envelope`'s rows (`upper` above, `lower` below, offsets into `buf`) over
 * its first `n` columns as one anti-aliased path, a vertex at each column's centre, cut square at `end`
 * (device px). The band is at least `minHalf` px either side of the centre line.
 */
function fillBand(
  ctx: CanvasRenderingContext2D,
  buf: Float32Array,
  upper: number,
  lower: number,
  n: number,
  end: number,
  midY: number,
  A: number,
  minHalf: number,
  step: number,
): void {
  if (n < 1) return;
  // Each row's y, held clear of the centre by `minHalf` (no closures: rasterise allocates nothing).
  const top = sub(midY - minHalf);
  const bot = sub(midY + minHalf);
  const e = sub(end);
  const half = SUB >> 1;
  ctx.setTransform(1 / SUB, 0, 0, 1 / SUB, 0, 0);
  ctx.beginPath();
  let y = sub(midY - buf[upper] * A);
  ctx.moveTo(0, y > top ? top : y);
  // Every `step`th column and the last one, out along the top and back along the bottom.
  for (let x = 0; ; x = x + step < n - 1 ? x + step : n - 1) {
    y = sub(midY - buf[upper + x] * A);
    const cx = x * SUB + half;
    ctx.lineTo(cx < e ? cx : e, y > top ? top : y);
    if (x === n - 1) break;
  }
  ctx.lineTo(e, y > top ? top : y);
  for (let x = n - 1; ; x = x === n - 1 ? Math.floor((n - 2) / step) * step : x - step) {
    y = sub(midY - buf[lower + x] * A);
    const cx = x * SUB + half;
    if (x === n - 1) ctx.lineTo(e, y < bot ? bot : y);
    ctx.lineTo(cx < e ? cx : e, y < bot ? bot : y);
    if (x === 0) break;
  }
  ctx.lineTo(0, y < bot ? bot : y);
  ctx.closePath();
  ctx.fill();
  ctx.setTransform(1, 0, 0, 1, 0, 0);
}

/** One edge of `envelope`'s rows (`row`, an offset into `buf`) over its first `n` columns as an open
 * subpath, a vertex at each `step`th column's centre, cut square at `e` (1/`SUB` px). */
function edgePath(
  ctx: CanvasRenderingContext2D,
  buf: Float32Array,
  row: number,
  n: number,
  e: number,
  midY: number,
  A: number,
  step: number,
): void {
  const half = SUB >> 1;
  let y = sub(midY - buf[row] * A);
  ctx.moveTo(0, y);
  for (let x = 0; ; x = x + step < n - 1 ? x + step : n - 1) {
    y = sub(midY - buf[row + x] * A);
    const cx = x * SUB + half;
    ctx.lineTo(cx < e ? cx : e, y);
    if (x === n - 1) break;
  }
  ctx.lineTo(e, y);
}

/**
 * Draw a take from `envelope`'s rows (`buf`, its first `n` columns) in `color` at alpha `body` (times
 * `mul`): the body, one anti-aliased fill. With an `edge` width (device px) its top and bottom edges are
 * stroked too, at `MUTED_EDGE`'s alpha: a muted take's contour.
 */
function drawTake(
  ctx: CanvasRenderingContext2D,
  buf: Float32Array,
  n: number,
  end: number,
  midY: number,
  A: number,
  step: number,
  color: string,
  body: number,
  edge: number,
  mul = 1,
): void {
  const dw = buf.length >> 2;
  ctx.fillStyle = color;
  // The body keeps a 1 px trace through a silent stretch, so a take reads as one line in its colour.
  ctx.globalAlpha = body * mul;
  fillBand(ctx, buf, 2 * dw, 3 * dw, n, end, midY, A, 0.5, step);
  if (edge > 0 && n > 0) {
    // Round joins, so the contour is as smooth as the fill; through silence the two edges meet in one
    // line on the centre.
    ctx.strokeStyle = color;
    ctx.globalAlpha = MUTED_EDGE.alpha * mul;
    ctx.lineWidth = edge * SUB;
    ctx.lineJoin = 'round';
    ctx.setTransform(1 / SUB, 0, 0, 1 / SUB, 0, 0);
    ctx.beginPath();
    const e = sub(end);
    edgePath(ctx, buf, 2 * dw, n, e, midY, A, step);
    edgePath(ctx, buf, 3 * dw, n, e, midY, A, step);
    ctx.stroke();
    ctx.setTransform(1, 0, 0, 1, 0, 0);
  }
  ctx.globalAlpha = 1;
}

/** The lane's fade canvas (`Lane.fading`), made on its first fade and resized with the lane: the
 * cached path only (`rasterise`), so the draw loop allocates nothing. */
function fadeFor(lane: Lane): Fade | null {
  const { dw, dh } = lane;
  let f = lane.fading;
  if (!f) {
    const take = document.createElement('canvas');
    const tctx = take.getContext('2d');
    // No context: the lane draws its fade as a plain playing take.
    if (!tctx) return null;
    f = { take, tctx };
    lane.fading = f;
  }
  if (f.take.width !== dw || f.take.height !== dh) {
    f.take.width = dw;
    f.take.height = dh;
  }
  return f;
}

/**
 * Rasterise the bar grid + the waveform body into the off-screen bitmap. All of this is the
 * CACHED-BITMAP path — it runs only on a dirty frame, never in the per-frame steady state.
 *
 * The wave is one filled path (`fillBand`), anti-aliased by the canvas: the BODY (`BODY_K` of the bins'
 * mean |peak|, smoothed over a bin each side, mirrored around mid and clipped to the bins' peak envelope).
 * It is sampled once per device column (`envelope`): where a bin spans several columns the column takes a
 * monotone cubic through the bin centres (`hermite`), which passes through every bin's value and never
 * past its neighbours, so a peak keeps its height and an attack rises within its bin, with no stair-steps
 * and no invented shape; where a column spans several bins it takes their extremes. While a take records (`span` > 0, its
 * `recSpanFrames`) each bin sits at its frame over the span, so the recorded region ends at `end` and, on
 * a later take, the remainder gets the dotted rec-red "tape to fill" guide.
 */
function rasterise(
  lane: Lane,
  count: number,
  min: Float32Array | null,
  max: Float32Array | null,
  state: TrackState,
  masterFrames: number,
  muted: boolean,
  span: number,
  fade: boolean,
): void {
  const { wctx, dw, dh, dpr } = lane;
  wctx.clearRect(0, 0, dw, dh);
  wctx.globalAlpha = 1;

  const midY = dh / 2;
  const A = midY * WAVE_REACH; // peak amplitude in px — headroom so loud transients never reach the row's edge

  // Bar grid, UNDER the wave. Bar count comes from the shared Looper.tsx helper; it (and the one
  // bpm signal read) is cached and only recomputed when the master loop length changes.
  if (masterFrames !== lane.gridMaster) {
    lane.gridMaster = masterFrames;
    lane.gridBars = masterFrames > 0 ? masterBars(masterFrames, clock.bpm(), sampleRate(), gridBeats) : 0;
  }
  drawGrid(lane, lane.gridBars);

  // Bins across the full width: a recording take's span in bins, else the loop's `count`. While a take
  // records, bin b sits at b·PEAK_FRAMES/span of the width whatever the record head does, so a drawn
  // feature holds still as bins arrive and moves only when the span grows (a free take passing the loop,
  // a first take doubling its window). `cols` is the recorded region.
  const across = span > 0 ? span / PEAK_FRAMES : count;
  const cols = span > 0 ? Math.min(dw, Math.ceil((count / across) * dw)) : dw;

  // Centre line vs "tape to fill": while a later track is laying down a take, the mid line becomes a
  // dotted rec-red guide over the not-yet-recorded remainder; otherwise it is a plain faint centre line.
  // (A first take's span is only a window, not its length → plain line, no tape.) Armed / count-in /
  // listening is not a take yet (span 0): plain line, no tape (the chrome says ARMED).
  const hasTape = span > 0 && masterFrames > 0;

  // The centre line runs where no take is drawn (a take carries its own 1 px trace through silence), so
  // it never strikes through a quiet grey take.
  const hasTake = count > 0 && min !== null && max !== null && cols >= 1;
  const from = hasTake ? cols : 0;
  if (!hasTape && from < dw) {
    wctx.fillStyle = lane.colors.mid;
    wctx.globalAlpha = MID_ALPHA;
    wctx.fillRect(from, Math.round(midY), dw - from, 1);
    wctx.globalAlpha = 1;
  }

  // The wave: the body per device column (`envelope`), filled as one path (a muted take's contour traces
  // the same rows).
  if (hasTake && min && max) {
    // Where the recorded region ends (device px): the take's last bin, a crisp cut while it records.
    const end = span > 0 ? Math.min(dw, (count / across) * dw) : dw;
    if (lane.levels.length < count) {
      const size = Math.max(256, count, 2 * lane.levels.length);
      lane.levels = new Float64Array(size);
      lane.shownMax = new Float32Array(size);
      lane.shownMin = new Float32Array(size);
      lane.slopes = new Float32Array(6 * size);
    }
    const { levels: lv, shownMax: hs, shownMin: ls, slopes: sl } = lane;
    const z = lv.length;
    for (let p = 0; p < count; p++) {
      lv[p] = shown(level(min, max, p, count - 1) * BODY_K);
      const hi = max[p];
      const lo = min[p];
      hs[p] = hi > 0 ? shown(hi) : -shown(-hi);
      ls[p] = lo > 0 ? shown(lo) : -shown(-lo);
    }
    slopes(hs, 1, count, sl, 0, z);
    slopes(ls, -1, count, sl, 2 * z, 3 * z);
    slopes(lv, 1, count, sl, 4 * z, 5 * z);
    const n = envelope(lane.cols, dw, cols, count, across, ls, hs, lv, sl, z);
    // A vertex every 2 device px where a bin spans 4 or more (the cubic is near-straight over 2 px),
    // else one per column, which keeps every bin's extremes.
    const step = across * 4 <= dw ? 2 : 1;
    const capturing = state === 'RECORDING' || state === 'OVERDUBBING';
    const f = fade ? fadeFor(lane) : null;
    if (f) {
      // A fading lane's take twice: grey, as it will be once stopped, and in its sounding colour on its
      // own canvas, which `drawFade` lays over it at the level it sounds at now.
      f.tctx.clearRect(0, 0, dw, dh);
      drawTake(wctx, lane.cols, n, end, midY, A, step, lane.colors.stopped, LOOK_STOPPED, 0);
      drawTake(f.tctx, lane.cols, n, end, midY, A, step, waveColor(lane.colors, state, false), LOOK_SOUNDING, 0);
    } else {
      const hushed = muted && !capturing;
      const body = hushed ? LOOK_MUTED : state === 'STOPPED' ? LOOK_STOPPED : LOOK_SOUNDING;
      const mul = muted && capturing ? MUTED_CAPTURE : 1;
      const edge = hushed ? (lane.cssW >= PLAYHEAD_WIDE ? MUTED_EDGE.wide : MUTED_EDGE.width) * dpr : 0;
      drawTake(wctx, lane.cols, n, end, midY, A, step, waveColor(lane.colors, state, muted), body, edge, mul);
    }
  }

  // Dotted rec-red "tape to fill" over the remainder to the right of the recorded region.
  if (hasTape) {
    wctx.fillStyle = lane.colors.rec;
    wctx.globalAlpha = 0.5;
    const dashY = Math.round(midY - dpr / 2);
    const dashH = Math.max(1, Math.round(dpr));
    const dashW = Math.max(1, Math.round(3 * dpr));
    const step = Math.max(2, Math.round(7 * dpr));
    for (let x = cols + Math.round(6 * dpr); x < dw; x += step) {
      wctx.fillRect(x, dashY, dashW, dashH);
    }
    wctx.globalAlpha = 1;
  }
}

/** Draw the moving playhead for a track over the (already-blitted) waveform. */
function drawPlayhead(lane: Lane, state: TrackState, waiting: boolean): void {
  const { ctx, dw, dh } = lane;
  let x = -1;
  let color = lane.colors.playhead;

  if (state === 'RECORDING' && waiting) {
    // Armed for the downbeat: an amber (--dub; the lane's ARMED --sc is red, which an armed take never draws) head rides the master phase in
    // sync with the other lanes. A count-in (a first take's, or a later take's from stopped loops, which
    // restart from the top on its downbeat) and AUTO LISTEN have no phase to ride → no head at all.
    if (looper.masterFramesValue() > 0 && !looper.countingValue()) {
      x = looper.phaseValue() * dw;
      color = lane.colors.overdubbing;
    }
  } else if (state === 'RECORDING') {
    // The head sweeps the same span the bins are placed over (`recSpanFrames`); -1 (no track yet): the
    // right edge.
    const frac = looper.recHeadFrac(lane.index);
    x = frac < 0 ? dw - lane.dpr : frac * dw;
    color = lane.colors.rec;
  } else if (state === 'PLAYING' || state === 'OVERDUBBING') {
    // An overdub's head stays warm white like a playing one: the take is amber already, an amber head
    // would vanish inside its body, and the heads line up down the display as one line.
    x = looper.phaseValue() * dw;
  }
  // STOPPED / EMPTY: no playhead.

  if (x < 0) return;
  // One crisp solid line, no glow columns, one width in every state: 2 CSS px, 2.5 on a wide lane (3
  // device px on a 2560 display at 100 %), so the head is found from the chair.
  const coreW = Math.max(1, Math.round(lane.dpr * (lane.cssW >= PLAYHEAD_WIDE ? PLAYHEAD_CSS_WIDE : PLAYHEAD_CSS)));
  const px = Math.round(x - coreW / 2);
  ctx.fillStyle = color;
  ctx.fillRect(px < 0 ? 0 : px > dw - coreW ? dw - coreW : px, 0, coreW, dh);
}

/** Lane `index` fades out and stops at device frame `stopAt` (null: it does not fade). From a Solid
 * effect (`Looper.tsx`); the fade began as the lane's first `stopAt` lands, so its start is the frame heard
 * now (within the feed's latency of the press). */
export function setLaneFade(index: number, stopAt: number | null): void {
  if (stopAt === null) {
    fadeTo[index] = -1;
    return;
  }
  if (fadeTo[index] === stopAt) return;
  fadeTo[index] = stopAt;
  fadeFrom[index] = looper.heardFrameValue();
}

/**
 * Draw a fading lane, one treatment across the whole row: the take's full-size grey silhouette (the
 * lane's bitmap, `rasterise`, as it will look once stopped) and over it the take in its sounding colour a
 * fixed step toward grey (`FADE_STEP`), scaled about the centre line to the level the lane sounds at now.
 * That level is the engine's ramp, r², r falling linearly from the fade's start (`from`) to its stop
 * (`to`), so the colour sinks into the grey as the fade runs and is gone at the stop. Per frame: two
 * blits at integer sizes; no allocation.
 */
function drawFade(lane: Lane, f: Fade, to: number, from: number): void {
  const { ctx, dw, dh } = lane;
  let r = (to - looper.heardFrameValue()) / Math.max(1, to - from);
  r = r < 0 ? 0 : r > 1 ? 1 : r;
  const h = Math.round(dh * r * r);
  ctx.drawImage(lane.wave, 0, 0);
  if (h < 1) return;
  ctx.globalAlpha = FADE_STEP;
  ctx.drawImage(f.take, 0, 0, dw, dh, 0, (dh - h) >> 1, dw, h);
  ctx.globalAlpha = 1;
}

/** The single rAF loop: draw every registered lane, then schedule the next frame. */
function frame(now: number): void {
  for (const lane of lanes.values()) {
    const sizeChanged = syncSize(lane);
    const state = looper.stateOf(lane.index);
    const muted = looper.mutedOf(lane.index);
    const master = looper.masterFramesValue();
    // Armed / count-in / AUTO LISTEN: the chrome says ARMED/LISTENING, so the well draws no rec-red.
    const waiting = state === 'RECORDING' && looper.waitingOf(lane.index);
    // A take laying down audio places its bins over its span: re-placed when that span grows.
    const span = state === 'RECORDING' && !waiting ? looper.recSpanFrames(lane.index) : 0;
    looper.peaksInto(lane.index, peakScratch);
    // A FADE draws on a playing take that sounds (a muted one is grey already) over a running loop.
    const to = fadeTo[lane.index] ?? -1;
    const fade = to >= 0 && state === 'PLAYING' && !muted && master > 0 && peakScratch.count > 0;

    const dirty =
      sizeChanged ||
      peakScratch.version !== lane.lastVersion ||
      state !== lane.lastState ||
      waiting !== lane.lastWaiting ||
      muted !== lane.lastMuted ||
      master !== lane.lastMasterFrames ||
      span !== lane.lastSpan ||
      fade !== lane.lastFading;
    if (dirty) {
      rasterise(lane, peakScratch.count, peakScratch.min, peakScratch.max, state, master, muted, span, fade);
      lane.lastFading = fade;
      lane.lastVersion = peakScratch.version;
      lane.lastWaiting = waiting;
      lane.lastState = state;
      lane.lastMuted = muted;
      lane.lastMasterFrames = master;
      lane.lastSpan = span;
    }

    // Blit + playhead only when the bitmap changed this frame OR the lane carries a MOVING playhead
    // (RECORDING/PLAYING/OVERDUBBING). A static lane (EMPTY/STOPPED) has no playhead — drawPlayhead
    // early-returns at x<0 — so re-blitting it every frame is pixel-identical wasted work (clearRect +
    // drawImage per idle lane). The moving→STOPPED edge is itself a state change, so it lands in `dirty`
    // that frame and re-rasterises playhead-free (erasing the last playhead) before the lane goes quiet.
    // Still no signal reads and no allocation here (invariant 6).
    const moving = state === 'RECORDING' || state === 'PLAYING' || state === 'OVERDUBBING';
    if (dirty || moving) {
      const { ctx, dw, dh } = lane;
      ctx.clearRect(0, 0, dw, dh);
      if (fade && lane.fading && lane.fading.take.width === dw) drawFade(lane, lane.fading, to, fadeFrom[lane.index] ?? 0);
      else ctx.drawImage(lane.wave, 0, 0);
      drawPlayhead(lane, state, waiting);
    }
  }
  drawDial();
  drawMeter();
  for (const m of cardMeters) if (m) drawCardMeter(m, now);
  rafId = requestAnimationFrame(frame);
}

/** Register a canvas that draws track `index`. Starts the shared rAF loop on the first lane. */
export function registerLane(index: number, canvas: HTMLCanvasElement): void {
  const ctx = canvas.getContext('2d');
  const wave = document.createElement('canvas');
  const wctx = wave.getContext('2d');
  if (!ctx || !wctx) {
    console.warn(`[waveform] 2D context unavailable for track ${index}; waveform disabled`);
    return;
  }

  const lane: Lane = {
    index,
    canvas,
    ctx,
    wave,
    wctx,
    colors: readColors(canvas),
    dw: 0,
    dh: 0,
    cssW: -1,
    cssH: -1,
    dpr: 0,
    lastVersion: -2,
    lastWaiting: false,
    lastState: '',
    lastMuted: false,
    lastMasterFrames: -1,
    lastSpan: 0,
    gridMaster: -1,
    gridBars: 0,
    gridFont: gridFontFor(self.devicePixelRatio || 1, canvas.clientHeight),
    cols: new Float32Array(0),
    levels: new Float64Array(0),
    shownMax: new Float32Array(0),
    shownMin: new Float32Array(0),
    slopes: new Float32Array(0),
    lastFading: false,
    fading: null,
  };
  syncSize(lane);
  lanes.set(canvas, lane);
  askGridFont(lane.gridFont);

  if (rafId === 0) rafId = requestAnimationFrame(frame);
}

/** Unregister a canvas. Stops the shared rAF loop when nothing is registered any more. */
export function unregisterLane(canvas: HTMLCanvasElement): void {
  lanes.delete(canvas);
  stopIfIdle();
}
