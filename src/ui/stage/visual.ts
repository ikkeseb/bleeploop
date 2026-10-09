import { looper, type PeakView } from '../state/audio';
import { LANES, feed, type LaneKind } from './stage-feed';

/**
 * OWNS: what every stage view shares: the palette, the drawing surface, the followers that turn the
 * loop's overview and the input meter into light, the shown loop phase, the beat pulses, and the lane
 * envelope the views rasterise. `stepLight` runs once per frame before the view draws; it reads the
 * plain feed and the looper's non-reactive getters only (invariant 6). Its own code allocates nothing
 * in steady playback, as the stage-view probe's budget case measures in headless Chromium; what that
 * case still sees is one boxed number a frame inside `looper.phaseValue()`.
 *
 * No audio reaches the UI: a lane's "level now" is its overview under the playhead times its volume,
 * so it sees neither lane FX nor a fade. The lift (1 / the lane's own recent peak, capped) makes a
 * quiet loop fill its form while silence stays dark; it scales light only, never geometry.
 */

/** The stage's device-pixel ratio is capped: its light reads from across the room, not its detail. */
export const DPR_CAP = 1.5;

export interface Palette {
  bg: string;
  surf1: string;
  text: string;
  dim: string;
  faint: string;
  engaged: string;
  rec: string;
  dub: string;
  play: string;
  cyan: string;
}

/** The colour tokens, read once per mount (a token edit over HMR needs a reload, as in waveform.ts). */
export function readPalette(el: Element): Palette {
  const cs = getComputedStyle(el);
  const v = (name: string, fallback: string): string => cs.getPropertyValue(name).trim() || fallback;
  return {
    bg: v('--bg', '#0a0b0e'),
    surf1: v('--surf-1', '#121419'),
    text: v('--text', '#eef1f8'),
    dim: v('--dim', '#b9bcc6'),
    faint: v('--faint', '#7d818d'),
    engaged: v('--engaged', '#f4efe6'),
    rec: v('--rec', '#ff3b52'),
    dub: v('--dub', '#ffae2a'),
    play: v('--play', '#36e39a'),
    cyan: v('--cyan', '#46d4e8'),
  };
}

/** A `#rrggbb` token as `rgba()` at `alpha`: for gradient stops, built on layout, never per frame. */
export function withAlpha(hex: string, alpha: number): string {
  const m = /^#([0-9a-f]{6})$/i.exec(hex);
  if (!m) return alpha > 0 ? hex : 'rgba(0,0,0,0)';
  const n = parseInt(m[1], 16);
  return `rgba(${n >> 16},${(n >> 8) & 255},${n & 255},${alpha})`;
}

/** The canvas a view draws on. Sizes change on resize only (`stage-loop.ts`), never per frame. */
export interface Surface {
  ctx: CanvasRenderingContext2D;
  pal: Palette;
  /** CSS px. */
  w: number;
  h: number;
  /** Device px, and the capped ratio between the two. */
  dw: number;
  dh: number;
  dpr: number;
  /** Where the count-in numeral sits and its size, in CSS px: a view's `layout` writes them. */
  countX: number;
  countY: number;
  countSize: number;
}

/** One stage look. `layout` runs on mount and on resize; `draw` every frame; `hit` names the lane
 * under a pointer (CSS px), or -1. */
export interface ViewDraw {
  layout(s: Surface): void;
  draw(s: Surface, now: number): void;
  hit(x: number, y: number): number;
  dispose(): void;
}

export interface StageViewDef {
  id: string;
  /** The name on the view switch. */
  name: string;
  /** This look draws the live scope columns, so the view asks the engine to fold and send them while
   * the look shows, and turns them off when it does not (`StageView.tsx`: nothing else sends
   * `SetScope`, and the engine sends nothing until something asks). */
  wantsScope?: boolean;
  create(): ViewDraw;
}

