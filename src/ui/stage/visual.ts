import { looper, type PeakView } from '../state/audio';
import { LANES, feed, type LaneKind } from './stage-feed';

/**
 * OWNS: what every stage view shares: the palette, the drawing surface, the followers that turn the
 * loop's overview and the input meter into light, the shown loop phase, the beat pulses, the lane
 * envelope the views rasterise, and the lane panel (card, head column, glass pane) SCOPE and STRATA
 * stand in. `stepLight` runs once per frame before the view draws; it reads the
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
  surfSel: string;
  glass: string;
  sunk: string;
  glassLine: string;
  glassTop: string;
  hair: string;
  rowLine: string;
  text: string;
  legend: string;
  dim: string;
  faint: string;
  engaged: string;
  rec: string;
  dub: string;
  play: string;
  cyan: string;
  /** The LED ladder's lit zones and their unlit segments (`--meter-*`). */
  meterG: string;
  meterY: string;
  meterR: string;
  meterGOff: string;
  meterYOff: string;
  meterROff: string;
}

/** The colour tokens, read once per mount (a token edit over HMR needs a reload, as in waveform.ts). */
export function readPalette(el: Element): Palette {
  const cs = getComputedStyle(el);
  const v = (name: string, fallback: string): string => cs.getPropertyValue(name).trim() || fallback;
  return {
    bg: v('--bg', '#111317'),
    surf1: v('--surf-1', '#16181c'),
    surfSel: v('--surf-sel', '#1d2024'),
    glass: v('--glass', '#07090b'),
    sunk: v('--sunk', '#0b0d10'),
    glassLine: v('--glass-line', 'rgba(255, 255, 255, 0.05)'),
    glassTop: v('--glass-top', 'rgba(255, 255, 255, 0.07)'),
    hair: v('--hair', 'rgba(255, 255, 255, 0.06)'),
    rowLine: v('--row-line', '#0b0c0e'),
    text: v('--text', '#e9ecef'),
    legend: v('--legend', '#c3c8cf'),
    dim: v('--dim', '#a9afb7'),
    faint: v('--faint', '#8a9098'),
    engaged: v('--engaged', '#fff3e0'),
    rec: v('--rec', '#ff3b52'),
    dub: v('--dub', '#ffae2a'),
    play: v('--play', '#45d295'),
    cyan: v('--cyan', '#46d4e8'),
    meterG: v('--meter-g', '#4fd98f'),
    meterY: v('--meter-y', '#ffb02e'),
    meterR: v('--meter-r', '#ff4a5a'),
    meterGOff: v('--meter-g-off', '#1b2620'),
    meterYOff: v('--meter-y-off', '#2e2716'),
    meterROff: v('--meter-r-off', '#4a2026'),
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
  /** This look draws the live scope columns, so the app asks the engine to fold and send them while
   * the look shows, and turns them off when it does not (`src/app.tsx`, the always mounted owner:
   * nothing else sends `SetScope`, and the engine sends nothing until something asks). */
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
/** `k` / TURN_STEPS of a turn in radians, for an arc's span: `ctx.arc(0, 0, r, 0, SPANS[k])`. */
export const SPANS = [null as unknown as number];
for (let k = 0; k <= TURN_STEPS; k++) SPANS[k] = (Math.PI * 2 * k) / TURN_STEPS;

/** Move the origin to (cx, cy) and point +x along loop phase `step` / TURN_STEPS (0 = twelve o'clock). */
export function turnTo(c: CanvasRenderingContext2D, cx: number, cy: number, step: number): void {
  c.setTransform(1, 0, 0, 1, cx, cy);
  c.rotate(TURNS[step < 0 ? 0 : step > TURN_STEPS ? TURN_STEPS : step]);
}

export function clamp(v: number, lo: number, hi: number): number {
  return v < lo ? lo : v > hi ? hi : v;
}

/** The HUD's edge padding, top command band height and message-line size (CSS px), as `stage.css` sizes them. */
export const hudPad = (h: number): number => clamp(0.02 * h, 12, 24);
export const hudBand = (h: number): number => clamp(0.074 * h, 48, 104);
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
const mem = new Float64Array(5);
const LAST = 0; // the last frame's timestamp
const REST_FROM = 1; // the phase the loop last moved at
const REST_AT = 2; // and when
const CLIP_AT = 3; // the last input clip
const BEAT_PHASE = 4; // the phase the beat check last read
/** The beat a pulse last marked (-1: none yet; the next frame marks its beat unpulsed). */
let beatIdx = -1;
/** The beat the previous frame read. */
let lineIdx = -1;
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
    // A beat pulses when this frame steps forward over a beat line onto a beat other than the one last
    // marked. A new clock anchor may step the phase back across a line: that is not a beat, and stepping
    // forward over that line again marks nothing twice. The direction is read from the phase, the
    // shorter way round the loop (more than half the loop back is the loop wrapping forward, more than
    // half on is a correction back over the wrap), not from the indices: in a loop of two beats (one bar
    // of 6/8) every forward step is also one beat back.
    const n = feed.beatsPerLoop;
    const idx = Math.floor(light.phase * n) % n;
    // The shorter way round, branch-free: with a branch for a jump of more than half the loop, taken
    // first mid-jam, `stage-view`'s allocation budget failed about half its runs (the frame allocated
    // until this loop was optimized again).
    const moved = light.phase - mem[BEAT_PHASE];
    const step = moved - Math.round(moved);
    if (beatIdx < 0) beatIdx = idx;
    else if (idx !== lineIdx && idx !== beatIdx && step >= 0) {
      pulse(now, idx % feed.beatsPerBar === 0);
      beatIdx = idx;
    }
    lineIdx = idx;
    mem[BEAT_PHASE] = light.phase;
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

// ── The lane panel: the card, head column and recessed glass pane SCOPE and STRATA share ─────────

/** The selected row's lift inside the recessed pane, which no token names: it stays well below the
 * pixel census's 40-channel floor over `--glass` (#07090b). The pane's hairlines and the card's top
 * light are the palette's (`--glass-line`, `--glass-top`, `--hair`). */
export const GLASS_SEL = '#0d1014';
/** The bar ticks, in the looper grid's tint. Deliberately under the pixel census's floor (max channel
 * 40) over `--glass`: the chrome covers whole bays, and the census counts it against a playing lane's
 * light (`empty.lit < play.lit / 3`). */
export const TICK = 'rgba(148,168,215,.07)';
/** The light edge of `--groove` (a compound shadow, not a colour token: keep the two equal). */
const GROOVE_HI = 'rgba(255,255,255,0.035)';
const LANE_NAMES = ['1', '2', '3', '4', '5'];
const BAND_TOP = 0.1; // of the height: under the top HUD band
const BAND = 0.78; // of the height: above the message line
const HUD_GAP = 8; // CSS px: the card's least gap under the HUD band (as ORBIT and EMBER)
const GUTTER = 84; // CSS px: the head column's least width
/** The head column's least margin each side of the widest lane number, of the number's size: the
 * tightest the 84 px column gives at 1000x700 to 1920x1080 (SCOPE at 1920x1080, 0.29), so the column
 * widens only where the numbers reach their 82 px cap (0.18 in 84 px: STRATA at 1920x1080, both looks
 * at 2560x1440). */
const NUM_MARGIN = 0.28;
const FRAME = 8; // CSS px: the panel card's frame around top, right and bottom of the recessed glass pane

/**
 * The panel both lane looks stand in, so the two read as one machine and cannot drift: a card
 * (`--surf-1`, `--r-card`, its top light `--hair`) under the HUD band and above the message line, a
 * head column carrying the lane numbers large in `--font-name` (the selected head `--surf-sel`, its
 * number warm white), and one recessed glass pane (`--sunk` bezel, `--glass` floor, `--r-well`) whose
 * rows are the look's `lead` (SCOPE's hero bay, STRATA's bar ruler) over one bay per lane. Device px,
 * whole numbers, laid out by `layoutPanel` on mount and resize. The still bitmap a look paints the
 * panel into starts at the card's top edge (`top - frame` on the stage canvas).
 */
export interface Panel {
  dpr: number;
  px: number; // the card's left edge (the HUD's side inset)
  ww: number; // the card's width
  ch: number; // the card's height: the still bitmap's
  frame: number; // the card's frame around top, right and bottom of the pane
  bezel: number; // the `--sunk` recessed border round the pane
  rCard: number;
  rWell: number;
  x0: number; // the pane's left edge, past the head column
  tw: number; // and its width
  top: number; // the pane's top on the stage canvas
  th: number; // the pane's height
  unit: number; // one lane bay's height
  lead: number; // the pane's rows above lane 1's bay
  hair: number;
  groove: number;
  headMid: number; // the head column's centre
  sheenH: number; // the pane's top reflection's height
  numFont: string;
  legendFont: string;
  legPx: number; // the legends' size, device px
  /** CSS px, for a hit test: lane 1's bay's top and one bay's height. */
  laneTopCss: number;
  laneCss: number;
  /** The card's and the pane's outlines on the still bitmap, and the pane's on the stage canvas, where
   * every live mark is clipped to it without a path built per frame. */
  card: Path2D | null;
  well: Path2D | null;
  liveWell: Path2D | null;
  sheen: CanvasGradient | null;
  topShadow: CanvasGradient | null;
}

export function createPanel(): Panel {
  return {
    dpr: 1, px: 0, ww: 0, ch: 0, frame: 8, bezel: 3, rCard: 8, rWell: 6, x0: 0, tw: 0, top: 0, th: 0, unit: 0, lead: 0,
    hair: 1, groove: 2, headMid: 0, sheenH: 0, numFont: '', legendFont: '', legPx: 12, laneTopCss: 0, laneCss: 1,
    card: null, well: null, liveWell: null, sheen: null, topShadow: null,
  };
}

/**
 * Lay the panel out on `s`: the card's outer edges at the band's proportions where they fit, else clear
 * of the HUD band above and the message line below (the edges ORBIT and EMBER keep), so a short window
 * shrinks the bays. The pane holds `leadPx` plus `leadUnits` bays' worth of rows over the five lanes.
 */
export function layoutPanel(p: Panel, s: Surface, leadUnits: number, leadPx: number): void {
  const dpr = s.dpr;
  const W = s.dw;
  const frame = Math.round(FRAME * dpr);
  const band = Math.round(BAND_TOP * s.dh);
  const cardTop = Math.max(band - frame, Math.round((hudBand(s.h) + HUD_GAP) * dpr));
  const cardBottom = Math.min(band + Math.floor(BAND * s.dh) + frame, s.dh - Math.round((hudPad(s.h) + hudMsg(s.h) * 1.3 + 6) * dpr));
  const top = cardTop + frame;
  const unit = Math.max(6, Math.floor((cardBottom - frame - top - leadPx) / (leadUnits + LANES)));
  const lead = leadPx + leadUnits * unit;
  const th = lead + LANES * unit;
  const px = Math.round(hudPad(s.h) * dpr);
  const bezel = Math.max(2, Math.round(3 * dpr));
  // Legends (`--font-legend`: 600 caps), large `--font-name` for the lane numbers; the head column
  // fits the widest number with `NUM_MARGIN` each side. Measured, so the font's own advance counts:
  // the stage loop lays out again once the fonts are in.
  const legPx = Math.round(clamp(0.013 * s.h, 12, 17) * dpr);
  const numPx = Math.round(clamp(unit * 0.56, 26 * dpr, 82 * dpr));
  p.legendFont = `600 ${legPx}px 'Instrument Sans', system-ui, sans-serif`;
  p.legPx = legPx;
  p.numFont = `650 ${numPx}px 'Instrument Sans', system-ui, sans-serif`;
  s.ctx.save();
  s.ctx.font = p.numFont;
  let adv = 0;
  for (const n of LANE_NAMES) adv = Math.max(adv, s.ctx.measureText(n).width);
  s.ctx.restore();
  const x0 = px + Math.max(Math.round(GUTTER * dpr), Math.ceil(adv + 2 * NUM_MARGIN * numPx) + bezel);
  const tw = Math.max(8, W - px - frame - x0);
  p.dpr = dpr;
  p.frame = frame;
  p.top = top;
  p.unit = unit;
  p.lead = lead;
  p.th = th;
  p.ch = th + 2 * frame;
  p.px = px;
  p.ww = Math.max(8, W - 2 * px);
  p.bezel = bezel;
  p.rCard = Math.round(8 * dpr);
  p.rWell = Math.round(6 * dpr);
  p.x0 = x0;
  p.tw = tw;
  p.hair = Math.max(1, Math.round(dpr));
  p.groove = Math.max(2, Math.round(2 * dpr));
  p.headMid = px + ((x0 - bezel - px) >> 1);
  p.sheenH = Math.round(2 * unit * 0.35);
  p.laneTopCss = (top + lead) / dpr;
  p.laneCss = unit / dpr;
  p.card = new Path2D();
  p.card.roundRect(px, 0, p.ww, p.ch, p.rCard);
  p.well = new Path2D();
  p.well.roundRect(x0, frame, tw, th, p.rWell);
  p.liveWell = new Path2D();
  p.liveWell.roundRect(x0, top, tw, th, p.rWell);
  p.sheen = s.ctx.createLinearGradient(0, frame, 0, frame + p.sheenH);
  p.sheen.addColorStop(0, 'rgba(255,255,255,0.032)');
  p.sheen.addColorStop(1, 'rgba(255,255,255,0)');
  p.topShadow = s.ctx.createLinearGradient(0, frame, 0, frame + Math.round(10 * dpr));
  p.topShadow.addColorStop(0, 'rgba(0,0,0,0.85)');
  p.topShadow.addColorStop(1, 'rgba(0,0,0,0)');
  s.countX = s.w / 2;
  s.countY = (top + th / 2) / dpr;
  s.countSize = clamp(0.36 * s.h, 140, 480);
}

/** The lane a point is over (CSS px: a lane's head or bay, inside the card's span), or -1. */
export function panelHit(p: Panel, x: number, y: number): number {
  const dx = x * p.dpr;
  if (dx < p.px || dx >= p.px + p.ww) return -1;
  const i = Math.floor((y - p.laneTopCss) / p.laneCss);
  return i >= 0 && i < LANES ? i : -1;
}

/** Lane `i`'s bay's top on the still bitmap. */
export const panelLaneY = (p: Panel, i: number): number => p.frame + p.lead + i * p.unit;

/** Head-number tone for lane `i`: 2 selected (warm white), 1 sounding (--legend), 0 silent (--faint). */
export function laneTone(i: number): number {
  if (i === feed.selected) return 2;
  const k = feed.kind[i];
  return k === 'rec' || k === 'dub' || (k === 'play' && !mutedLook(i)) ? 1 : 0;
}

/** A lane bay's glass floor (one tone up when selected) and the hairline over it. */
export function glassRow(c: CanvasRenderingContext2D, pal: Palette, p: Panel, i: number): void {
  const y = panelLaneY(p, i);
  c.globalAlpha = 1;
  c.fillStyle = i === feed.selected ? GLASS_SEL : pal.glass;
  c.fillRect(p.x0, y, p.tw, p.unit);
  c.fillStyle = pal.glassLine;
  c.fillRect(p.x0, y, p.tw, p.hair);
}

/** The pane's one reflection along its top edge, its inner shadow and its top light (`--glass-top`). */
export function paneTop(c: CanvasRenderingContext2D, pal: Palette, p: Panel): void {
  const y = p.frame;
  if (p.sheen) {
    c.fillStyle = p.sheen;
    c.fillRect(p.x0, y, p.tw, p.sheenH);
  }
  if (p.topShadow) {
    c.fillStyle = p.topShadow;
    c.fillRect(p.x0, y, p.tw, Math.round(10 * p.dpr));
  }
  c.fillStyle = pal.glassTop;
  c.fillRect(p.x0, y, p.tw, p.hair);
}

/** Lane `i`'s head cell: panel tone (`--surf-sel` when selected), the milled groove above it, and its
 * large `--font-name` number, clipped to the card. */
function laneHead(c: CanvasRenderingContext2D, pal: Palette, p: Panel, i: number): void {
  if (!p.card) return;
  const y = panelLaneY(p, i);
  const hw = p.x0 - p.bezel - p.px;
  const fillH = i === LANES - 1 ? p.unit + p.frame : p.unit;
  c.save();
  c.clip(p.card);
  c.globalAlpha = 1;
  c.fillStyle = i === feed.selected ? pal.surfSel : pal.surf1;
  c.fillRect(p.px, y, hw, fillH);
  // Milled groove separating rows (matches `--groove` on the looper lanes).
  c.fillStyle = pal.rowLine;
  c.fillRect(p.px, y, hw, p.groove);
  c.fillStyle = GROOVE_HI;
  c.fillRect(p.px, y + p.groove, hw, p.hair);
  const tone = laneTone(i);
  c.font = p.numFont;
  c.textBaseline = 'middle';
  c.textAlign = 'center';
  c.fillStyle = tone === 2 ? pal.engaged : tone === 1 ? pal.legend : pal.faint;
  c.fillText(LANE_NAMES[i], p.headMid, y + (p.unit >> 1) + Math.round(1 * p.dpr));
  c.restore();
}

/**
 * The still bitmap's device rows `y0` up to `y1`, cleared and painted whole, every layer in one order
 * under a pixel-aligned clip, so any band of rows comes out exactly as a full repaint draws it: the
 * panel card, `lead` (the look's own print over the head column beside its lead rows, clipped to the
 * card), the lane heads, the recessed bezel, `bays` (the look's glass rows, clipped to the pane) and the
 * card's seam. A band repainted over itself never stacks the antialiased edges of the card or the pane.
 */
export function paintPanel(
  c: CanvasRenderingContext2D,
  pal: Palette,
  p: Panel,
  y0: number,
  y1: number,
  lead: ((c: CanvasRenderingContext2D, pal: Palette) => void) | null,
  bays: (c: CanvasRenderingContext2D, pal: Palette, y0: number, y1: number) => void,
): void {
  if (!p.card || !p.well) return;
  const W = c.canvas.width;
  c.save();
  c.beginPath();
  c.rect(0, y0, W, y1 - y0);
  c.clip();
  c.clearRect(0, y0, W, y1 - y0);
  c.globalAlpha = 1;

  // 1. Panel card (`--surf-1`, radius `--r-card`, the card's top light `--hair`), and the look's print
  // beside its lead rows.
  c.save();
  c.fillStyle = pal.surf1;
  c.fill(p.card);
  c.clip(p.card);
  c.fillStyle = pal.hair;
  c.fillRect(p.px, 0, p.ww, p.hair);
  if (lead && y0 < p.frame + p.lead) lead(c, pal);
  c.restore();

  // 2. Lane heads the band reaches: the selected one tone up (`--surf-sel`), no lit left edge.
  for (let i = 0; i < LANES; i++) {
    const y = panelLaneY(p, i);
    if (y < y1 && (i === LANES - 1 ? p.ch : y + p.unit) > y0) laneHead(c, pal, p, i);
  }

  // 3. Recessed glass bezel (`--sunk`) framing the one continuous glass pane (`--r-well`).
  c.fillStyle = pal.sunk;
  c.beginPath();
  c.roundRect(p.x0 - p.bezel, p.frame - p.bezel, p.tw + 2 * p.bezel, p.th + 2 * p.bezel, p.rWell + p.bezel);
  c.fill();

  // 4. The look's glass rows, inside the clipped pane.
  c.save();
  c.clip(p.well);
  bays(c, pal, y0, y1);
  c.restore();

  // 5. Crisp dark seam framing the panel card on `--bg`.
  c.strokeStyle = 'rgba(0,0,0,0.6)';
  c.lineWidth = p.hair;
  c.beginPath();
  c.roundRect(p.px + p.hair * 0.5, p.hair * 0.5, p.ww - p.hair, p.ch - p.hair, p.rCard);
  c.stroke();
  c.restore();
}
