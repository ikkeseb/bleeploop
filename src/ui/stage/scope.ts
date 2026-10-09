import { PEAK_FRAMES, SCOPE_MASTER, SCOPE_MONITOR, looper, type ScopeView } from '../state/audio';
import { LANES, feed } from './stage-feed';
import {
  ALPHAS,
  clamp,
  contourAlpha,
  contourColor,
  createStamp,
  decay,
  env,
  holdsAudio,
  hudPad,
  laneChanged,
  light,
  lightColor,
  peaks,
  sampleEnvelope,
  waitColor,
  type Palette,
  type StageViewDef,
  type Surface,
  type ViewDraw,
} from './visual';

/**
 * SCOPE: six bays stacked in the HUD's insets. The top one is the hero, two lane-heights tall: the
 * engine's whole output as an additively composited body with the live monitor (the guitar) as a cyan
 * line over it. Under it one bay per lane, each a live scope of that lane after its FX.
 *
 * Every bay draws two layers. Underneath, the lane's RECORDED take as a dim static contour (the
 * looper lane's two-layer envelope in `contourColor`'s state colour), which says *this lane holds
 * eight bars shaped like this* and is the only thing left when the lane is muted, stopped or
 * recording. Over it the live light: the engine's scope columns, drawn additively, so a pass that
 * lands on the pass before it accumulates and a locked loop GLOWS while a one-off peaks and dies.
 * Light is the proof; nothing has to move for the picture to say which lanes are locked.
 *
 * Each column is placed by its OWN device frame, not by counting back from the playhead: its loop
 * phase is `(frame - the master grid's anchor) mod the sweep's span`, which is exact and jitter-free
 * at any loop length. The newest column therefore sits one output latency ahead of the drawn
 * playhead, and the beam rides the newest column, which is the honest place for it.
 *
 * Two canvases, both sized in `layout` and never per frame: a still bitmap (the wells, the hairlines,
 * the bar ticks, the bay names, the selection mark and every lane's contour), invalidated per bay by
 * `laneChanged` and whole when the layout, the selection or the loop's bar count moves; and the trace,
 * which holds only the live columns. A frame is two blits plus a handful of column strips and the
 * beams, all at whole device px with alphas from `visual.ts`'s table, so this module's own frame code
 * allocates nothing in steady playback (the stage-view probe's budget case). Fractional state across
 * frames lives in typed arrays: a fractional double in a closure variable is boxed on every store.
 */

const BAYS = LANES + 1; // the hero, then one bay per lane
const SOURCES = SCOPE_MASTER + 1; // a batch's sources: lanes 0..4, the monitor, the master output
const HERO = 2; // the hero's height, in lane units
const UNITS = HERO + LANES;
const BAND_TOP = 0.1; // of the height: under the chips
const BAND = 0.78; // of the height: above the message line
const GUTTER = 44; // CSS px: the column that carries the bay names
const LANE_NAMES = ['1', '2', '3', '4', '5'];
/** With no master loop the sweep is a fixed window, so a first take still sweeps (about 2 s at 48 kHz). */
const FALLBACK_COLUMNS = 512;
/** Below this a column is silence and draws nothing: a resting lane rests, and no input hum paints a
 * permanent line (0.01 is about -40 dB). */
const FLOOR = 0.01;
/** The bar ticks, in the looper grid's tint. Deliberately under the pixel census's floor (max channel
 * 40) over `--surf-1`: the chrome covers whole bays, and the census counts it against a playing lane's
 * light (`empty.lit < play.lit / 3`). */
const TICK = 'rgba(148,168,215,.07)';
/** The well's one hairline of top light, and the selected bay's tone step: both under that same floor
 * over `--surf-1`, which is why the step starts below the hairline rather than under it. */