/**
 * Fractional canvas arguments without garbage. V8 boxes a fresh heap number for every fractional double
 * that crosses into a canvas call (or into a function it did not inline), so the draw loop passes
 * integers (device px) and constants, and takes the two fractions that change every frame, an alpha
 * and a rotation, from tables of numbers boxed once: `ctx.globalAlpha = ALPHAS[(a * 255) | 0]`. The
 * leading `null` keeps a table's elements tagged; a plain number array is stored unboxed and would box
 * again on every read. 1/255 of alpha is all a pixel shows, and 1/8192 of a turn is under a device
 * pixel on any ring the stage draws.
 */
export const ALPHAS = [null as unknown as number];
for (let k = 0; k < 512; k++) ALPHAS[k] = k >= 255 ? 1 : k / 255; // an index past 255 is 1, never undefined
export const TURN_STEPS = 8192;
const TURNS = [null as unknown as number];
for (let k = 0; k <= TURN_STEPS; k++) TURNS[k] = (Math.PI * 2 * k) / TURN_STEPS - Math.PI / 2;

/** Move the origin to (cx, cy) and point +x along loop phase `step` / TURN_STEPS (0 = twelve o'clock). */
export function turnTo(c: CanvasRenderingContext2D, cx: number, cy: number, step: number): void {
  c.setTransform(1, 0, 0, 1, cx, cy);
  c.rotate(TURNS[step < 0 ? 0 : step > TURN_STEPS ? TURN_STEPS : step]);
}

export function clamp(v: number, lo: number, hi: number): number {
  return v < lo ? lo : v > hi ? hi : v;
}

/** The HUD's edge padding and message-line size (CSS px), as `stage.css` sizes them. */
export const hudPad = (h: number): number => clamp(0.02 * h, 12, 24);
export const hudMsg = (h: number): number => clamp(0.045 * h, 28, 56);

// ── The lane envelope ─────────────────────────────────────────────────────────────────────────────

/** The one peak view every stage read fills: no allocation per frame. */
export const peaks: PeakView = { min: null, max: null, count: 0, version: 0 };

/** Column buffers for `sampleEnvelope`; they grow on a rasterise, never in the steady state. */
export const env = { lo: new Float32Array(0), hi: new Float32Array(0), body: new Float32Array(0) };

/**
 * Down-sample the lane in `peaks` to `n` columns over `across` bins (the loop's count, or a recording
 * take's span in bins): per column the peak pair clamped to [-1, 1] and the one-pole-smoothed
 * mean-|peak| body, waveform.ts's two-layer envelope. Returns how many columns hold audio.
 */
export function sampleEnvelope(n: number, across: number): number {
  if (env.hi.length < n) {
    env.lo = new Float32Array(n);
    env.hi = new Float32Array(n);
    env.body = new Float32Array(n);
  }
  const { min, max, count } = peaks;
  if (!min || !max || count <= 0 || across <= 0) return 0;
  let body = 0;
  let x = 0;
  for (; x < n; x++) {
    const p0 = Math.floor((x / n) * across);
    if (p0 >= count) break;
    let p1 = Math.floor(((x + 1) / n) * across);
    if (p1 <= p0) p1 = p0 + 1;
    if (p1 > count) p1 = count;
    let mn = min[p0];
    let mx = max[p0];
    for (let p = p0 + 1; p < p1; p++) {
      if (min[p] < mn) mn = min[p];
      if (max[p] > mx) mx = max[p];
    }
    body = body * 0.72 + (((mn < 0 ? -mn : mn) + (mx < 0 ? -mx : mx)) / 2) * 0.28;
    env.lo[x] = mn < -1 ? -1 : mn > 0 ? 0 : mn;
    env.hi[x] = mx > 1 ? 1 : mx < 0 ? 0 : mx;
    env.body[x] = body * 1.7 > 1 ? 1 : body * 1.7;
  }
  return x;
}

