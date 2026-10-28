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
 * invariant 6) and never scans raw PCM. Per track the waveform (two-layer envelope) AND its bar grid are
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
const GRID_NUM = 'rgba(148,168,215,.26)'; //   engraved bar numbers, top-left of each bar
// The wave's two layers (alpha of the state colour): colour on black, not a block of paint; the body
// over the outer lands near .54, the outer envelope a fainter rim around it. The outer
// peak stays at or over .18 so a take's rec-red reads in the outer envelope alone (wave-steady probe).
const WAVE_OUTER = 0.2;
const WAVE_BODY = 0.42;
/** The centre line's alpha in --faint: a guide under the wave, quieter than any wave. */
const MID_ALPHA = 0.45;
/** The meter's counted beats a bar (`setGridBeats`): the grid's beat lines, a bar line every `gridBeats`. */
let gridBeats = 4;

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
  /** Cached bar-number font string (depends only on dpr) — so rasterise allocates nothing. */
  gridFont: string;
}

/** The bar-number font at a given device-pixel ratio (7.5px CSS): Geist Mono, the `--mono` token's
 * stack (`src/app.css`). */
function gridFontFor(dpr: number): string {
  return `${(7.5 * dpr).toFixed(1)}px 'Geist Mono', ui-monospace, Consolas, monospace`;
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

/** Pick the waveform colour for a track state. */
function waveColor(c: LaneColors, state: TrackState): string {
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
  lane.gridFont = gridFontFor(dpr);
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
  wctx.fillStyle = GRID_NUM;
  wctx.font = lane.gridFont;
  wctx.textBaseline = 'top';
  const pad = Math.round(5 * dpr);
  const top = Math.round(3 * dpr);
  for (let b = 0; b < bars; b++) {
    wctx.fillText(String(b + 1), Math.round((b / bars) * dw) + pad, top);
  }
}

/**
 * Rasterise the bar grid + two-layer waveform envelope into the off-screen bitmap. All of this is the
 * CACHED-BITMAP path — it runs only on a dirty frame, never in the per-frame steady state.
 *
 * The envelope is drawn per device-pixel column: a translucent OUTER min/max peak (`WAVE_OUTER`)
 * plus a denser INNER body (`WAVE_BODY`) whose height is a one-pole-smoothed mean-|peak| — the
 * RMS-ish core, mirrored around mid. We only have min/max bins, so the per-column mean is approximated
 * as (|min|+|max|)/2. While a take records (`span` > 0, its `recSpanFrames`) each bin sits at its frame
 * over the span, so the recorded region covers `cols` columns and, on a later take, the remainder gets
 * the dotted rec-red "tape to fill" guide.
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
): void {
  const { wctx, dw, dh, dpr } = lane;
  wctx.clearRect(0, 0, dw, dh);
  wctx.globalAlpha = 1;

  const midY = dh / 2;
  const A = midY * 0.86; // peak amplitude in px — 14% headroom so loud transients don't kiss the edge

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

  if (!hasTape) {
    wctx.fillStyle = lane.colors.mid;
    wctx.globalAlpha = MID_ALPHA;
    wctx.fillRect(0, Math.round(midY), dw, 1);
    wctx.globalAlpha = 1;
  }

  // Two-layer envelope. Down-samples the peak bins across `cols` device columns: with more bins than
  // columns we take the extremes per column group, with fewer we stretch. O(cols + count).
  if (count > 0 && min && max && cols >= 1) {
    // STOPPED reads as a dimmed take; MUTED dims further so a silent lane is never mistaken for a
    // sounding one (the state word says MUTED, the lane's --sc goes grey — this is the wave's half).
    const dimMul = muted ? 0.3 : state === 'STOPPED' ? 0.55 : 1;
    wctx.fillStyle = waveColor(lane.colors, state);
    let body = 0;
    for (let x = 0; x < cols; x++) {
      const p0 = Math.floor((x / dw) * across);
      if (p0 >= count) break;
      let p1 = Math.floor(((x + 1) / dw) * across);
      if (p1 <= p0) p1 = p0 + 1;
      if (p1 > count) p1 = count;
      let mn = min[p0];
      let mx = max[p0];
      for (let p = p0 + 1; p < p1; p++) {
        if (min[p] < mn) mn = min[p];
        if (max[p] > mx) mx = max[p];
      }
      // One-pole-smoothed mean-|peak| → the inner body track (RMS-ish core).
      const absMn = mn < 0 ? -mn : mn;
      const absMx = mx < 0 ? -mx : mx;
      body = body * 0.72 + ((absMn + absMx) / 2) * 0.28;

      // Outer peak envelope (translucent). Clamp sample range to [-1,1] so loud transients stay on-canvas.
      const mxc = mx > 1 ? 1 : mx;
      const mnc = mn < -1 ? -1 : mn;
      let y1 = midY - mxc * A;
      let y2 = midY - mnc * A;
      if (y1 < 0) y1 = 0;
      if (y2 > dh) y2 = dh;
      wctx.globalAlpha = WAVE_OUTER * dimMul;
      wctx.fillRect(x, y1, 1, Math.max(1, y2 - y1));

      // Inner body (solid), mirrored around mid.
      const hb = Math.max(1, Math.min(1, body * 1.7) * A);
      wctx.globalAlpha = WAVE_BODY * dimMul;
      wctx.fillRect(x, midY - hb, 1, hb * 2);
    }
    wctx.globalAlpha = 1;
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
    // Armed for the downbeat: an amber (--dub, the lane's ARMED --sc) head rides the master phase in
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
    x = looper.phaseValue() * dw;
    if (state === 'OVERDUBBING') color = lane.colors.overdubbing;
  }
  // STOPPED / EMPTY: no playhead.

  if (x < 0) return;
  const coreW = lane.dpr < 1 ? 1 : Math.round(lane.dpr);
  const px = Math.round(x);
  // One crisp solid line, one CSS px wide: no glow columns.
  ctx.fillStyle = color;
  ctx.fillRect(px, 0, coreW, dh);
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

    const dirty =
      sizeChanged ||
      peakScratch.version !== lane.lastVersion ||
      state !== lane.lastState ||
      waiting !== lane.lastWaiting ||
      muted !== lane.lastMuted ||
      master !== lane.lastMasterFrames ||
      span !== lane.lastSpan;
    if (dirty) {
      rasterise(lane, peakScratch.count, peakScratch.min, peakScratch.max, state, master, muted, span);
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
      ctx.drawImage(lane.wave, 0, 0);
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
    gridFont: gridFontFor(self.devicePixelRatio || 1),
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
