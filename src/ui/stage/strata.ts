import { PEAK_FRAMES, looper } from '../state/audio';
import { LANES, feed } from './stage-feed';
import {
  ALPHAS,
  GLASS_SEL,
  TICK,
  clamp,
  contourAlpha,
  contourColor,
  createPanel,
  createStamp,
  decay,
  env,
  glassRow,
  holdsAudio,
  laneChanged,
  laneTone,
  layoutPanel,
  light,
  paintPanel,
  paneTop,
  panelHit,
  peaks,
  sampleEnvelope,
  waitColor,
  type Palette,
  type StageViewDef,
  type Surface,
  type ViewDraw,
} from './visual';

/**
 * STRATA: five lane ribbons of waveform scrolling right to left past a fixed now line, in SCOPE's panel
 * (`visual.ts` `layoutPanel`): the card, the head column with the lane numbers, and one recessed glass
 * pane whose top strip is the loop's one bar ruler over a bay per lane. What comes next is bright on
 * the right, what just played is dimmed on the left, and each lane swells with its own level and its
 * form lights up where it sounds at the now line.
 *
 * Each lane's whole loop is one cached strip (the looper lane's two-layer envelope), redrawn only when
 * the lane's form changes (`laneChanged`) or the strip's size does, and the ruler's numbers are one more
 * strip; the panel is a still bitmap, repainted when the layout, the selection or a head's tone moves. A
 * frame blits each strip twice (the wrap), the bar lines and a handful of rects, all at whole device px,
 * with alphas from `visual.ts`'s table, so this module's own frame code allocates nothing in steady
 * playback (the stage-view probe's budget case).
 */

const DIGITS = ['0', '1', '2', '3', '4', '5', '6', '7', '8', '9'];
const NOW = 0.38; // of the pane's width: the past left of it, what comes next right of it
/** The playhead, as the main screen's (`looper/waveform.ts`): one crisp solid line, 2 CSS px, 2.5 on a
 * pane 1600 CSS px wide or wider. */
const PLAYHEAD_WIDE = 1600;
/** The contour's reach of a bay's half height. */
const REACH = 0.42;