/** What a lane's cached drawing was made from: it is redrawn when any of these moves. */
export interface Stamp {
  version: Int32Array;
  span: Float64Array;
  master: Float64Array;
  kind: (LaneKind | '')[];
  muted: boolean[];
}

export function createStamp(): Stamp {
  return {
    version: new Int32Array(LANES).fill(-2),
    span: new Float64Array(LANES),
    master: new Float64Array(LANES).fill(-1),
    kind: Array.from({ length: LANES }, (): LaneKind | '' => ''),
    muted: Array.from({ length: LANES }, () => false),
  };
}

/**
 * Did lane `i`'s drawn form change since `st` (waveform.ts's dirty set: peaks, state, mute, master
 * length, a recording take's span)? Leaves the lane in `peaks` and its span in `st.span[i]`.
 */
export function laneChanged(st: Stamp, i: number): boolean {
  looper.peaksInto(i, peaks);
  const kind = feed.kind[i];
  const muted = feed.muted[i];
  const master = looper.masterFramesValue();
  const span = kind === 'rec' ? looper.recSpanFrames(i) : 0;
  if (
    peaks.version === st.version[i] &&
    kind === st.kind[i] &&
    muted === st.muted[i] &&
    master === st.master[i] &&
    span === st.span[i]
  ) {
    return false;
  }
  st.version[i] = peaks.version;
  st.kind[i] = kind;
  st.muted[i] = muted;
  st.master[i] = master;
  st.span[i] = span;
  return true;
}

/** The lane holds audio to draw. */
export const holdsAudio = (kind: LaneKind): boolean => kind === 'stop' || kind === 'play' || kind === 'dub' || kind === 'rec';

/** MUTED reads grey over a playing or stopped take; a live capture keeps its colour. */
export const mutedLook = (i: number): boolean => feed.muted[i] && (feed.kind[i] === 'play' || feed.kind[i] === 'stop');

/** The contour's colour: the take's state colour (an overdub keeps the loop's green under its amber light). */
export function contourColor(pal: Palette, i: number): string {
  if (mutedLook(i)) return pal.faint;
  const kind = feed.kind[i];
  return kind === 'rec' ? pal.rec : kind === 'play' || kind === 'dub' ? pal.play : pal.dim;
}

/** The contour's alpha multiplier: muted 0.3, stopped 0.38 (a silent lane is never brighter than a
 * sounding one), under an overdub 0.45. */
export function contourAlpha(i: number): number {
  if (mutedLook(i)) return 0.3;
  const kind = feed.kind[i];
  return kind === 'stop' ? 0.38 : kind === 'dub' ? 0.45 : 1;
}

/** A waiting lane's colour: amber armed for its downbeat, cyan listening for input (the surface
 * language's LISTEN colour, as on the looper lanes). */
export const waitColor = (pal: Palette, kind: LaneKind): string => (kind === 'listening' ? pal.cyan : pal.dub);

/** The colour of the lane's light (glow, wake, now-light). */
export function lightColor(pal: Palette, i: number): string {
  const kind = feed.kind[i];
  return kind === 'rec' ? pal.rec : kind === 'dub' ? pal.dub : mutedLook(i) ? pal.faint : pal.play;
}

// ── Light: followers, the shown phase, the beat pulses ────────────────────────────────────────────

const REST_MS = 700; // everything eases to rest over this
const WAKE_MS = 150; // and comes back over this

export const light = {
  /** The loop phase the views draw: the engine's while the loop moves, eased to the loop start at rest. */
  phase: 0,
  /** 1 while the loop moves, 0 at rest: playheads and wakes fade with it. */
  move: 0,
  /** The drawing's alpha: 0.45 under a count-in numeral. */
  dim: 1,
  /** Per lane: level x lift, 0..1 (120 ms release), whatever its mute. */
  amp: new Float32Array(LANES),
  /** Per lane: 1 while it sounds (plays unmuted, overdubs, records), eased like `move`. */
  lit: new Float32Array(LANES),
  /** Per lane: a slower follower (400 ms) of `lit x amp`. */
  slow: new Float32Array(LANES),
  /** The input: a 140 ms follower for light, a 40 ms smoother of it for geometry, the clip flash. */
  input: 0,
  inputGeo: 0,
  clip: false,
  /** The input element's steady alpha: 0.9 while a device runs, 0.3 without. */
  core: 0.3,
  /** The last two beat pulses (`performance.now()`'s clock) and whether each was a bar's first. */
  beatAt: -1e9,
  beatBar: false,
  prevAt: -1e9,
  prevBar: false,
  /** The last bar pulse. */
  barAt: -1e9,
};

