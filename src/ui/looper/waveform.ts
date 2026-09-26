import { clock, looper, sampleRate, type PeakView, type TrackState } from '../state/audio';
import { masterBars } from './shared';

/**
 * Waveform + bar-grid + playhead renderer.
 *
 * A SINGLE requestAnimationFrame loop drives every registered track canvas. It reads the looper's
 * pre-computed peak arrays through non-reactive getters (`looper.peaksInto` / `phaseValue` / `stateOf`
 * / `recHeadFrac` / `masterFramesValue` — NO Solid signals in the per-frame path, per invariant 6) and
 * never scans raw PCM. Per track the waveform (two-layer envelope) AND its bar grid are rasterised into
 * an off-screen canvas and cached; the bitmap is re-rasterised ONLY when the peaks change (version
 * bump), the track state changes (colour), the master loop length changes (grid geometry), or the
 * element resizes. Every frame the loop just blits that cached bitmap and draws the playhead — so the
 * steady-state per-frame cost is a `drawImage` + a few `fillRect`s per track, holding 60fps with flat
 * GC even with all five tracks live.
 *
 * The bar grid is derived from `masterBars()` (the shared bar-math in `shared.ts`) so it can
 * never disagree with the spoken loop length. That is the ONLY place Solid signals are read
 * (`clock.bpm()`, and in engine mode the device's rate behind `sampleRate()`), and it is gated to a
 * master-length change (a rare structural event; BPM is locked for the life of a committed master)
 * inside the cached-bitmap path — never in the per-frame steady state.
 *
 * Solid only ever creates/destroys the <canvas> elements (via the Looper component) and calls
 * `registerLane` / `unregisterLane`; all drawing lives here in plain TS.
 */

// Bar-grid tints — constant, not theme-dependent (based on the --surf-1 base tint).
const GRID_BEAT = 'rgba(148,168,215,.085)'; // beat lines — a whisper (bumped from .055: the --well lift to .04 same-hue was swallowing them)
const GRID_BAR = 'rgba(148,168,215,.13)'; //   bar lines — readable
const GRID_NUM = 'rgba(148,168,215,.28)'; //   engraved bar numbers, top-left of each bar
const BEATS_PER_BAR = 4;

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
  /** Grid cache: the bar count for `gridMaster`, recomputed (reading bpm) only when master changes. */
  gridMaster: number;
  gridBars: number;
  /** Cached bar-number font string (depends only on dpr) — so rasterise allocates nothing. */
  gridFont: string;
}

/** The bar-number font at a given device-pixel ratio (7.5px CSS, mono). */
function gridFontFor(dpr: number): string {
  return `${(7.5 * dpr).toFixed(1)}px ui-monospace, 'SF Mono', Consolas, monospace`;
}

const lanes = new Map<number, Lane>();

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
 * aria-valuenow/aria-valuetext refreshed ~6×/s. Transport.tsx puts the AUTO threshold on the same
 * scale via `meterFrac` (a low-frequency effect, not this loop).
 */
let meter: { el: HTMLElement; last: number; hot: boolean; ariaTick: number } | null = null;