function createStrata(): ViewDraw {
  const strips = Array.from({ length: LANES }, () => document.createElement('canvas'));
  const sctx = strips.map((strip) => strip.getContext('2d'));
  const ruler = document.createElement('canvas'); // the bar numbers over the whole loop, one strip
  const rctx = ruler.getContext('2d');
  const back = document.createElement('canvas'); // the still panel: card, heads, the glass
  const bctx = back.getContext('2d');
  const panel = createPanel();
  const stamp = createStamp();
  const A = ALPHAS;
  // Device px, whole numbers, laid out once (`layout`).
  let top = 0; // the pane's top on the stage canvas
  let x0 = 0; // the pane's left edge
  let tw = 0; // and its width
  let strip = 0; // the ruler's height at the pane's top
  let rowH = 0; // one lane bay
  let nowP = 0; // the now line's x in the pane
  let stripW = 0;
  let hair = 1;
  let head = 2; // the playhead's width
  let line = 2; // a waiting lane's hairline
  let liftW = 24; // the half widths of the three lifts round the now line, wide to narrow
  let liftM = 12;
  let liftN = 4;
  let rulerFont = '';
  let numFamily = '';
  let digitW = 0;
  let beats = -1;
  let perBar = 4;
  let bars = 0;
  let ticks = new Int32Array(0); // each bar line's x on the strips
  let resized = true;
  let liveWell: Path2D | null = null;
  // What the still panel shows.
  let shownSel = -1;
  const shownTone = new Int8Array(LANES).fill(-1);

  function layout(s: Surface): void {
    const dpr = s.dpr;
    // The ruler's numerals: `--font-num`, 12 CSS px at a 1000x700 window (a legend's floor) up to 18.
    if (!numFamily) numFamily = getComputedStyle(s.ctx.canvas).getPropertyValue('--font-num').trim() || "'Instrument Sans', system-ui, sans-serif";
    const rulerPx = clamp(0.015 * s.h, 12, 18);
    strip = Math.round(rulerPx * 1.9 * dpr);
    layoutPanel(panel, s, 0, strip);
    top = panel.top;
    x0 = panel.x0;
    tw = panel.tw;
    rowH = panel.unit;
    liveWell = panel.liveWell;
    nowP = Math.round(NOW * tw);
    hair = panel.hair;
    head = Math.max(1, Math.round(dpr * (tw / dpr >= PLAYHEAD_WIDE ? 2.5 : 2)));
    line = Math.max(1, Math.round(1.5 * dpr));
    liftW = Math.round(24 * dpr);
    liftM = Math.round(12 * dpr);
    liftN = Math.max(1, Math.round(4 * dpr));
    rulerFont = `500 ${Math.round(rulerPx * dpr)}px ${numFamily}`;
    if (rctx) {
      rctx.font = rulerFont;
      let w = 0;
      for (let d = 0; d < 10; d++) {
        const m = rctx.measureText(DIGITS[d]).width;
        if (m > w) w = m;
      }
      digitW = w;
    }
    back.width = s.dw;
    back.height = panel.ch;
    shownSel = -1;
    resized = true;
  }

  /** The lane bays' glass, the ruler's strip over them, and the pane's top reflection. */
  function bays(c: CanvasRenderingContext2D, pal: Palette): void {
    c.globalAlpha = 1;
    c.fillStyle = pal.glass;
    c.fillRect(x0, panel.frame, tw, strip);
    for (let i = 0; i < LANES; i++) glassRow(c, pal, panel, i);
    paneTop(c, pal, panel);
  }

  /** The still panel, whole: the card, the head column, the bezel and the glass. */
  function paintBack(pal: Palette): void {
    if (!bctx) return;
    paintPanel(bctx, pal, panel, 0, panel.ch, null, bays);
    shownSel = feed.selected;
    for (let i = 0; i < LANES; i++) shownTone[i] = laneTone(i);
  }

  /** The strips span the loop: one pane width up to 8 bars, two from 16 on. */
  function sizeStrips(): void {
    bars = beats > 0 && perBar > 0 ? Math.round(beats / perBar) : 0;
    stripW = Math.max(1, Math.round(tw * clamp(bars / 8, 1, 2)));
    for (const s of strips) {
      s.width = stripW;
      s.height = rowH;
    }
    if (ticks.length !== bars) ticks = new Int32Array(bars);
    for (let b = 0; b < bars; b++) ticks[b] = Math.round((b / bars) * stripW);
  }

  /**
   * The ruler: each bar's number hangs off the top of its bar line, as on the main screen's pane
   * (`looper/waveform.ts` `drawGrid`): its left edge a hair right of the line, in the numerals' face,
   * every digit one advance wide, in --faint. A long loop's bars grow narrower than their numbers: it
   * then names every 2nd, 4th, 8th … bar, the fewest skipped for the widest number and a digit's air.
   * A one-bar loop, or none, has no numbers: a lone 1 says nothing.
   */
  function rasteriseRuler(pal: Palette): void {
    const c = rctx;
    ruler.width = stripW;
    ruler.height = strip;
    if (!c || bars < 2) return;
    c.font = rulerFont;
    c.fillStyle = pal.faint;
    c.textAlign = 'center';
    c.textBaseline = 'middle';
    const pad = 1 + Math.round(4 * panel.dpr);
    const mid = (strip >> 1) + Math.round(panel.dpr);
    let widest = 1;
    for (let m = bars; m >= 10; m = (m / 10) | 0) widest++;
    const need = (widest + 1) * digitW + pad;
    let every = 1;
    while (every < bars && (every * stripW) / bars < need) every *= 2;
    for (let b = 0; b < bars; b += every) {
      let n = b + 1;
      let digits = 1;
      for (let m = n; m >= 10; m = (m / 10) | 0) digits++;
      const x = ticks[b] + pad;
      for (let k = digits - 1; k >= 0; k--) {
        c.fillText(DIGITS[n % 10], x + (k + 0.5) * digitW, mid);
        n = (n / 10) | 0;
      }
    }
  }

  /** One layer of the envelope over `cols` columns: up by `hi`, down by `lo`. */
  function layer(c: CanvasRenderingContext2D, cols: number, mid: number, reach: number, hi: Float32Array, lo: Float32Array | null): void {
    c.beginPath();
    c.moveTo(0, mid - Math.max(0.5, reach * hi[0]));
    for (let x = 1; x < cols; x++) c.lineTo(x + 0.5, mid - Math.max(0.5, reach * hi[x]));
    c.lineTo(cols, mid);
    for (let x = cols - 1; x >= 0; x--) c.lineTo(x + 0.5, mid + Math.max(0.5, reach * (lo ? -lo[x] : hi[x])));
    c.closePath();
    c.fill();
  }

  /** Redraw lane `i`'s strip: its recorded take as the dim two-layer contour, in its state's colour. */
  function rasterise(pal: Palette, i: number): void {
    const c = sctx[i];
    if (!c) return;
    c.clearRect(0, 0, stripW, rowH);
    const span = stamp.span[i];
    looper.peaksInto(i, peaks);
    const cols = holdsAudio(feed.kind[i]) ? sampleEnvelope(stripW, span > 0 ? span / PEAK_FRAMES : peaks.count) : 0;
    if (cols > 0) {
      const mid = rowH / 2;
      const reach = rowH * REACH;
      const mul = contourAlpha(i);
      c.fillStyle = contourColor(pal, i);
      c.globalAlpha = 0.34 * mul;
      layer(c, cols, mid, reach, env.hi, env.lo);
      c.globalAlpha = 0.95 * mul;
      layer(c, cols, mid, reach, env.body, null);
    }
    c.globalAlpha = 1;
  }

  /**
   * Blit source columns [at - half, at + half) of `src` (`sh` tall) centred on the now line, `dh` tall
   * from `y` (`half` -1: the whole pane's width); a loop wraps around, a first take does not. Integers
   * only: a fractional argument boxes crossing in.
   */
  function blit(c: CanvasRenderingContext2D, src: HTMLCanvasElement, at: number, half: number, wrap: boolean, y: number, sh: number, dh: number): void {
    const from = half < 0 ? 0 : nowP - half; // the pane x the span starts at
    const span = half < 0 ? tw : 2 * half;
    let off = at - nowP + from; // the source x under it
    if (wrap) {
      off %= stripW;
      if (off < 0) off += stripW;
      const first = Math.min(span, stripW - off);
      c.drawImage(src, off, 0, first, sh, x0 + from, y, first, dh);
      if (first < span) c.drawImage(src, 0, 0, span - first, sh, x0 + from + first, y, span - first, dh);
      return;
    }
    const sx = Math.max(0, off);
    const dx = from + sx - off;
    const sw = Math.min(stripW - sx, from + span - dx);
    if (sw > 0) c.drawImage(src, sx, 0, sw, sh, x0 + dx, y, sw, dh);
  }

  function draw(s: Surface, now: number): void {
    const c = s.ctx;
    const pal = s.pal;
    const master = looper.masterFramesValue();
    if (feed.beatsPerLoop !== beats || feed.beatsPerBar !== perBar || resized) {
      beats = feed.beatsPerLoop;
      perBar = feed.beatsPerBar;
      sizeStrips();
      rasteriseRuler(pal);
      for (let i = 0; i < LANES; i++) stamp.version[i] = -2;
      resized = false;
    }
    let tones = feed.selected !== shownSel;
    for (let i = 0; i < LANES; i++) if (laneTone(i) !== shownTone[i]) tones = true;
    if (tones) paintBack(pal);
    for (let i = 0; i < LANES; i++) if (laneChanged(stamp, i)) rasterise(pal, i);

    const still = feed.reduced;
    const beatEnv = decay(now, light.beatAt, 300);
    const at = master > 0 ? Math.round(light.phase * stripW) : 0;
    const dimA = A[(light.dim * 255) | 0];
    const nowX = x0 + nowP;
    const lanes = top + strip;

    c.setTransform(1, 0, 0, 1, 0, 0);
    c.globalCompositeOperation = 'source-over';
    c.globalAlpha = 1;
    c.fillStyle = pal.bg;
    c.fillRect(0, 0, s.dw, s.dh);
    c.globalAlpha = dimA;
    c.drawImage(back, 0, top - panel.frame);
    // Every live mark below lands inside the glass pane, never on its rounded corners or the bezel.
    c.save();
    if (liveWell) c.clip(liveWell);

    // The bar lines down the pane and the ruler's numbers roll past the now line with the loop: that is
    // the beat. A first take has none yet.
    if (master > 0 && bars > 0) {
      c.fillStyle = TICK;
      const off = at - nowP; // the strip x under the pane's left edge
      for (let b = 0; b < bars; b++) {
        let x = (ticks[b] - off) % stripW;
        if (x < 0) x += stripW;
        if (x < tw) c.fillRect(x0 + x, top, hair, panel.th);
      }
      blit(c, ruler, at, -1, true, top, strip, strip);
    }

    // The bays: a refusal's flash, then the loop itself; what just played is dimmed, but a take in
    // flight is not (its past is the news).
    for (let i = 0; i < LANES; i++) {
      const y = lanes + i * rowH;
      const rec = feed.kind[i] === 'rec';
      if (feed.cueAt[i] > 0) {
        const cue = decay(now, feed.cueAt[i], 160);
        if (cue > 0) {
          c.globalAlpha = A[(0.12 * cue * 255) | 0];
          c.fillStyle = pal.engaged;
          c.fillRect(x0, y, tw, rowH);
        }
      }
      // A recording take scrolls by its own head (its span may differ from the loop's); a first take has
      // nothing ahead of it, so it does not wrap.
      const dh = still ? rowH : Math.round(rowH * (0.84 + 0.16 * light.slow[i]));
      const dy = y + ((rowH - dh) >> 1);
      const from = rec ? Math.round(Math.max(0, looper.recHeadFrac(i)) * stripW) : at;
      const wrap = rec ? master > 0 : true;
      c.globalAlpha = dimA;
      blit(c, strips[i], from, -1, wrap, dy, rowH, dh);
      if (feed.stopping[i] && !feed.fading[i] && master > 0) {
        // A stop waits for the loop end: what is left of the loop is veiled.
        c.globalAlpha = 0.2;
        c.fillStyle = pal.dim;
        c.fillRect(nowX, y, Math.min(tw - nowP, stripW - at), rowH);
      }
      if (!rec) {
        c.globalAlpha = 0.5;
        c.fillStyle = i === feed.selected ? GLASS_SEL : pal.glass;
        c.fillRect(x0, y, nowP, rowH);
      }
      // Its light where it sounds: the form itself lit at the now line, wide and faint to narrow and
      // bright, over the dimmed past as over what comes next, never a box of light on the glass.
      const g = light.dim * light.lit[i];
      if (!still && g > 0.004) {
        const a = g * (0.3 + 0.7 * light.amp[i] * (feed.fading[i] ? 0.5 : 1));
        c.globalCompositeOperation = 'lighter';
        c.globalAlpha = A[(a * 0.15 * 255) | 0];
        blit(c, strips[i], from, liftW, wrap, dy, rowH, dh);
        c.globalAlpha = A[(a * 0.2 * 255) | 0];
        blit(c, strips[i], from, liftM, wrap, dy, rowH, dh);
        c.globalAlpha = A[(a * 0.25 * 255) | 0];
        blit(c, strips[i], from, liftN, wrap, dy, rowH, dh);
        c.globalCompositeOperation = 'source-over';
      }
    }

    // The now line, one crisp line down the pane: in each bay the colour of what lands there (a capture's
    // red, an overdub's or an armed take's amber, a fade's grey), else the main screen's playhead; and an
    // armed or listening lane's hairline says what it waits for.
    const hx = nowX - (head >> 1);
    c.globalAlpha = dimA;
    c.fillStyle = pal.text;
    c.fillRect(hx, top, head, strip);
    for (let i = 0; i < LANES; i++) {
      const y = lanes + i * rowH;
      const kind = feed.kind[i];
      c.globalAlpha = dimA;
      c.fillStyle = kind === 'rec' ? pal.rec : kind === 'dub' || (kind === 'armed' && master > 0) ? pal.dub : feed.fading[i] ? pal.dim : pal.text;
      c.fillRect(hx, y, head, rowH);
      if (kind === 'armed' || kind === 'listening') {
        const breathe = still ? 0.45 : kind === 'armed' ? 0.3 + 0.3 * beatEnv : 0.4 + 0.1 * Math.sin((Math.PI * 2 * now) / 1200);
        c.globalAlpha = A[(breathe * 255) | 0];
        c.fillStyle = waitColor(pal, kind);
        c.fillRect(x0, y + (rowH >> 1), tw, line);
      }
    }
    c.restore();
    c.globalAlpha = 1;
  }

  /** The lane whose bay holds the point. */
  function hit(_x: number, y: number): number {
    return panelHit(panel, y);
  }

  return {
    layout,
    draw,
    hit,
    dispose: () => {
      for (const s of strips) s.width = s.height = 0;
      ruler.width = ruler.height = 0;
      back.width = back.height = 0;
    },
  };
}

export const strata: StageViewDef = { id: 'strata', name: 'STRATA', create: createStrata };