const level = new Float32Array(LANES);
const peak = new Float32Array(LANES);
const heard = new Float32Array(LANES);
const PEAK_FLOOR = 1 / 6; // the lift is capped at 6
// The loop's own clock marks, in a typed array: a fractional number stored in a module variable is
// boxed again on every store.
const mem = new Float64Array(4);
const LAST = 0; // the last frame's timestamp
const REST_FROM = 1; // the phase the loop last moved at
const REST_AT = 2; // and when
const CLIP_AT = 3; // the last input clip
let beatIdx = -1;
let seenSeq = 0;

/** Start from the feed as it stands: opening the stage fades nothing in. */
export function resetLight(): void {
  mem[LAST] = 0;
  mem[REST_FROM] = 0;
  mem[REST_AT] = -1e9;
  mem[CLIP_AT] = -1e9;
  beatIdx = -1;
  seenSeq = feed.beatSeq;
  level.fill(0);
  peak.fill(PEAK_FLOOR);
  light.amp.fill(0);
  light.slow.fill(0);
  for (let i = 0; i < LANES; i++) light.lit[i] = sounds(i) ? 1 : 0;
  light.phase = 0;
  light.move = feed.moving ? 1 : 0;
  light.dim = feed.counting ? 0.45 : 1;
  light.input = 0;
  light.inputGeo = 0;
  light.clip = false;
  light.core = feed.running ? 0.9 : 0.3;
  light.beatAt = light.prevAt = light.barAt = -1e9;
}

function sounds(i: number): boolean {
  const kind = feed.kind[i];
  return kind === 'dub' || kind === 'rec' || (kind === 'play' && !feed.muted[i]);
}

function pulse(now: number, bar: boolean): void {
  light.prevAt = light.beatAt;
  light.prevBar = light.beatBar;
  light.beatAt = now;
  light.beatBar = bar;
  if (bar) light.barAt = now;
}

/** Lane `i`'s level now, into `heard`: the mean |peak| over the three bins around `bin` of the lane in
 * `peaks`, times the lane's volume for a loop (which wraps; a take in flight does neither). */
function hear(i: number, bin: number, loop: boolean): void {
  const { min, max, count } = peaks;
  heard[i] = 0;
  if (!min || !max || count <= 0) return;
  let sum = 0;
  for (let k = -1; k <= 1; k++) {
    let b = bin + k;
    if (loop) b = (b + count) % count;
    else if (b < 0 || b >= count) continue;
    const mn = min[b];
    const mx = max[b];
    sum += ((mn < 0 ? -mn : mn) + (mx < 0 ? -mx : mx)) / 2;
  }
  heard[i] = (sum / 3) * (loop ? feed.volume[i] : 1);
}