const HAIR_A = 0.06;
const STEP_A = 0.045;
/**
 * The look's two knobs. Under `'lighter'`, light added at alpha ADD and dimmed by FADE before the next
 * pass lands converges on ADD / FADE of its colour: a locked loud lane settles near 0.75 of its own
 * green while a one-off tops out at 0.30 and dies. The hero's body adds less because warm white is the
 * palette's loudest colour and `--engaged` means SELECTED in this app: 0.22 / 0.4 settles near 0.55, so
 * a locked loop can never read as selected. Turn ADD_* for how hot one pass reads, FADE for how many
 * passes a lane takes to settle.
 *
 * THE BUDGET, which a new source must not break: the additive alpha one pass lands on ONE pixel, summed
 * over every pass drawn there, stays at or under 0.32, so no pixel's ceiling passes 0.8 of its colour.
 * The hero is the only bay where two sources meet, and during a first take the master IS nearly the
 * monitor, so their forms coincide almost exactly: the master's body (0.22) plus the monitor's glow
 * (0.1) is the whole budget, and the monitor's own line is written `'source-over'` rather than added,
 * which cannot blow out. Live guitar never locks, so accumulation buys the monitor nothing.
 */
const ADD_LANE = 0.3;
const ADD_MASTER = 0.22;
const ADD_MON_GLOW = 0.1;
const MON_LINE = 0.9; // written, not added: no term in the budget
const FADE = 0.4;
/** The beam at the newest column: one wide faint pass and one narrow bright one, per bay, in that
 * bay's own state colour. The core's alpha is what the pixel census reads as the bay's state. */
const BEAM_CORE = 0.85;
const BEAM_GLOW = 0.12;
/** How long an armed lane's hairline carries the beat it was struck on. */
const BEAT_MS = 420;
/** The column stream stopped (the device stopped, a fault, the taps closed, a held feed). The
 * per-column dim only erases where a new column lands, so after this long with no column the whole
 * trace fades out over IDLE_FADE_MS and then stops compositing until columns return. */
const IDLE_WAIT_MS = 250;
const IDLE_FADE_MS = 400;
/** The contour reaches a little past the live light's 70 % of the bay's half height. */
const LIVE_REACH = 0.35;
const CONTOUR_REACH = 0.4;

