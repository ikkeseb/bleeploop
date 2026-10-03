import { PEAK_FRAMES, looper } from '../state/audio';
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
 * STRATA: five full-width ribbons of waveform scrolling right to left past a fixed now line. What
 * comes next is bright on the right, what just played is dimmed on the left, and each lane swells and
 * lights the now line with its own level.
 *
 * Each lane's whole loop is one cached strip (the looper lane's two-layer envelope over its bar
 * grid), redrawn only when the lane's form changes (`laneChanged`) or the strip's size does; a frame
 * blits each strip twice (the wrap) and adds a handful of rects, all at whole device px, with alphas
 * from `visual.ts`'s table, so this module's own frame code allocates nothing in steady playback (the
 * stage-view probe's budget case).
 */

// The looper lanes' grid tints (waveform.ts).
const GRID_BEAT = 'rgba(148,168,215,.085)';
const GRID_BAR = 'rgba(148,168,215,.14)';
const GRID_NUM = 'rgba(148,168,215,.32)';
// An empty lane keeps the grid at half strength and no numbers: emptiness must not read as structure.
const GRID_BEAT_EMPTY = 'rgba(148,168,215,.0425)';
const GRID_BAR_EMPTY = 'rgba(148,168,215,.07)';
const DIGITS = ['1', '2', '3', '4', '5'];
const BAR_LABELS = Array.from({ length: 64 }, (_, k) => String(k + 1)); // made once, not per redraw
const BAND_TOP = 0.1; // of the height: under the chips
const BAND = 0.78; // of the height: above the message line
const NOW = 0.38; // of the width: the past left of it, what comes next right of it

function createStrata(): ViewDraw {
  const strips = Array.from({ length: LANES }, () => document.createElement('canvas'));
  const sctx = strips.map((strip) => strip.getContext('2d'));
  const numbers = document.createElement('canvas'); // the five lane numbers, one column
  const stamp = createStamp();
  const A = ALPHAS;
  // Device px, whole numbers, laid out once (`layout`).
  let dpr = 1;
  let W = 0;
  let top = 0;
  let rowH = 0;
  let nowX = 0;
  let stripW = 0;
  let line = 2; // 1.5 and 2 CSS px, the bar's 2.5
  let bold = 2;
  let heavy = 3;
  let glow = 22; // the now-light's half width
  let edge = 6; // the selected row's bar
  let numbersX = 16;
  let gridFont = '';
  let beats = -1;
  let resized = true;
  // CSS px, for the hit test.
  let topCss = 0;
  let rowCss = 1;

  function layout(s: Surface): void {
    dpr = s.dpr;
    W = s.dw;
    top = Math.round(BAND_TOP * s.dh);
    rowH = Math.max(8, Math.floor((BAND * s.dh) / LANES));
    nowX = Math.round(NOW * W);
    line = Math.max(1, Math.round(1.5 * dpr));
    bold = Math.max(1, Math.round(dpr));
    heavy = Math.max(line + 1, Math.round(2.5 * dpr));
    glow = Math.round(22 * dpr);
    edge = Math.round(6 * dpr);
    numbersX = Math.round(16 * dpr);
    gridFont = `${Math.round(11 * dpr)}px 'Geist Mono', ui-monospace, monospace`;
    topCss = top / dpr;
    rowCss = rowH / dpr;
    s.countX = s.w / 2;
    s.countY = (top + (rowH * LANES) / 2) / dpr;
    s.countSize = clamp(0.36 * s.h, 140, 480);
    // The lane numbers: one small bitmap, blitted every frame.
    const size = Math.round(15 * dpr);
    numbers.width = Math.round(28 * dpr);
    numbers.height = rowH * LANES;
    const n = numbers.getContext('2d');
    if (n) {
      n.font = `600 ${size}px Geist, system-ui, sans-serif`;
      n.textBaseline = 'top';
      n.fillStyle = s.pal.faint;
      for (let i = 0; i < LANES; i++) n.fillText(DIGITS[i], 0, i * rowH + Math.round(9 * dpr));
    }
    resized = true;
  }

  /** The strips span the loop: one window width up to 8 bars, two from 16 on. */
  function sizeStrips(): void {
    const bars = beats / 4;
    stripW = Math.max(1, Math.round(W * clamp(bars / 8, 1, 2)));
    for (const strip of strips) {
      strip.width = stripW;
      strip.height = rowH;
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

  /** Redraw lane `i`'s strip: the bar grid, the centre hairline, the envelope, a later take's tape. */
  function rasterise(pal: Palette, i: number): void {
    const c = sctx[i];
    if (!c) return;
    c.clearRect(0, 0, stripW, rowH);
    c.globalAlpha = 1;
    const mid = rowH / 2;
    const master = looper.masterFramesValue();
    const kind = feed.kind[i];
    const span = stamp.span[i];
    const line = Math.max(1, Math.round(dpr));

    // The bar grid rolls past the now line with the loop: that is the beat. A first take has none yet.
    if (beats > 0 && master > 0) {
      const empty = kind === 'empty';
      for (let b = 0; b < beats; b++) {
        c.fillStyle = b % 4 === 0 ? (empty ? GRID_BAR_EMPTY : GRID_BAR) : empty ? GRID_BEAT_EMPTY : GRID_BEAT;
        c.fillRect(Math.round((b / beats) * stripW), 0, line, rowH);
      }
      if (i === 0 && !empty) {
        c.fillStyle = GRID_NUM;
        c.font = gridFont;
        c.textBaseline = 'top';
        for (let b = 0; b < beats; b += 4) c.fillText(BAR_LABELS[(b >> 2) % BAR_LABELS.length], Math.round((b / beats) * stripW) + 6 * dpr, 5 * dpr);
      }
    }

    looper.peaksInto(i, peaks);
    const cols = holdsAudio(kind) ? sampleEnvelope(stripW, span > 0 ? span / PEAK_FRAMES : peaks.count) : 0;
    const tape = kind === 'rec' && master > 0;
    if (!tape) {
      c.fillStyle = pal.faint;
      c.globalAlpha = cols > 0 ? 0.25 : 0.12;
      c.fillRect(0, Math.round(mid), stripW, line);
    }
    if (cols > 0) {
      const reach = rowH * 0.42;
      const mul = contourAlpha(i);
      c.fillStyle = contourColor(pal, i);
      c.globalAlpha = 0.34 * mul;
      layer(c, cols, mid, reach, env.hi, env.lo);
      c.globalAlpha = 0.95 * mul;
      layer(c, cols, mid, reach, env.body, null);
    }
    if (tape) {
      c.fillStyle = pal.rec;
      c.globalAlpha = 0.5;
      const step = Math.round(7 * dpr);
      for (let x = cols + Math.round(6 * dpr); x < stripW; x += step) c.fillRect(x, Math.round(mid), Math.round(3 * dpr), line);
    }
    c.globalAlpha = 1;
  }

  /** Blit `strip` with its column `at` on the now line, `dh` tall from `y`; a loop wraps around. */
  function blit(c: CanvasRenderingContext2D, strip: HTMLCanvasElement, at: number, wrap: boolean, y: number, dh: number): void {
    let off = at - nowX; // the source x under the canvas's left edge
    if (wrap) {
      off %= stripW;
      if (off < 0) off += stripW;
      const first = Math.min(W, stripW - off);
      c.drawImage(strip, off, 0, first, rowH, 0, y, first, dh);
      if (first < W) c.drawImage(strip, 0, 0, W - first, rowH, first, y, W - first, dh);
      return;
    }
    const sx = Math.max(0, off);
    const dx = sx - off;
    const sw = Math.min(stripW - sx, W - dx);
    if (sw > 0) c.drawImage(strip, sx, 0, sw, rowH, dx, y, sw, dh);
  }

  function draw(s: Surface, now: number): void {
    const c = s.ctx;
    const pal = s.pal;
    const master = looper.masterFramesValue();
    if (feed.beatsPerLoop !== beats || resized) {
      beats = feed.beatsPerLoop;
      sizeStrips();
      for (let i = 0; i < LANES; i++) stamp.version[i] = -2;
      resized = false;
    }
    for (let i = 0; i < LANES; i++) if (laneChanged(stamp, i)) rasterise(pal, i);

    const still = feed.reduced;
    const bandH = rowH * LANES;
    const beatEnv = decay(now, light.beatAt, 300);
    const at = Math.round(light.phase * stripW);

    c.setTransform(1, 0, 0, 1, 0, 0);
    c.globalCompositeOperation = 'source-over';
    c.globalAlpha = 1;
    c.fillStyle = pal.bg;
    c.fillRect(0, 0, s.dw, s.dh);

    // The strips: the selected row's floor one tone up, a refusal's flash, then the loop itself; what
    // just played is dimmed, but a take in flight is not (its past is the news).
    for (let i = 0; i < LANES; i++) {
      const y = top + i * rowH;
      const kind = feed.kind[i];
      const rec = kind === 'rec';
      if (i === feed.selected) {
        c.globalAlpha = 1;
        c.fillStyle = pal.surf1;
        c.fillRect(0, y, W, rowH);
      }
      const cue = decay(now, feed.cueAt[i], 160);
      if (cue > 0) {
        c.globalAlpha = A[(0.12 * cue * 255) | 0];
        c.fillStyle = pal.engaged;
        c.fillRect(0, y, W, rowH);
      }
      // A recording take scrolls by its own head (its span may differ from the loop's); a first take has
      // nothing ahead of it, so it does not wrap.
      const dh = still ? rowH : Math.round(rowH * (0.84 + 0.16 * light.slow[i]));
      c.globalAlpha = A[(light.dim * 255) | 0];
      if (rec) blit(c, strips[i], Math.round(Math.max(0, looper.recHeadFrac(i)) * stripW), master > 0, y + ((rowH - dh) >> 1), dh);
      else blit(c, strips[i], master > 0 ? at : 0, true, y + ((rowH - dh) >> 1), dh);
      if (feed.stopping[i] && !feed.fading[i] && master > 0) {
        // A stop waits for the loop end: what is left of the loop is veiled.
        c.globalAlpha = 0.2;
        c.fillStyle = pal.dim;
        c.fillRect(nowX, y, Math.min(W - nowX, stripW - at), rowH);
      }
      if (!rec) {
        c.globalAlpha = 0.5;
        c.fillStyle = i === feed.selected ? pal.surf1 : pal.bg;
        c.fillRect(0, y, nowX, rowH);
      }
    }

    // Per lane at the now line: its light where it sounds (three stacked bars, wide and faint to narrow
    // and bright), a capture's head, an armed wait's hairline.
    for (let i = 0; i < LANES; i++) {
      const y = top + i * rowH;
      const kind = feed.kind[i];
      const g = light.dim * light.lit[i];
      if (!still && g > 0.004) {
        const L = light.amp[i] * (feed.fading[i] ? 0.5 : 1);
        const a = g * (0.1 + 0.4 * L);
        c.globalCompositeOperation = 'lighter';
        c.fillStyle = lightColor(pal, i);
        c.globalAlpha = A[(a * 0.3 * 255) | 0];
        c.fillRect(nowX - glow, y, 2 * glow, rowH);
        c.globalAlpha = A[(a * 0.5 * 255) | 0];
        c.fillRect(nowX - (glow >> 1), y, glow, rowH);
        c.globalAlpha = A[(a * 255) | 0];
        c.fillRect(nowX - (glow >> 3), y, glow >> 2, rowH);
        c.globalCompositeOperation = 'source-over';
      }
      const head = kind === 'rec' ? pal.rec : kind === 'dub' || (kind === 'armed' && master > 0) ? pal.dub : feed.fading[i] ? pal.dim : '';
      if (head !== '') {
        c.globalAlpha = 1;
        c.fillStyle = head;
        c.fillRect(nowX - bold, y, 2 * bold, rowH);
      }
      if (kind === 'armed' || kind === 'listening') {
        const breathe = still ? 0.45 : kind === 'armed' ? 0.3 + 0.3 * beatEnv : 0.4 + 0.1 * Math.sin((Math.PI * 2 * now) / 1200);
        c.globalAlpha = A[(breathe * 255) | 0];
        c.fillStyle = waitColor(pal, kind);
        c.fillRect(0, y + (rowH >> 1), W, line);
      }
    }

    // The now line: brighter on each beat, wider on a bar's first; the input's spine glows around it.
    const flash = still ? 0 : decay(now, light.beatAt, 120);
    const lw = !still && decay(now, light.barAt, 160) > 0 ? heavy : line;
    c.globalAlpha = A[((0.7 + 0.3 * flash) * 255) | 0];
    c.fillStyle = pal.text;
    c.fillRect(nowX - (lw >> 1), top, lw, bandH);
    if (!still) {
      const spine = Math.round((4 + 60 * light.inputGeo) * dpr);
      const a = 0.05 + 0.35 * light.input;
      c.globalCompositeOperation = 'lighter';
      c.fillStyle = light.clip ? pal.rec : pal.engaged;
      c.globalAlpha = A[(a * 0.5 * 255) | 0];
      c.fillRect(nowX - (spine >> 1), top, spine, bandH);
      c.globalAlpha = A[(a * 255) | 0];
      c.fillRect(nowX - (spine >> 2), top, spine >> 1, bandH);
      c.globalCompositeOperation = 'source-over';
    }

    // The selected row's edge and the lane numbers, over the dimmed past.
    c.globalAlpha = 1;
    c.fillStyle = pal.engaged;
    c.fillRect(0, top + feed.selected * rowH, edge, rowH);
    c.drawImage(numbers, numbersX, top);
  }

  function hit(_x: number, y: number): number {
    const i = Math.floor((y - topCss) / rowCss);
    return i >= 0 && i < LANES ? i : -1;
  }

  return {
    layout,
    draw,
    hit,
    dispose: () => {
      for (const strip of strips) strip.width = strip.height = 0;
      numbers.width = numbers.height = 0;
    },
  };
}

export const strata: StageViewDef = { id: 'strata', name: 'STRATA', create: createStrata };