/** Advance the followers, the shown phase and the beat pulses to `now` (the frame's timestamp). */
export function stepLight(now: number): void {
  const dt = mem[LAST] === 0 ? 16 : clamp(now - mem[LAST], 0, 100);
  mem[LAST] = now;
  const master = looper.masterFramesValue();

  // The shown phase: the engine's while the loop moves; at rest, eased to the loop start (the next PLAY
  // starts from the top, so a playhead that ran on would lie).
  if (feed.moving) {
    light.phase = master > 0 ? looper.phaseValue() : 0;
    mem[REST_FROM] = light.phase;
    mem[REST_AT] = now;
  } else {
    const from = mem[REST_FROM];
    const u = feed.reduced ? 1 : clamp((now - mem[REST_AT]) / REST_MS, 0, 1);
    const eased = 1 - (1 - u) * (1 - u) * (1 - u);
    const p = from + ((from > 0.5 ? 1 : 0) - from) * eased;
    light.phase = p >= 1 ? 0 : p;
  }
  // `move` and each lane's `lit` come up over WAKE_MS and ease out over REST_MS (at once when still).
  const up = feed.reduced ? 1 : dt / WAKE_MS;
  const down = feed.reduced ? 1 : dt / REST_MS;
  const move = feed.moving ? light.move + up : light.move - down;
  light.move = move < 0 ? 0 : move > 1 ? 1 : move;

  const fall120 = Math.exp(-dt / 120);
  const fall400 = Math.exp(-dt / 400);
  const fall1600 = Math.exp(-dt / 1600);
  for (let i = 0; i < LANES; i++) {
    const kind = feed.kind[i];
    heard[i] = 0;
    if (feed.moving && (kind === 'play' || kind === 'dub' || kind === 'rec')) {
      looper.peaksInto(i, peaks);
      if (kind === 'rec') hear(i, peaks.count - 2, false);
      else hear(i, Math.floor(light.phase * peaks.count), true);
    }
    const v = heard[i];
    const lv = Math.max(v, level[i] * fall120);
    const pk = Math.max(v, PEAK_FLOOR, peak[i] * fall1600);
    level[i] = lv;
    peak[i] = pk;
    // Red is already loud: a recording take gets no lift.
    const amp = kind === 'rec' ? lv : lv / (pk < 1 ? pk : 1);
    light.amp[i] = amp > 1 ? 1 : amp;
    const lit = sounds(i) ? light.lit[i] + up : light.lit[i] - down;
    light.lit[i] = lit < 0 ? 0 : lit > 1 ? 1 : lit;
    light.slow[i] = Math.max(light.lit[i] * light.amp[i], light.slow[i] * fall400);
  }

  // The input meter (`levelValue` reads at least 1 while the engine reports a clip).
  const raw = looper.levelValue();
  if (raw >= 1) mem[CLIP_AT] = now;
  light.clip = now - mem[CLIP_AT] < 120;
  light.input = Math.max(raw > 1 ? 1 : raw, light.input * Math.exp(-dt / 140));
  light.inputGeo += (light.input - light.inputGeo) * (1 - Math.exp(-dt / 40));
  const core = feed.running ? 0.9 : 0.3;
  light.core += clamp(core - light.core, (-0.6 * dt) / REST_MS, (0.6 * dt) / REST_MS);
  light.dim += ((feed.counting ? 0.45 : 1) - light.dim) * (1 - Math.exp(-dt / 60));

  // Beats: from the loop phase while a loop moves (no signal), else from the mirrored beat (a count-in,
  // a first take). At rest nothing pulses.
  if (master > 0 && feed.moving && feed.beatsPerLoop > 0) {
    const n = feed.beatsPerLoop;
    const idx = Math.floor(light.phase * n) % n;
    // A new clock anchor may step the phase back across a beat line: that is not a beat.
    if (idx !== beatIdx && (beatIdx - idx + n) % n !== 1) {
      if (beatIdx >= 0) pulse(now, idx % 4 === 0);
      beatIdx = idx;
    }
    seenSeq = feed.beatSeq;
  } else {
    beatIdx = -1;
    if (feed.beatSeq !== seenSeq) {
      seenSeq = feed.beatSeq;
      if (feed.moving || feed.counting) pulse(now, feed.beat === 0);
    }
  }
}

/** A pulse's envelope: 1 at `at`, 0 from `ms` on (and before it). */
export function decay(now: number, at: number, ms: number): number {
  const age = now - at;
  return age < 0 || age >= ms ? 0 : 1 - age / ms;
}