function createScope(): ViewDraw {
  const back = document.createElement('canvas'); // the still picture: chrome and contours
  const bctx = back.getContext('2d');
  const trace = document.createElement('canvas'); // the live columns alone
  const tctx = trace.getContext('2d');
  const stamp = createStamp();
  const A = ALPHAS;
  const view: ScopeView = { lo: null, hi: null, at: 0, count: 0, frame: 0, bin: 0, epoch: 0 };
  // One frame's new columns, folded onto device-pixel columns: the x, and per source the lowest low
  // and the highest high that landed there. A frame holds a handful; a stall is capped at the ring.
  const gx = new Int32Array(1024);
  // Each folded column's own width, so the strips TILE the trace instead of overlapping. `cw` is the
  // nominal width rounded up while an x is rounded down, so two neighbours overlap by a pixel whenever
  // the sweep does not divide the trace evenly (any master loop under about 7 s at 1080p). An
  // overlapped pixel took its dim and its light twice a pass, which lifts its converged brightness
  // above `added alpha / fade` and lets the hero bay climb toward the warm white that means SELECTED
  // here. Brightness would then measure column geometry as well as repetition, which is the one thing
  // this look claims to show.
  const gw = new Int32Array(1024);
  const glo = new Float32Array(1024 * SOURCES);
  const ghi = new Float32Array(1024 * SOURCES);
  // Device px, whole numbers, laid out once (`layout`).
  const bayY = new Int32Array(BAYS);
  const bayH = new Int32Array(BAYS);
  const reach = new Float64Array(BAYS);
  let dpr = 1;
  let W = 0;
  let top = 0;
  let th = 0;
  let unit = 0;
  let px = 0; // the HUD's side inset
  let ww = 0; // and the well's width inside it
  let x0 = 0; // the trace's left edge, past the gutter
  let tw = 0; // and its width
  let hair = 1;
  let line = 2;
  let core = 2;
  let glow = 14;
  let edge = 6;
  let grow = 2; // how far the monitor's glow reaches past its line
  let nameFont = '';
  let nameX = 0;
  // CSS px, for the hit test.
  let laneTopCss = 0;
  let laneCss = 1;
  // What the still bitmap holds.
  let shownSel = -1;
  let shownBeats = -1;
  let resized = true;
  // The trace's own state. The newest column drawn and the sweep it was placed in are device frames:
  // they live in a typed array, not in a closure variable that would box on every store.
  const mem = new Float64Array(3);
  const DRAWN = 0; // the newest column already drawn, -1 before the first
  const SPAN = 1; // the sweep's span in frames
  const GRID = 2; // and the grid anchor it was placed against
  let drawnEpoch = -1;
  let drawnBin = 0;
  let cw = 1; // one column's width in device px
  /** `beam[0]`: where the beam stands, the newest column's x in the trace, -1 while the trace is out of
   * step with the mirror (no beam this frame), -2 while no column has arrived at all (the drawn
   * playhead carries it). `beam[1]`: whether the engine's columns are still arriving. Int32 slots
   * rather than return values, which box crossing a frame's call. */
  const beam = new Int32Array(2);
  const ALIVE = 1;
  /**
   * The device-pixel column still being folded. A batch ends wherever the engine's columns fall, not on
   * a pixel boundary (about 4.17 columns arrive a frame, and about 1.46 of them share a pixel on a 16 s
   * loop at 48 kHz), so the column a frame leaves half-folded has to survive into the next one. It is
   * neither dimmed nor drawn until a column with another x arrives, so every pixel column takes exactly
   * ONE dim and ONE light per pass. Folding per frame instead would dim a pixel that straddled a frame
   * boundary twice, and the light would then measure how many columns landed in a pixel rather than how
   * often the pass repeated, which is the one thing this look claims to show.
   */
  let pendX = -1;
  const pendLo = new Float32Array(SOURCES);
  const pendHi = new Float32Array(SOURCES);
  /** The trace holds light worth compositing. */
  let held = false;
  /** When a column last landed, when the idle fade began and the share of the trace still standing
   * (`performance.now()`'s clock). Read in `draw` only: a double read inside `pullColumns` boxes. */
  const idle = new Float64Array(3);
  const SEEN = 0;
  const FROM = 1;
  const LEFT = 2;

  function layout(s: Surface): void {
    dpr = s.dpr;
    W = s.dw;
    top = Math.round(BAND_TOP * s.dh);
    unit = Math.max(6, Math.floor((BAND * s.dh) / UNITS));
    th = UNITS * unit;
    px = Math.round(hudPad(s.h) * dpr);
    ww = Math.max(8, W - 2 * px);
    x0 = px + Math.round(GUTTER * dpr);
    tw = Math.max(8, W - px - x0);
    for (let b = 0; b < BAYS; b++) {
      bayY[b] = b === 0 ? 0 : HERO * unit + (b - 1) * unit;
      bayH[b] = b === 0 ? HERO * unit : unit;
      reach[b] = bayH[b] * LIVE_REACH;
    }
    hair = Math.max(1, Math.round(dpr));
    line = Math.max(1, Math.round(1.5 * dpr));
    core = Math.max(2, Math.round(2 * dpr));
    glow = Math.max(core + 2, Math.round(14 * dpr));
    edge = Math.round(6 * dpr);
    grow = Math.max(1, Math.round(2 * dpr));
    // Geist Mono in a full-screen performance view reads as an instrument panel; the app's own rule
    // reserving it for changing numerals is written for the command bar (the spec's call).
    nameFont = `${Math.round(12 * dpr)}px 'Geist Mono', ui-monospace, monospace`;
    nameX = px + Math.round(11 * dpr);
    laneTopCss = (top + HERO * unit) / dpr;
    laneCss = unit / dpr;
    back.width = W;
    back.height = th;
    trace.width = tw;
    trace.height = th;
    s.countX = s.w / 2;
    s.countY = (top + th / 2) / dpr;
    s.countSize = clamp(0.36 * s.h, 140, 480);
    mem[DRAWN] = -1;
    mem[SPAN] = 0;
    drawnBin = 0;
    drawnEpoch = -1;
    pendX = -1;
    held = false;
    resized = true;
  }

  /** Bay `b`'s well over [x, x + w): the tone step, its one hairline of top light, the bar ticks. */
  function chrome(pal: Palette, b: number, x: number, w: number): void {
    const c = bctx;
    if (!c) return;
    const y = bayY[b];
    const h = bayH[b];
    c.globalAlpha = 1;
    c.fillStyle = pal.surf1;
    c.fillRect(x, y, w, h);
    if (b > 0 && b - 1 === feed.selected) {
      c.globalAlpha = A[(STEP_A * 255) | 0];
      c.fillStyle = pal.text;
      c.fillRect(x, y + hair, w, h - hair);
    }
    c.globalAlpha = A[(HAIR_A * 255) | 0];
    c.fillStyle = pal.text;
    c.fillRect(x, y, w, hair);
    c.globalAlpha = 1;
    // The grid still has to read inside a multi-bar sweep; a first take has no bars yet.
    const bars = feed.beatsPerLoop >> 2;
    if (bars > 0) {
      c.fillStyle = TICK;
      for (let k = 0; k < bars; k++) {
        const tx = x0 + (((k / bars) * tw) | 0);
        if (tx >= x && tx < x + w) c.fillRect(tx, y, hair, h);
      }
    }
  }

  /** The whole still bitmap: every well, the bay names in the gutter and the selection's mark. Each
   * lane's contour follows from its own band redraw (the stamps are forced stale). */
  function redrawAll(pal: Palette): void {
    const c = bctx;
    if (!c) return;
    c.clearRect(0, 0, W, th);
    for (let b = 0; b < BAYS; b++) chrome(pal, b, px, ww);
    c.globalAlpha = 1;
    c.fillStyle = pal.engaged;
    c.fillRect(px, bayY[feed.selected + 1], edge, unit);
    c.font = nameFont;
    c.textBaseline = 'middle';
    c.textAlign = 'left';
    c.fillStyle = pal.faint;
    c.fillText('OUT', nameX, bayY[0] + (unit >> 1));
    c.fillStyle = pal.cyan; // IN takes the monitor line's own colour
    c.fillText('IN', nameX, bayY[0] + unit + (unit >> 1));
    for (let i = 0; i < LANES; i++) {
      c.fillStyle = i === feed.selected ? pal.engaged : pal.faint;
      c.fillText(LANE_NAMES[i], nameX, bayY[i + 1] + (unit >> 1));
    }
    shownSel = feed.selected;
    shownBeats = feed.beatsPerLoop;
    resized = false;
    for (let i = 0; i < LANES; i++) stamp.version[i] = -2;
  }

  /** One layer of the envelope over `cols` columns of the trace rectangle: up by `hi`, down by `lo`. */
  function layer(c: CanvasRenderingContext2D, cols: number, mid: number, r: number, hi: Float32Array, lo: Float32Array | null): void {
    c.beginPath();
    c.moveTo(x0, mid - Math.max(0.5, r * hi[0]));
    for (let x = 1; x < cols; x++) c.lineTo(x0 + x + 0.5, mid - Math.max(0.5, r * hi[x]));
    c.lineTo(x0 + cols, mid);
    for (let x = cols - 1; x >= 0; x--) c.lineTo(x0 + x + 0.5, mid + Math.max(0.5, r * (lo ? -lo[x] : hi[x])));
    c.closePath();
    c.fill();
  }

  /**
   * Lane `i`'s band of the still bitmap: its recorded take as the dim contour under the live light, in
   * two passes over each other (the looper lane's envelope). Only the trace rectangle is repainted, so
   * the gutter's name and the selection's edge survive a recording take's redraw every frame.
   */
  function rasterise(pal: Palette, i: number): void {
    const c = bctx;
    if (!c) return;
    const b = i + 1;
    c.clearRect(x0, bayY[b], tw, unit);
    chrome(pal, b, x0, tw);
    const span = stamp.span[i];
    looper.peaksInto(i, peaks);
    const cols = holdsAudio(feed.kind[i]) ? sampleEnvelope(tw, span > 0 ? span / PEAK_FRAMES : peaks.count) : 0;
    if (cols <= 0) return;
    const mid = bayY[b] + (unit >> 1);
    const r = unit * CONTOUR_REACH;
    const mul = contourAlpha(i);
    c.fillStyle = contourColor(pal, i);
    c.globalAlpha = A[(0.34 * mul * 255) | 0];
    layer(c, cols, mid, r, env.hi, env.lo);
    c.globalAlpha = A[(0.95 * mul * 255) | 0];
    layer(c, cols, mid, r, env.body, null);
    c.globalAlpha = 1;
  }

  /**
   * Source `src`'s folded columns into bay `bay`: one filled strip per column, from its low to its high
   * over the bay's half height, at `ai` of 255 of `color`. Every fractional value is read inside, so
   * nothing boxes crossing in.
   */
  function addBody(t: CanvasRenderingContext2D, src: number, bay: number, color: string, ai: number, ng: number): void {
    const y = bayY[bay];
    const lim = y + bayH[bay];
    const my = y + (bayH[bay] >> 1);
    const r = reach[bay];
    t.fillStyle = color;
    t.globalAlpha = A[ai];
    for (let g = 0; g < ng; g++) {
      const o = g * SOURCES + src;
      const up = ghi[o];
      const dn = glo[o];
      if (up < FLOOR && dn > -FLOOR) continue;
      let ya = my - ((r * up + 0.5) | 0);
      let yb = my + ((r * -dn + 0.5) | 0);
      if (yb <= ya) yb = ya + 1; // a column that holds sound draws at least a hairline
      if (ya < y) ya = y;
      if (yb > lim) yb = lim;
      if (yb > ya) t.fillRect(gx[g], ya, gw[g], yb - ya);
    }
  }

  /**
   * The same columns as the envelope's two EDGES, each `rh` tall and grown by `pad`: the monitor's line
   * over the master's body. A filled body there would double the alpha landing on one pixel wherever
   * the two forms coincide, which during a first take is everywhere (the master IS nearly the monitor),
   * and the hero bay would climb toward white. The two rows merge where they meet, so a quiet line
   * takes its alpha once rather than twice.
   */
  function addEdges(t: CanvasRenderingContext2D, src: number, bay: number, color: string, ai: number, ng: number, pad: number, rh: number): void {
    const y = bayY[bay];
    const lim = y + bayH[bay];
    const my = y + (bayH[bay] >> 1);
    const r = reach[bay];
    t.fillStyle = color;
    t.globalAlpha = A[ai];
    for (let g = 0; g < ng; g++) {
      const o = g * SOURCES + src;
      const up = ghi[o];
      const dn = glo[o];
      if (up < FLOOR && dn > -FLOOR) continue;
      const ya = my - ((r * up + 0.5) | 0);
      const yb = my + ((r * -dn + 0.5) | 0);
      let a0 = ya - pad;
      let a1 = ya + rh + pad;
      let b0 = yb - rh - pad;
      let b1 = yb + pad;
      if (b0 <= a1) {
        a1 = b1; // the rows meet: one strip, and nothing is left for the second
        b0 = b1;
      }
      if (a0 < y) a0 = y;
      if (a1 > lim) a1 = lim;
      if (a1 > a0) t.fillRect(gx[g], a0, gw[g], a1 - a0);
      if (b0 < y) b0 = y;
      if (b1 > lim) b1 = lim;
      if (b1 > b0) t.fillRect(gx[g], b0, gw[g], b1 - b0);
    }
  }

  /**
   * The columns the engine folded since the last frame, placed by their own device frames. Each new
   * column's own strip is dimmed across the full trace height first, then every source's light is
   * added over it: the per-pass arithmetic of a sweep-wide fade without a brightness step once a loop,
   * and two composite-mode changes a frame rather than two a column. Leaves the beam's x in `beam`.
   */
  function pullColumns(pal: Palette, quiet: boolean): void {
    looper.scopeInto(view);
    const t = tctx;
    const lo = view.lo;
    const hi = view.hi;
    const bin = view.bin;
    beam[ALIVE] = 0;
    if (!t || !lo || !hi || bin <= 0 || view.count <= 0) {
      // No column has arrived: the taps just opened, or this is the browser rig, whose engine fake
      // sends none. The beam rides the drawn playhead until the engine's own frames can place it.
      beam[0] = -2;
      return;
    }
    const master = looper.masterFramesValue();
    const span = master > 0 ? master : FALLBACK_COLUMNS * bin;
    // Loop position 0 plays at the master grid's anchor, so a column's phase is its own frame's.
    const grid = looper.gridValue();
    if (span !== mem[SPAN] || grid !== mem[GRID] || bin !== drawnBin) {
      // The sweep's length or its anchor moved, so every column already on the canvas stands at an x
      // that now means something else. This is the one case that clears the trace: the master's length
      // changed (a new engine, whose master goes to 0, comes through here too), or the grid was
      // re-anchored, which only a deliberate transport event does (a restart, `lf-engine`'s looper.rs),
      // and a fresh sweep there is honest. An xrun is not that: it keeps the no-clear path below.
      t.clearRect(0, 0, tw, th);
      held = false;
      pendX = -1;
      mem[SPAN] = span;
      mem[GRID] = grid;
      drawnBin = bin;
      mem[DRAWN] = view.frame - bin;
      cw = Math.max(1, Math.ceil((tw * bin) / span));
    }
    if (mem[DRAWN] < 0) mem[DRAWN] = view.frame - bin;
    let p = (view.frame - grid) % span;
    if (p < 0) p += span;
    let newest = ((p / span) * tw) | 0;
    if (newest >= tw) newest = tw - 1;
    beam[0] = newest;
    if (view.epoch !== drawnEpoch) {
      // The trace broke (an xrun, a ring overrun, a new engine). Resynchronise and skip one beam, but
      // never wipe the trace: one 4 ms window is not worth a full-screen blink mid-jam, and the next
      // pass's own dimming carries the old light away.
      drawnEpoch = view.epoch;
      mem[DRAWN] = view.frame;
      pendX = -1;
      beam[0] = -1;
      beam[ALIVE] = 1;
      return;
    }
    let n = Math.round((view.frame - mem[DRAWN]) / bin);
    mem[DRAWN] = view.frame;
    if (n <= 0) return;
    beam[ALIVE] = 1; // the stream is alive, whatever this frame's columns fold into
    const most = Math.min(view.count, Math.ceil(span / bin), gx.length); // after a stall, one sweep is all the x there are
    if (n > most) {
      // Columns are being discarded, so the pending one is no longer the neighbour of the first kept
      // one: folding them together would mix audio from both sides of a gap nothing marked.
      n = most;
      pendX = -1;
    }

    // Fold the new columns, oldest first: the columns that land on one device-pixel column become the
    // lowest low and the highest high there, so a long loop stays as cheap as a short one. A column
    // joins the pending one or completes it; the last stays pending for the next frame.
    const len = lo[0].length;
    let ng = 0;
    for (let k = n - 1; k >= 0; k--) {
      let q = (p - k * bin) % span;
      if (q < 0) q += span;
      let x = ((q / span) * tw) | 0;
      if (x >= tw) x = tw - 1;
      let j = (view.at - 1 - k) % len;
      if (j < 0) j += len;
      if (x === pendX) {
        for (let s = 0; s < SOURCES; s++) {
          const l = lo[s][j];
          const u = hi[s][j];
          if (l < pendLo[s]) pendLo[s] = l;
          if (u > pendHi[s]) pendHi[s] = u;
        }
        continue;
      }
      if (pendX >= 0) {
        const o = ng * SOURCES;
        for (let s = 0; s < SOURCES; s++) {
          glo[o + s] = pendLo[s];
          ghi[o + s] = pendHi[s];
        }
        gx[ng] = pendX;
        let w = (x - pendX + tw) % tw; // forward to the next column's own x: disjoint by construction
        if (w > cw) w = cw; // columns were dropped (a stall): leave the gap rather than paint a slab
        gw[ng] = w;
        ng++;
      }
      pendX = x;
      for (let s = 0; s < SOURCES; s++) {
        pendLo[s] = lo[s][j];
        pendHi[s] = hi[s][j];
      }
    }
    if (ng === 0) return;

    // The dim, one strip per new column: a bay whose lane does not sound clears instead, so a stopped
    // or muted lane's live layer settles to nothing and then holds still (its contour carries it).
    let heroLive = light.input > 0.004;
    for (let i = 0; i < LANES; i++) if (light.lit[i] > 0.004) heroLive = true;
    t.globalCompositeOperation = 'destination-out';
    t.fillStyle = '#000';
    for (let b = 0; b < BAYS; b++) {
      const live = b === 0 ? heroLive : light.lit[b - 1] > 0.004;
      t.globalAlpha = quiet || !live ? 1 : A[(FADE * 255) | 0];
      const y = bayY[b];
      const h = bayH[b];
      for (let g = 0; g < ng; g++) t.fillRect(gx[g], y, gw[g], h);
    }

    // The light: what accumulates first (the master's body, the monitor's glow beside it inside the
    // budget, each lane's own body), then the monitor's line written over them. Under reduced motion the
    // columns are opaque and nothing persists, so a locked loop settles into a still picture instead of
    // glowing, and the glow is left out.
    t.globalCompositeOperation = quiet ? 'source-over' : 'lighter';
    addBody(t, SCOPE_MASTER, 0, light.clip ? pal.rec : pal.engaged, quiet ? 255 : (ADD_MASTER * 255) | 0, ng);
    if (!quiet) addEdges(t, SCOPE_MONITOR, 0, pal.cyan, (ADD_MON_GLOW * 255) | 0, ng, grow, line);
    for (let i = 0; i < LANES; i++) addBody(t, i, i + 1, lightColor(pal, i), quiet ? 255 : (ADD_LANE * 255) | 0, ng);
    t.globalCompositeOperation = 'source-over';
    addEdges(t, SCOPE_MONITOR, 0, pal.cyan, (MON_LINE * 255) | 0, ng, 0, line);
    t.globalAlpha = 1;
    held = true;
  }

  function draw(s: Surface, now: number): void {
    const c = s.ctx;
    const pal = s.pal;
    const quiet = feed.reduced;
    if (resized || feed.selected !== shownSel || feed.beatsPerLoop !== shownBeats) redrawAll(pal);
    for (let i = 0; i < LANES; i++) if (laneChanged(stamp, i)) rasterise(pal, i);
    pullColumns(pal, quiet);
    // The beam with no column to ride: the drawn playhead. The phase is read HERE, in the frame's own
    // hot code: the same read inside `pullColumns` boxes a number every frame (measured, 21 bytes a
    // frame, by the stage-view probe's budget case).
    let bx = beam[0];
    if (bx === -2) {
      bx = Math.round(light.phase * tw);
      if (bx >= tw) bx = tw - 1;
    }
    // The columns stopped arriving: the device stopped, a fault, the taps closed, a held feed. The
    // per-column dim only erases where a new column lands, so the trace would freeze with light on it.
    // After IDLE_WAIT_MS of silence it fades to nothing over IDLE_FADE_MS and stops compositing until
    // columns return. Nothing to fade is nothing to do, so a rig whose engine sends no column at all
    // never enters this (`held` stays false) and the canvas holds exactly as still as it did.
    if (beam[ALIVE] !== 0) {
      idle[SEEN] = now;
      idle[FROM] = 0;
    } else if (held && tctx) {
      if (now - idle[SEEN] > IDLE_WAIT_MS) {
        if (idle[FROM] === 0) {
          idle[FROM] = now;
          idle[LEFT] = 1;
        }
        const want = 1 - (now - idle[FROM]) / IDLE_FADE_MS;
        if (want <= 0) {
          tctx.clearRect(0, 0, tw, th);
          held = false;
        } else {
          // The share of what still stands that this frame takes, so all of it is gone at the end.
          const step = ((idle[LEFT] - want) / idle[LEFT]) * 255;
          idle[LEFT] = want;
          tctx.globalCompositeOperation = 'destination-out';
          tctx.fillStyle = '#000';
          tctx.globalAlpha = A[step > 0 ? step | 0 : 0];
          tctx.fillRect(0, 0, tw, th);
          tctx.globalCompositeOperation = 'source-over';
          tctx.globalAlpha = 1;
        }
      }
    }

    c.setTransform(1, 0, 0, 1, 0, 0);
    c.globalCompositeOperation = 'source-over';
    c.globalAlpha = 1;
    c.fillStyle = pal.bg;
    c.fillRect(0, 0, s.dw, s.dh);
    c.globalAlpha = A[(light.dim * 255) | 0];
    c.drawImage(back, 0, top);
    if (held) {
      c.globalCompositeOperation = quiet ? 'source-over' : 'lighter';
      c.drawImage(trace, x0, top);
      c.globalCompositeOperation = 'source-over';
    }

    // The beam at the newest column, per bay in that bay's own state colour: rec red, overdub amber,
    // play green, grey where a lane holds audio but does not sound, warm white (a clip's red) for the
    // room's level. No legend needed, and nothing moves at rest or under reduced motion.
    const show = quiet ? 0 : light.move;
    if (show > 0.004 && bx >= 0) {
      const at = x0 + bx;
      let gl = at - (glow >> 1);
      if (gl < x0) gl = x0;
      let gr = at - (glow >> 1) + glow;
      if (gr > x0 + tw) gr = x0 + tw;
      let cl = at - (core >> 1);
      if (cl < x0) cl = x0;
      let cr = cl + core;
      if (cr > x0 + tw) cr = x0 + tw;
      c.globalCompositeOperation = 'lighter';
      for (let b = 0; b < BAYS; b++) {
        const i = b - 1;
        if (b > 0 && !holdsAudio(feed.kind[i])) continue;
        const lit = b === 0 ? 1 : light.lit[i];
        const strength = show * (b === 0 ? 1 : 0.4 + 0.6 * lit);
        c.fillStyle = b === 0 ? (light.clip ? pal.rec : pal.engaged) : lit > 0.004 ? lightColor(pal, i) : pal.faint;
        const y = top + bayY[b];
        const h = bayH[b];
        if (gr > gl) {
          c.globalAlpha = A[(BEAM_GLOW * strength * 255) | 0];
          c.fillRect(gl, y, gr - gl, h);
        }
        if (cr > cl) {
          c.globalAlpha = A[(BEAM_CORE * strength * 255) | 0];
          c.fillRect(cl, y, cr - cl, h);
        }
      }
      c.globalCompositeOperation = 'source-over';
    }

    // A refused press flashes its bay, and a waiting lane's hairline says what it waits for.
    for (let i = 0; i < LANES; i++) {
      const y = top + bayY[i + 1];
      const kind = feed.kind[i];
      if (feed.cueAt[i] > 0) {
        const cue = decay(now, feed.cueAt[i], 160);
        if (cue > 0) {
          c.globalAlpha = A[(0.12 * cue * 255) | 0];
          c.fillStyle = pal.engaged;
          c.fillRect(px, y, ww, unit);
        }
      }
      if (kind === 'armed' || kind === 'listening') {
        // An armed lane's hairline takes the count's beat and falls away over most of one (a beat is
        // 500 ms at 120 BPM), so the pre-roll reads as a pulse on the lane the take will land in.
        const breathe = quiet
          ? 0.45
          : kind === 'armed'
            ? 0.26 + 0.5 * decay(now, light.beatAt, BEAT_MS)
            : 0.4 + 0.1 * Math.sin((Math.PI * 2 * now) / 1200);
        c.globalAlpha = A[(breathe * 255) | 0];
        c.fillStyle = waitColor(pal, kind);
        c.fillRect(x0, y + (unit >> 1), tw, line);
      }
    }
    c.globalAlpha = 1;
  }

  /** The lane whose bay holds the point. The hero bay is the room, not a lane. */
  function hit(_x: number, y: number): number {
    const i = Math.floor((y - laneTopCss) / laneCss);
    return i >= 0 && i < LANES ? i : -1;
  }

  return {
    layout,
    draw,
    hit,
    dispose: () => {
      back.width = back.height = 0;
      trace.width = trace.height = 0;
    },
  };
}

/** The look asks the engine to fold and send the columns while it shows (`StageView.tsx`). */
export const scope: StageViewDef = { id: 'scope', name: 'SCOPE', create: createScope, wantsScope: true };
