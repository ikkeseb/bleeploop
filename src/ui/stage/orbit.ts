import { PEAK_FRAMES, looper } from '../state/audio';
import { LANES, feed } from './stage-feed';
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
 * ORBIT: five concentric rings, each lane's loop wrapped once around as a contour (lane 1 outermost,
 * twelve o'clock is the loop start), one playhead sweeping clockwise, the live input breathing at the
 * centre and a ripple leaving it on every beat.
 *
 * The rings, the beat ticks and the lane numbers are one cached bitmap, redrawn only when a lane's
 * form changes (`laneChanged`), and then only that lane's envelope is sampled again; a frame is that
 * blit plus strokes and fills: a breathing glow and a wake per sounding lane, the playhead, the
 * selection circle, the ripples and the core. Every size a frame passes to the canvas is an integer of
 * device px laid out once, and every alpha and rotation comes from `visual.ts`'s tables, so this
 * module's own frame code allocates nothing in steady playback (the stage-view probe's budget case).
 */

const N = 720; // angular samples per ring
const TAU = Math.PI * 2;
const TOP = -Math.PI / 2;
const COS = new Float32Array(N + 1);
const SIN = new Float32Array(N + 1);
for (let k = 0; k <= N; k++) {
  COS[k] = Math.cos(TOP + (TAU * k) / N);
  SIN[k] = Math.sin(TOP + (TAU * k) / N);
}
const DIGITS = ['1', '2', '3', '4', '5'];
const DASH = [3, 5];
const NO_DASH: number[] = [];
const CORE_STEPS = 6; // the input core's discs

interface Wakes {
  play: CanvasGradient;
  dub: CanvasGradient;
  faint: CanvasGradient;
}