const METER_FLOOR_DB = -60;

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
  const hot = raw >= 0.89;
  if (hot !== meter.hot) {
    meter.hot = hot;
    meter.el.classList.toggle('is-hot', hot);
  }
  if (++meter.ariaTick >= 10) {
    meter.ariaTick = 0;
    meter.el.setAttribute('aria-valuenow', lvl.toFixed(2));
    const db = Math.round(METER_FLOOR_DB + -METER_FLOOR_DB * lvl);
    meter.el.setAttribute('aria-valuetext', `${db} dBFS${hot ? ', clipping' : ''}`);
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
  if (lanes.size === 0 && dial === null && meter === null && rafId !== 0) {
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
  const total = bars * BEATS_PER_BAR;
  for (let i = 1; i < total; i++) {
    const gx = Math.round((i / total) * dw);
    wctx.fillStyle = i % BEATS_PER_BAR === 0 ? GRID_BAR : GRID_BEAT;
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
 * The envelope is drawn per device-pixel column: a translucent OUTER min/max peak (globalAlpha .34)
 * plus a solid INNER body (globalAlpha .95) whose height is a one-pole-smoothed mean-|peak| — the
 * RMS-ish core, mirrored around mid. We only have min/max bins, so the per-column mean is approximated
 * as (|min|+|max|)/2. During a later-track take the recorded region occupies only `waveCols` (the
 * record-head fraction of the width) and the remainder gets the dotted rec-red "tape to fill" guide.
 */
function rasterise(
  lane: Lane,
  count: number,
  min: Float32Array | null,
  max: Float32Array | null,
  state: TrackState,
  masterFrames: number,
  muted: boolean,
  waiting: boolean,
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
    lane.gridBars = masterFrames > 0 ? masterBars(masterFrames, clock.bpm(), sampleRate()) : 0;
  }
  drawGrid(lane, lane.gridBars);

  // Centre line vs "tape to fill": while a later track is laying down a take, the mid line becomes a
  // dotted rec-red guide over the not-yet-recorded remainder (right of the record head); otherwise it
  // is a plain faint centre line. (First-track grow-from-left has no known length → plain line, no tape.)
  // Armed / count-in / listening is not a take yet: plain line, no tape (the chrome says ARMED).
  const recording = state === 'RECORDING' && !waiting;
  const headFrac = recording ? looper.recHeadFrac(lane.index) : -1;
  const hasTape = recording && headFrac >= 0;
  const waveCols = hasTape ? Math.min(dw, Math.max(0, Math.round(headFrac * dw))) : dw;

  if (!hasTape) {
    wctx.fillStyle = lane.colors.mid;
    wctx.fillRect(0, Math.round(midY), dw, 1);
  }

  // Two-layer envelope. Down-samples `count` peak bins across `waveCols` device columns: with more bins
  // than columns we take the extremes per column group, with fewer we stretch. O(waveCols + count).
  if (count > 0 && min && max && waveCols >= 1) {
    // STOPPED reads as a dimmed take; MUTED dims further so a silent lane is never mistaken for a
    // sounding one (the state word says MUTED, the lane's --sc goes grey — this is the wave's half).
    const dimMul = muted ? 0.3 : state === 'STOPPED' ? 0.55 : 1;
    wctx.fillStyle = waveColor(lane.colors, state);
    let body = 0;
    for (let x = 0; x < waveCols; x++) {
      const p0 = Math.floor((x / waveCols) * count);
      let p1 = Math.floor(((x + 1) / waveCols) * count);
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
      wctx.globalAlpha = 0.34 * dimMul;
      wctx.fillRect(x, y1, 1, Math.max(1, y2 - y1));

      // Inner body (solid), mirrored around mid.
      const hb = Math.max(1, Math.min(1, body * 1.7) * A);
      wctx.globalAlpha = 0.95 * dimMul;
      wctx.fillRect(x, midY - hb, 1, hb * 2);
    }
    wctx.globalAlpha = 1;
  }

  // Dotted rec-red "tape to fill" over the remainder to the right of the record head.
  if (hasTape) {
    wctx.fillStyle = lane.colors.rec;
    wctx.globalAlpha = 0.5;
    const dashY = Math.round(midY - dpr / 2);
    const dashH = Math.max(1, Math.round(dpr));
    const dashW = Math.max(1, Math.round(3 * dpr));
    const step = Math.max(2, Math.round(7 * dpr));
    for (let x = waveCols + Math.round(6 * dpr); x < dw; x += step) {
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
    // sync with the other lanes. Count-in / AUTO LISTEN have no master yet → no head at all.
    if (looper.masterFramesValue() > 0) {
      x = looper.phaseValue() * dw;
      color = lane.colors.overdubbing;
    }
  } else if (state === 'RECORDING') {
    const frac = looper.recHeadFrac(lane.index);
    // -1 => first track, master not yet defined: head rides the right edge of the grown data.
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
  ctx.fillStyle = color;
  // Soft glow: one column each side at ~25% alpha (no shadowBlur in the frame loop). Then the solid core.
  ctx.globalAlpha = 0.25;
  ctx.fillRect(px - coreW, 0, coreW, dh);
  ctx.fillRect(px + coreW, 0, coreW, dh);
  ctx.globalAlpha = 1;
  ctx.fillRect(px, 0, coreW, dh);
}

/** The single rAF loop: draw every registered lane, then schedule the next frame. */
function frame(): void {
  for (const lane of lanes.values()) {
    const sizeChanged = syncSize(lane);
    const state = looper.stateOf(lane.index);
    const muted = looper.mutedOf(lane.index);
    const master = looper.masterFramesValue();
    // Armed / count-in / AUTO LISTEN: the chrome says ARMED/LISTENING, so the well draws no rec-red.
    const waiting = state === 'RECORDING' && looper.waitingOf(lane.index);
    looper.peaksInto(lane.index, peakScratch);

    const dirty =
      sizeChanged ||
      peakScratch.version !== lane.lastVersion ||
      state !== lane.lastState ||
      waiting !== lane.lastWaiting ||
      muted !== lane.lastMuted ||
      master !== lane.lastMasterFrames;
    if (dirty) {
      rasterise(lane, peakScratch.count, peakScratch.min, peakScratch.max, state, master, muted, waiting);
      lane.lastVersion = peakScratch.version;
      lane.lastWaiting = waiting;
      lane.lastState = state;
      lane.lastMuted = muted;
      lane.lastMasterFrames = master;
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
  rafId = requestAnimationFrame(frame);
}

/** Register a track's canvas with the renderer. Starts the shared rAF loop on the first lane. */
export function registerLane(index: number, canvas: HTMLCanvasElement): void {
  // Defensive: never keep two lanes for one index (a remount without cleanup would leak).
  if (lanes.has(index)) unregisterLane(index);
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
    gridMaster: -1,
    gridBars: 0,
    gridFont: gridFontFor(self.devicePixelRatio || 1),
  };
  syncSize(lane);
  lanes.set(index, lane);

  if (rafId === 0) rafId = requestAnimationFrame(frame);
}

/** Unregister a track's canvas. Stops the shared rAF loop when the last lane goes away. */
export function unregisterLane(index: number): void {
  lanes.delete(index);
  stopIfIdle();
}