function createOrbit(): ViewDraw {
  const bitmap = document.createElement('canvas');
  const bctx = bitmap.getContext('2d');
  const stamp = createStamp();
  const A = ALPHAS;
  // Each lane's envelope, kept between redraws: a capture changes one lane's peaks many times a second,
  // and only a `stale` lane is sampled again.
  const hi = Array.from({ length: LANES }, () => new Float32Array(N));
  const lo = Array.from({ length: LANES }, () => new Float32Array(N));
  const body = Array.from({ length: LANES }, () => new Float32Array(N));
  const cols = new Int32Array(LANES); // how many samples of each hold audio
  const stale = new Uint8Array(LANES).fill(1);
  // Device px, whole numbers, laid out once (`layout`).
  const r = new Int32Array(LANES); // each ring's centre radius
  const edge = new Int32Array(LANES); // each band's inner edge
  const sel = new Int32Array(LANES); // each lane's selection circle
  let dpr = 1;
  let cx = 0;
  let cy = 0;
  let R = 0; // the ring stack's outer radius
  let T = 0; // band thickness
  let G = 0; // gap between bands
  let hole = 0; // the centre: the input core and the count-in numeral's home
  let side = 0; // the bitmap's side
  let inner = 0; // where the playhead starts
  let reach = 0; // and how far it runs
  let hair = 1; // 1, 1.5, 2 and 2.5 CSS px
  let line = 2;
  let bold = 2;
  let heavy = 3;
  let tab = 0; // the lane number's tab, and its font
  let tabFont = '';
  let dirty = true;
  let full = true; // the whole bitmap is due, not only the lanes that changed
  let beats = -1;
  let wakes: Wakes | null = null;

  function layout(s: Surface): void {
    dpr = s.dpr;
    // The stack fills the height left above the message line, never more than 0.46 of the width a side.
    const top = 6;
    const bottom = hudPad(s.h) + hudMsg(s.h) * 1.3 + 6;
    const half = Math.max(40, Math.min(0.46 * s.w, (s.h - top - bottom) / 2));
    // Outside ring 1: the selection circle (0.4 G), then the beat ticks.
    const Rcss = (half - 16) / 1.016;
    R = Math.round(Rcss * dpr);
    T = Math.round(0.1 * R);
    G = Math.round(0.04 * R);
    hole = Math.round(0.3 * R);
    for (let i = 0; i < LANES; i++) {
      edge[i] = R - T - i * (T + G);
      r[i] = edge[i] + (T >> 1);
      sel[i] = edge[i] + T + Math.round(G * 0.4);
    }
    inner = edge[LANES - 1] - G;
    reach = R + Math.round(2 * dpr) - inner;
    hair = Math.max(1, Math.round(dpr));
    line = Math.max(1, Math.round(1.5 * dpr));
    bold = Math.max(2, Math.round(2 * dpr));
    heavy = Math.max(line + 1, Math.round(2.5 * dpr));
    tab = Math.min(T * 0.8, 18 * dpr);
    tabFont = `600 ${Math.round(tab * 0.78)}px Geist, system-ui, sans-serif`;
    cx = Math.round((s.w / 2) * dpr);
    cy = Math.round((top + half) * dpr);
    side = Math.ceil(half * dpr) * 2;
    bitmap.width = side;
    bitmap.height = side;
    s.countX = s.w / 2;
    s.countY = top + half;
    // Inside the hole at every size: a digit is about 0.6 em wide and 0.72 em tall.
    s.countSize = Math.min(clamp(0.36 * s.h, 140, 480), 0.59 * Rcss);
    wakes = null;
    dirty = true;
    full = true;
  }

  /** A wake: the lane colour rising from nothing, one beat behind the playhead, to full at it. */
  function wake(c: CanvasRenderingContext2D, color: string, span: number): CanvasGradient {
    const g = c.createConicGradient(-span, 0, 0);
    const end = span / TAU;
    g.addColorStop(0, withAlpha(color, 0));
    g.addColorStop(end * 0.6, withAlpha(color, 0.35));
    g.addColorStop(end, withAlpha(color, 1));
    g.addColorStop(Math.min(1, end + 0.0005), withAlpha(color, 0));
    g.addColorStop(1, withAlpha(color, 0));
    return g;
  }

  /**
   * One layer of a ring's contour over its first `n` samples: out by `up`, in by `down`. A whole ring
   * closes on its first sample; a partial one (a take in flight) runs on to its last column's far edge
   * at that column's value, so it meets the hairline that draws the rest of the ring.
   */
  function band(c: CanvasRenderingContext2D, mid: number, radius: number, n: number, swing: number, up: Float32Array, down: Float32Array | null, floor: number): void {
    const whole = n === N;
    c.beginPath();
    for (let k = 0; k <= n; k++) {
      const j = whole ? k % N : k < n ? k : n - 1;
      const rr = radius + Math.max(floor, swing * up[j]);
      if (k === 0) c.moveTo(mid + COS[k] * rr, mid + SIN[k] * rr);
      else c.lineTo(mid + COS[k] * rr, mid + SIN[k] * rr);
    }
    for (let k = n; k >= 0; k--) {
      const j = whole ? k % N : k < n ? k : n - 1;
      const rr = radius - Math.max(floor, swing * (down ? -down[j] : up[j]));
      c.lineTo(mid + COS[k] * rr, mid + SIN[k] * rr);
    }
    c.closePath();
    c.fill();
  }

  /** Lane `i` on the bitmap: its contour (sampled again only when `stale`), the hairline or tape where
   * it holds no audio, and its number. Everything it draws lies inside its own band. */
  function paintLane(c: CanvasRenderingContext2D, pal: Palette, i: number, mid: number, master: number): void {
    const kind = feed.kind[i];
    if (stale[i]) {
      looper.peaksInto(i, peaks);
      const span = stamp.span[i];
      const held = holdsAudio(kind) ? sampleEnvelope(N, span > 0 ? span / PEAK_FRAMES : peaks.count) : 0;
      for (let k = 0; k < held; k++) {
        hi[i][k] = env.hi[k];
        lo[i][k] = env.lo[k];
        body[i][k] = env.body[k];
      }
      cols[i] = held;
      stale[i] = 0;
    }
    const n = cols[i];
    const swing = (T / 2) * 0.86;
    if (n < N) {
      // Where no audio is: a hairline, or the dotted rec-red "tape to fill" of a later take.
      const tape = kind === 'rec' && master > 0;
      c.strokeStyle = tape ? pal.rec : pal.text;
      c.globalAlpha = tape ? 0.6 : 0.07;
      c.lineWidth = tape ? line : hair;
      c.setLineDash(tape ? DASH : NO_DASH);
      c.beginPath();
      c.arc(mid, mid, r[i], TOP + (TAU * n) / N, TOP + TAU);
      c.stroke();
      c.setLineDash(NO_DASH);
    }
    if (n > 0) {
      const mul = contourAlpha(i);
      c.fillStyle = contourColor(pal, i);
      c.globalAlpha = 0.34 * mul;
      band(c, mid, r[i], n, swing, hi[i], lo[i], dpr * 0.5);
      c.globalAlpha = 0.95 * mul;
      band(c, mid, r[i], n, swing, body[i], null, dpr * 0.5);
    }
    // The lane number on a dark tab at the loop start.
    const tx = mid + 5 * dpr;
    const ty = mid - r[i];
    c.globalAlpha = 0.85;
    c.fillStyle = pal.bg;
    c.fillRect(tx, ty - tab / 2, tab, tab);
    c.globalAlpha = 1;
    c.fillStyle = kind === 'empty' ? pal.faint : pal.dim;
    c.font = tabFont;
    c.textAlign = 'center';
    c.textBaseline = 'middle';
    c.fillText(DIGITS[i], tx + tab / 2, ty + tab * 0.04);
  }

  /**
   * Redraw the cached bitmap. `full` (a layout, a change of the beat count): the beat ticks and every
   * ring. Otherwise only the lanes whose form changed: a capture's peaks arrive many times a second, and
   * then its band alone is cleared and redrawn (the bands never overlap), the other four untouched.
   */
  function rasterise(pal: Palette): void {
    const c = bctx;
    if (!c) return;
    const mid = side / 2;
    const master = looper.masterFramesValue();
    c.setTransform(1, 0, 0, 1, 0, 0);
    if (full) {
      c.clearRect(0, 0, side, side);
      // Beat ticks outside ring 1 (and its selection circle); a bar's first is longer and brighter.
      c.fillStyle = pal.text;
      for (let b = 0; b < beats; b++) {
        const a = TOP + (TAU * b) / beats;
        const bar = b % 4 === 0;
        c.setTransform(Math.cos(a), Math.sin(a), -Math.sin(a), Math.cos(a), mid, mid);
        c.globalAlpha = bar ? 0.3 : 0.14;
        c.fillRect(sel[0] + 5 * dpr, -dpr / 2, (bar ? 8 : 4) * dpr, dpr);
      }
      c.setTransform(1, 0, 0, 1, 0, 0);
      for (let i = 0; i < LANES; i++) paintLane(c, pal, i, mid, master);
      full = false;
    } else {
      const half = (T + G) >> 1;
      for (let i = 0; i < LANES; i++) {
        if (!stale[i]) continue;
        c.save();
        c.beginPath();
        c.arc(mid, mid, r[i] + half, 0, TAU);
        c.arc(mid, mid, r[i] - half, 0, TAU, true);
        c.clip();
        c.clearRect(0, 0, side, side);
        paintLane(c, pal, i, mid, master);
        c.restore();
      }
    }
    c.globalAlpha = 1;
  }

  /** A circle stroke around (x, y). */
  function ring(c: CanvasRenderingContext2D, x: number, y: number, radius: number): void {
    c.beginPath();
    c.arc(x, y, radius, 0, TAU);
    c.stroke();
  }

  function disc(c: CanvasRenderingContext2D, radius: number): void {
    c.beginPath();
    c.arc(cx, cy, radius, 0, TAU);
    c.fill();
  }

  function draw(s: Surface, now: number): void {
    const c = s.ctx;
    const pal = s.pal;
    const master = looper.masterFramesValue();
    for (let i = 0; i < LANES; i++) {
      if (laneChanged(stamp, i)) {
        stale[i] = 1;
        dirty = true;
      }
    }
    if (feed.beatsPerLoop !== beats) {
      beats = feed.beatsPerLoop;
      wakes = null;
      dirty = true;
      full = true;
    }
    if (dirty) {
      rasterise(pal);
      dirty = false;
    }
    if (!wakes) {
      const span = beats > 0 ? TAU / beats : TAU / 16;
      wakes = { play: wake(c, pal.play, span), dub: wake(c, pal.dub, span), faint: wake(c, pal.faint, span) };
    }

    const still = feed.reduced;
    const dim = light.dim;
    const turn = (light.phase * TURN_STEPS) | 0;
    const beatEnv = decay(now, light.beatAt, 300);

    c.setTransform(1, 0, 0, 1, 0, 0);
    c.globalCompositeOperation = 'source-over';
    c.globalAlpha = 1;
    c.fillStyle = pal.bg;
    c.fillRect(0, 0, s.dw, s.dh);
    c.globalAlpha = A[(dim * 255) | 0];
    c.drawImage(bitmap, cx - (side >> 1), cy - (side >> 1));

    // Light: each sounding lane breathes with its own level (three stacked strokes, wide and faint to
    // narrow and bright) and drags a wake behind the playhead.
    if (!still) {
      c.globalCompositeOperation = 'lighter';
      for (let i = 0; i < LANES; i++) {
        const g = dim * light.lit[i];
        if (g <= 0.004) continue;
        const L = light.amp[i] * (feed.fading[i] ? 0.5 : 1);
        c.strokeStyle = lightColor(pal, i);
        c.globalAlpha = A[(g * (0.02 + 0.06 * L) * 255) | 0];
        c.lineWidth = T + 2 * G;
        ring(c, cx, cy, r[i]);
        c.globalAlpha = A[(g * (0.03 + 0.1 * L) * 255) | 0];
        c.lineWidth = Math.round(T * (0.5 + 0.35 * L));
        ring(c, cx, cy, r[i]);
        c.globalAlpha = A[(g * 0.22 * L * 255) | 0];
        c.lineWidth = Math.round(T * (0.14 + 0.2 * L));
        ring(c, cx, cy, r[i]);
      }
      if (master > 0 && light.move > 0.004) {
        turnTo(c, cx, cy, turn);
        c.lineWidth = Math.round(T * 0.9);
        for (let i = 0; i < LANES; i++) {
          const kind = feed.kind[i];
          if (kind !== 'play' && kind !== 'dub') continue;
          // A sounding lane always drags a wake (a floor under its level); a muted one half of it, grey.
          const muted = mutedLook(i);
          const a = dim * light.move * (0.25 + 0.55 * light.amp[i]) * (muted ? 0.5 : 1);
          if (a <= 0.004) continue;
          c.strokeStyle = muted ? wakes.faint : kind === 'dub' ? wakes.dub : wakes.play;
          c.globalAlpha = A[(a * 255) | 0];
          ring(c, 0, 0, r[i]);
        }
        c.setTransform(1, 0, 0, 1, 0, 0);
      }
      c.globalCompositeOperation = 'source-over';
    }

    // Per lane: the armed or listening hairline, a pending loop-end stop, a refusal's flash.
    for (let i = 0; i < LANES; i++) {
      const kind = feed.kind[i];
      if (kind === 'armed' || kind === 'listening') {
        const breathe = still ? 0.45 : kind === 'armed' ? 0.3 + 0.3 * beatEnv : 0.4 + 0.1 * Math.sin((TAU * now) / 1200);
        c.strokeStyle = waitColor(pal, kind);
        c.globalAlpha = A[(breathe * 255) | 0];
        c.lineWidth = line;
        ring(c, cx, cy, r[i]);
      }
      if (feed.stopping[i] && !feed.fading[i] && master > 0) {
        // A stop waits for the loop end: the stretch still to play is marked on the band's edge.
        turnTo(c, cx, cy, turn);
        c.strokeStyle = pal.dim;
        c.globalAlpha = 0.3;
        c.lineWidth = hair;
        c.beginPath();
        c.arc(0, 0, edge[i] + T, 0, TAU * (1 - light.phase));
        c.stroke();
        c.setTransform(1, 0, 0, 1, 0, 0);
      }
      const cue = decay(now, feed.cueAt[i], 160);
      if (cue > 0) {
        c.strokeStyle = pal.engaged;
        c.globalAlpha = A[(0.8 * cue * 255) | 0];
        c.lineWidth = bold;
        ring(c, cx, cy, r[i]);
      }
    }

    // The selected lane: a warm-white circle just outside its band, moved with no transition.
    c.strokeStyle = pal.engaged;
    c.globalAlpha = 0.9;
    c.lineWidth = bold;
    ring(c, cx, cy, sel[feed.selected]);

    // The playhead: one radial line over the stack, each lane's stretch in its capture's colour.
    if (master > 0 && light.move > 0.004) {
      const bar = !still && decay(now, light.barAt, 120) > 0;
      const core = bar ? heavy : line;
      turnTo(c, cx, cy, turn);
      c.fillStyle = pal.text;
      c.globalAlpha = A[(0.25 * light.move * 255) | 0];
      c.fillRect(inner, -(core >> 1) - bold, reach, core + 2 * bold);
      c.globalAlpha = A[((bar ? 1 : 0.9) * light.move * 255) | 0];
      c.fillRect(inner, -(core >> 1), reach, core);
      c.globalAlpha = A[(light.move * 255) | 0];
      for (let i = 0; i < LANES; i++) {
        const kind = feed.kind[i];
        const tint = kind === 'dub' || kind === 'armed' ? pal.dub : kind !== 'rec' && feed.fading[i] ? pal.dim : '';
        if (tint === '') continue;
        c.fillStyle = tint;
        c.fillRect(edge[i], -bold, T, 2 * bold);
      }
      // A take in flight is drawn over its own span (a multiply, or a free take past the loop, is
      // longer than the loop), so its head sits where its contour ends, not on the loop's playhead.
      c.fillStyle = pal.rec;
      for (let i = 0; i < LANES; i++) {
        if (feed.kind[i] !== 'rec') continue;
        const at = looper.recHeadFrac(i);
        turnTo(c, cx, cy, at >= 0 ? (at * TURN_STEPS) | 0 : turn);
        c.fillRect(edge[i], -bold, T, 2 * bold);
      }
      c.setTransform(1, 0, 0, 1, 0, 0);
    } else if (master <= 0) {
      // A first take has no loop to sweep yet: its own head runs around its ring in rec-red.
      for (let i = 0; i < LANES; i++) {
        if (feed.kind[i] !== 'rec') continue;
        const at = looper.recHeadFrac(i);
        turnTo(c, cx, cy, at > 0 ? (at * TURN_STEPS) | 0 : 0);
        c.fillStyle = pal.rec;
        c.globalAlpha = 0.25;
        c.fillRect(inner, -(line >> 1) - bold, reach, line + 2 * bold);
        c.globalAlpha = 1;
        c.fillRect(inner, -(line >> 1), reach, line);
        c.setTransform(1, 0, 0, 1, 0, 0);
      }
    }

    if (!still) {
      // Beat ripples: a circle leaves the hole on every beat, a heavier one on a bar's first.
      c.globalCompositeOperation = 'lighter';
      c.strokeStyle = pal.text;
      for (let k = 0; k < 2; k++) {
        const life = decay(now, k === 0 ? light.beatAt : light.prevAt, 450);
        if (life <= 0) continue;
        const bar = k === 0 ? light.beatBar : light.prevBar;
        const u = 1 - life;
        c.globalAlpha = A[((bar ? 0.35 : 0.18) * life * 255) | 0];
        c.lineWidth = bar ? bold : hair;
        ring(c, cx, cy, Math.round(hole + (R + T - hole) * (1 - (1 - u) * (1 - u))));
      }
      c.globalCompositeOperation = 'source-over';
    }

    // The input core, hidden while a count-in numeral owns the centre: a soft disc that swells and
    // brightens with the input around a steady dot (bright while a device runs). Thin discs, each over
    // the last, so the light falls off from the centre with no edge to read as a target.
    const show = clamp((dim - 0.45) / 0.55, 0, 1);
    if (show > 0.004) {
      const glow = show * (still ? 0.06 : 0.06 + 0.45 * light.input);
      const size = hole * (still ? 0.35 : 0.35 + 0.5 * Math.pow(light.inputGeo, 0.6));
      c.fillStyle = light.clip ? pal.rec : pal.engaged;
      c.globalAlpha = A[Math.max(1, ((glow / CORE_STEPS) * 255) | 0)];
      for (let k = 0; k < CORE_STEPS; k++) disc(c, Math.round(size * (1 - 0.11 * k)));
      c.globalAlpha = A[(show * light.core * 255) | 0];
      disc(c, Math.round(hole * 0.16));
    }
    c.globalAlpha = 1;
  }

  /** The lane whose band (and half the gap each side) holds the point; the hole and outside: none. */
  function hit(x: number, y: number): number {
    const d = Math.hypot(x * dpr - cx, y * dpr - cy);
    for (let i = 0; i < LANES; i++) if (Math.abs(d - r[i]) <= (T + G) / 2) return i;
    return -1;
  }

  return {
    layout,
    draw,
    hit,
    dispose: () => {
      bitmap.width = bitmap.height = 0;
    },
  };
}

export const orbit: StageViewDef = { id: 'orbit', name: 'ORBIT', create: createOrbit };
