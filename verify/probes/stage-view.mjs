/**
 * Stage view (src/ui/stage/), the performance layer: one canvas per look (`views.ts`) under a thin HUD,
 * in engine mode on the web engine fake (`src/platform/host.web.ts`, the `engine-seam` pattern: an init
 * script sets `window.__lfEngineFake`, the probe scripts the feed through `__lf.native` and reads what
 * the UI sends). Groups, each from a fresh engine (`--case=a,b` runs only those; a red group does not
 * stop the others, the run fails at the end):
 *
 * - enter: B (either case), the `stageView` action, the command-bar cap, the exit button and Escape open
 *   and close it; the normal UI is inert while open and its lane canvas is the same element after.
 * - views: it opens on the first look; V and the view switch cycle the looks in order and wrap; a look
 *   switch sends the engine only the one `SetScope` the looks' own `wantsScope` owes and never touches
 *   the transport; the choice survives close and reopen and a page reload; the lane selection carries
 *   across looks; one canvas, its backing store at the device-pixel ratio capped at 1.5 (checked at a
 *   ratio of 2), sized on resize.
 * - chips: for EMPTY, ARMED (count-in), LISTENING, REC, PLAY, STOP, MUTED, DUB and an ARMED later take,
 *   each chip's data-state and number colour follow the lane the feed reports and its accessible name
 *   says the state; the selected chip carries aria-current and the warm face; chips never move; no
 *   state word, dB, loop seconds or MUTE/REV flag is left in the view.
 * - pixels (per look): the canvas follows the feed: a PLAYING lane draws in the play colour, MUTED in
 *   grey with no play colour, EMPTY near nothing, REC in rec-red, an overdub adds amber; the warm-white
 *   selection mark moves when a digit selects another lane; at rest the loop is cued at its start (no
 *   playhead moves) while the engine's phase runs on.
 * - scope: the engine's live scope columns, which only the SCOPE look draws and no other group emits.
 *   Opening the view on SCOPE sends `{SetScope: true}` and nothing else; stepping to a look that does
 *   not draw them sends `false`; stepping back asks again; closing lets them go; none of it touches the
 *   transport. One pass of columns puts light in the lane bays and a body in the hero bay that neither
 *   holds without them. Then the look's central claim, at BOTH column densities (the 8-bar loop, whose
 *   4000 raw columns a pass share the 1204 device px of a 1280-wide window; a half-second loop, whose
 *   125 are 10 px wide each) and with the batch boundaries moved across the pulse, since a fold that
 *   only holds inside one batch fails exactly where a pulse straddles two: a pass repeated is over
 *   twice as bright as one pass (the look's ADD / FADE converges on 2.44x at ten), a one-off then
 *   silence falls to a fifth of its own single pass, and once the stream stops the trace fades out and
 *   then holds still. A batch the engine could not splice (`gap`) leaves the bay's light where it was:
 *   an xrun costs one 4 ms window, not the picture.
 * - count: a first take's count-in (no master, `Beat`s with countLeft 4..1) shows each numeral in the
 *   overlay, inside the window at three sizes, with the message line empty, and nothing at 0; an armed
 *   later take beside a PLAYING lane (countLeft 0) reads WAITING FOR DOWNBEAT and shows no numeral.
 * - cue: an engine refusal (`Empty`) on lane 3 puts "3 · reason" in the message line, which clears
 *   when the cue ends.
 * - bar: no bar read-out with no loop; on a 2- and a 4-bar loop at 240 BPM the bar steps one at a time
 *   across the loop boundary and the lit beat dot follows the scripted `Beat`; at rest it reads bar 1.
 * - controls: the two buttons hide after 3 s without the pointer and return on a pointer move and on a
 *   key; B still exits while they are hidden.
 * - pointer (per look): a press on lane 4's form sends `SelectTrack` 3; a press in Orbit's centre
 *   sends nothing.
 * - reduced: with prefers-reduced-motion the chips' animations are off and the canvas holds still
 *   across a scripted beat (it moves without it: the control). On the FIRST look only, and the comment
 *   at the group says what a per-look run needs first.
 * - legibility (per look, 1000x700, 1280x820, 1920x1080): chips, tempo, message line, buttons and the
 *   numeral do not overlap and stay inside the window; a chip number's cap height is at least 14 px.
 * - budget (per look, 1920x1080): five lanes PLAYING on an 8-bar loop, the frame's code warmed up by
 *   running it by hand: the mean script time of the stage's frame over 240 frames is under 6 ms, and the
 *   sampling heap profiler attributes under 32 bytes a frame to the frame's call tree. The floor is one
 *   boxed number a frame, about 16 to 22 bytes: the looper's `phaseValue` reads Date.now(). An array or
 *   an object a frame adds 32 bytes or more and fails. Closing the stage stops its rAF loop. Also prints
 *   the frame interval and the hidden looper lanes' cost.
 * - keys: a held A lights no key and sends native MIDI's router no note (`__lf.native.inputSent`) inside
 *   the view (it does outside); drum mode's 3
 *   plays no pad inside it but selects lane 3; V plays the Hi Tom pad outside the view and leaves the
 *   look alone (as the `stageNextView` action does there, a pedal's path), and inside it steps the look
 *   and sends the engine nothing but the `SetScope` that step owes; a pad key released with
 *   Shift down still releases its pad.
 * - shots: for the eye, per look and size: a rich scene (four lanes with different loops: playing,
 *   muted, overdubbing, stopped, and one empty), with the engine's live scope columns fed in over the
 *   whole loop and the newest one at the playhead, at three loop phases, a later take recording, an armed
 *   wait, a first take, LISTENING, the count-in and the rest state, in logs/stage-view/, plus tiled
 *   sheets (sheet-<look>-<a|b>.png).
 *
 * Cannot see the native engine (the looper state machine, count-in, takes, overdubs and refusals are
 * lf-engine's tests), Tauri IPC, WebView2, the GPU the owner's machine draws with, or the distance the
 * view is read from: the fake answers no command by itself, so every state on screen was scripted, and
 * the frame time is headless Chromium's script time, not a frame on the rig. The scope columns are
 * scripted too: nothing here says the engine folds what it claims to fold (`lf-engine`'s scope tests
 * and `verify/probes/scope-mirror.mjs` own that), and the `budget` case still runs the look with no
 * column at all, so what the fold and the per-column strips cost a frame is unmeasured.
 * Run: pnpm probe stage-view
 */
import assert from 'node:assert/strict';
import { mkdir, readFile } from 'node:fs/promises';
import { arg, probe } from '../harness/probe.ts';

const outDir = 'logs/stage-view';
const viewports = [[1000, 700], [1280, 820], [1920, 1080]];
/** Legibility is also checked at the app's minimum window (src-tauri/tauri.conf.json `minWidth`/`minHeight`). */
const legible = [[960, 600], ...viewports];
const RATE = 48000;
const PEAK_FRAMES = 1024; // the engine's waveform bin (`lf-engine/src/overview.rs`)
const BAR = 2 * RATE; // one 4/4 bar at 120 BPM
const LOOP = 8 * BAR;
const only = arg('case')?.split(',');

// ── The live scope taps ───────────────────────────────────────────────────────────────────────────
/** `lf_engine::scope::scope_bin_frames(48_000)`: a 4 ms column. */
const BIN = 192;
/** `lf_engine::scope::SCOPE_SOURCES`: the five lanes after their FX, the monitor, the master output. */
const SCOPE_SOURCES = 7;
const LOOP_COLUMNS = LOOP / BIN; // 4000 columns a pass on the 8-bar loop: about 3.3 to a device px
const SHORT = RATE / 2; // a half-second master loop: 125 columns, each about 10 device px wide
const SHORT_COLUMNS = SHORT / BIN;
/**
 * Columns a batch carries at most. The look folds at most `gx.length` (1024) new columns a frame and
 * the store's ring holds 1024, so a bigger batch would lose columns the engine never loses: the probe
 * would be measuring its own emit, not the fold.
 */
const CHUNK = 800;
/** Batch boundaries every `step` columns of a `cols`-column pass. */
const cutsEvery = (cols, step) => {
  const out = [];
  for (let c = 0; c < cols; c += step) out.push(c);
  out.push(cols);
  return out;
};
/** A loop's worth of column amplitudes: a pulse on every beat under a swell across the loop, so a
 * screenshot shows light shaped like a part rather than a band of noise. */
const columnAmp = (cols, beats) => Array.from({ length: cols }, (_, j) => {
  const t = (j / cols) * beats;
  const hit = 0.18 + 0.62 * Math.exp(-(t % 1) * 5) * (Math.floor(t) % 4 === 0 ? 1 : 0.72);
  return Math.min(1, hit * (0.5 + 0.5 * Math.sin(Math.PI * (j / cols)) ** 2));
});
/** A pass of silence with a loud pulse over columns [from, to): the accumulation cases' pattern, read
 * back at [from / cols, to / cols] of the bay's width. */
const pulseAmp = (cols, from, to) => Array.from({ length: cols }, (_, j) => (j >= from && j < to ? 1 : 0));
/** What each source carries in the rich scene: the five lanes (4 is EMPTY), the monitor under its own
 * line, the master output. */
const RICH_GAIN = { 0: 0.92, 1: 0.7, 2: 0.8, 3: 0.6, 4: 0, 5: 0.4, 6: 0.85 };
const RICH_CUTS = cutsEvery(LOOP_COLUMNS, CHUNK);
const RICH_AMP = columnAmp(LOOP_COLUMNS, 32);
/** Lane 1 alone, at the level the accumulation cases measure. */
const LANE1_ONLY = { 0: 0.8 };

/**
 * The page side of the live scope taps, installed by the probe's init script (so it survives the
 * reloads the `views` group does) as `window.__sc`. It builds a batch of columns the way the feed
 * carries one, pushes it through the engine fake and lets a stage frame draw it, and reads the scope
 * look's bays back off the canvas. `k` carries the engine's constants.
 */
function scopeRig(k) {
  const el = () => document.querySelector('.sv-canvas');
  /** The scope look's own layout (`src/ui/stage/scope.ts` `layout`): the HUD's band and side padding,
   * the gutter that carries the bay names, and the hero bay two lane-heights tall. */
  const geom = () => {
    const c = el();
    const dpr = c.width / c.clientWidth;
    const pad = Math.round(Math.min(24, Math.max(12, 0.02 * c.clientHeight)) * dpr);
    const x0 = pad + Math.round(44 * dpr);
    return {
      dw: c.width,
      dh: c.height,
      dpr,
      top: Math.round(0.1 * c.height),
      unit: Math.max(6, Math.floor((0.78 * c.height) / 7)),
      x0,
      tw: Math.max(8, c.width - pad - x0),
      cw: (span) => Math.max(1, Math.ceil(((Math.max(8, c.width - pad - x0)) * k.bin) / span)),
    };
  };
  /** Bay `b`'s rectangle of the trace (0 the hero, then one per lane) over the trace's x fraction
   * [from, to). A fraction of the trace's width IS a loop phase, so columns [a, b) of a pass read back
   * at [a / cols, b / cols] of their bay. */
  const rect = (g, b, from, to) => {
    const x = g.x0 + Math.round(from * g.tw);
    return {
      x,
      y: g.top + (b === 0 ? 0 : 2 * g.unit + (b - 1) * g.unit),
      w: g.x0 + Math.round(to * g.tw) - x,
      h: b === 0 ? 2 * g.unit : g.unit,
    };
  };
  const twoFrames = () => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
  /** Columns [from, to) of a pass that starts at device frame `base`, each source scaled by its own
   * `gain` and rounded to three decimals, as the feed rounds a column (`engine_io/feed.rs`). */
  const batch = (base, amp, gain, from, to, quiet, gap) => {
    const min = [];
    const max = [];
    for (let s = 0; s < k.sources; s++) {
      const mul = quiet ? 0 : (gain[s] ?? 0);
      const lo = [];
      const hi = [];
      for (let j = from; j < to; j++) {
        const v = Math.round(amp[j] * mul * 1000) / 1000;
        lo.push(-v);
        hi.push(v);
      }
      min.push(lo);
      max.push(hi);
    }
    return { frame: base + from * k.bin, bin: k.bin, gap, min, max };
  };
  window.__sc = {
    geom,
    batch,
    /** Each bay in `bays` (`[bay, from, to]`): the channel sums over its rectangle and how many of its
     * pixels are above the census's floor. A sum, not a count: the light a pass adds to a pixel is
     * what accumulation moves, and a count saturates as soon as the pixel is lit at all. */
    read: (bays) => {
      const g = geom();
      const d = el().getContext('2d').getImageData(0, 0, g.dw, g.dh).data;
      return bays.map(([b, from, to]) => {
        const box = rect(g, b, from, to);
        let r = 0;
        let gr = 0;
        let bl = 0;
        let lit = 0;
        for (let y = box.y; y < box.y + box.h; y++) {
          for (let x = box.x; x < box.x + box.w; x++) {
            const p = 4 * (y * g.dw + x);
            r += d[p];
            gr += d[p + 1];
            bl += d[p + 2];
            if (Math.max(d[p], d[p + 1], d[p + 2]) >= 40) lit++;
          }
        }
        return { bay: b, box: [box.x, box.y, box.w, box.h], r, g: gr, b: bl, lit, px: box.w * box.h };
      });
    },
    /**
     * `passes` passes of one column pattern over a master loop of `span` frames, cut into batches at
     * `cuts` with a stage frame drawn before the next goes in, then a partial pass of `tail` columns
     * (so the newest column can be left at the loop phase the scene's anchor shows). `silentAfter`
     * sends the pattern that many passes and silence after them. Returns the feed sequence it reached.
     *
     * The sweep opens with a one-column batch the engine could not splice (`gap`): the look resyncs on
     * a new epoch and draws nothing that frame, and this is also what parks the beam off the right
     * edge, so a baseline read taken after it holds no wandering playhead.
     */
    sweep: async (o) => {
      let seq = o.seq;
      const send = (base, from, to, quiet, gap) =>
        window.__lf.native.emit({ seq: ++seq, reset: false, events: [], scope: batch(base, o.amp, o.gain, from, to, quiet, gap) });
      send(o.pass0 * o.span - k.bin, 0, 1, true, true);
      await twoFrames();
      for (let p = 0; p <= o.passes; p++) {
        const base = (o.pass0 + p) * o.span;
        const end = p === o.passes ? o.tail : o.cuts[o.cuts.length - 1];
        const quiet = o.silentAfter > 0 && p >= o.silentAfter;
        for (let c = 0; c + 1 < o.cuts.length; c++) {
          const to = Math.min(o.cuts[c + 1], end);
          if (to <= o.cuts[c]) break;
          send(base, o.cuts[c], to, quiet, false);
          await twoFrames();
        }
      }
      return seq;
    },
  };
}

const lane = (state, extra = {}) => ({
  state,
  length: 0,
  armed: false,
  autoArmed: false,
  canUndo: false,
  canReverse: false,
  reversed: false,
  stopAt: null,
  fading: false,
  retakePass: 0,
  ...extra,
});
const laneEvent = (i, info, frame = 0) => ({ Lane: { frame, lane: i, info } });
const transport = (master, locked, bpm) => ({ Transport: { frame: 0, master, bpm, locked } });
const empties = (...lanes) => lanes.map((i) => laneEvent(i, lane('Empty')));
/** The clock anchor with `frame` rendering now. */
const anchorAt = (frame) => ({ frame, atMs: Date.now(), rate: RATE, grid: 0 });
/** The beat the engine last sent `at` frames into a 120 BPM loop. */
const beatAt = (at) => ({ Beat: { frame: Math.floor(at / (RATE / 2)) * (RATE / 2), beatInBar: Math.floor(at / (RATE / 2)) % 4, countLeft: 0, clicked: false } });

/** `bins` waveform bins of lane `i` at 120 BPM, each `style` a different kind of part: 0 a kick-like pulse
 * on the beat, 1 a pad swelling over two bars, 2 sparse eighth-note plucks, 3 a busy texture with accents. */
function wave(i, frames, style = i, bins = Math.ceil(frames / PEAK_FRAMES)) {
  const beat = RATE / 2 / PEAK_FRAMES;
  let seed = 7 + 31 * style;
  const rnd = () => (seed = (seed * 16807) % 2147483647) / 2147483647;
  const max = [];
  const min = [];
  for (let b = 0; b < bins; b++) {
    const t = b / beat;
    const inBeat = t % 1;
    let a;
    if (style === 0) a = 0.06 + 0.8 * Math.exp(-inBeat * 7) * (Math.floor(t) % 4 === 0 ? 1 : 0.7);
    else if (style === 1) a = 0.12 + 0.42 * Math.sin(Math.PI * ((t / 8) % 1)) ** 2 * (0.8 + 0.2 * Math.sin(t * 2.1));
    else if (style === 2) a = 0.04 + [1, 0, 1, 1, 0, 1, 0, 0, 1, 1, 0, 1, 0, 0, 1, 0][Math.floor(t * 2) % 16] * 0.6 * Math.exp(-((t * 2) % 1) * 3.2);
    else a = 0.1 + 0.3 * Math.abs(Math.sin(t * Math.PI * 2)) * (0.5 + 0.5 * Math.sin(t * 0.37 + 1)) + (t % 8 < 0.5 ? 0.35 : 0);
    a = Math.min(1, a * (0.85 + 0.3 * rnd()));
    max.push(a);
    min.push(-a * (0.7 + 0.3 * rnd()));
  }
  return { lane: i, start: 0, count: bins, min, max };
}
const playing = (length) => lane('Playing', { length, canReverse: true });
const stopped = (length) => lane('Stopped', { length, canReverse: true });

await probe(async ({ browser, open }) => {
  await mkdir(outDir, { recursive: true });
  // Time the stage's frame from outside: its rAF callback (`stageFrame`, stage-loop.ts) and the looper
  // lanes' (`frame`, waveform.ts) run through one trampoline each, so the wrapper allocates nothing.
  const init = async (p) => {
    await p.addInitScript(() => {
      window.__lfEngineFake = true;
      const stats = (window.__raf = { on: false, manual: false, stage: null, stageFrame: { seen: 0, n: 0, ms: 0, max: 0 }, frame: { seen: 0, n: 0, ms: 0, max: 0 } });
      const real = window.requestAnimationFrame.bind(window);
      const trampolines = new Map();
      window.requestAnimationFrame = (cb) => {
        const s = cb.name === 'stageFrame' || cb.name === 'frame' ? stats[cb.name] : null;
        if (!s) return real(cb);
        if (cb.name === 'stageFrame') {
          stats.stage = cb;
          // The probe drives the frame by hand (the JIT's warm-up): the frame already scheduled keeps the loop.
          if (stats.manual) return 0;
        }
        let t = trampolines.get(cb);
        if (!t) {
          t = (now) => {
            s.seen++;
            if (!stats.on) return cb(now);
            if (s.n === 0) s.first = now;
            s.last = now;
            const t0 = performance.now();
            cb(now);
            const d = performance.now() - t0;
            s.n++;
            s.ms += d;
            if (d > s.max) s.max = d;
          };
          trampolines.set(cb, t);
        }
        return real(t);
      };
    });
    await p.addInitScript(scopeRig, { bin: BIN, sources: SCOPE_SOURCES });
  };
  const { page, consoleErrors } = await open({ viewport: { width: 1280, height: 820 }, init });
  let seq = 0;
  const emitOn = (p, frame) => p.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], ...frame });
  const emit = (frame) => emitOn(page, frame);
  const sent = () => page.evaluate(() => window.__lf.native.sent.slice());
  const clearSent = () => page.evaluate(() => void (window.__lf.native.sent.length = window.__lf.native.inputSent.length = 0));
  /** The notes the keyboard sent native MIDI's router (`input.note`), as `{ note, on }`. */
  const notesSent = () => page.evaluate(() => window.__lf.native.inputSent.filter((e) => e.note).map((e) => e.note));
  const sentAtLeast = async (count) => {
    await page.waitForFunction((n) => window.__lf.native.sent.length >= n, count, { timeout: 5000 });
    return sent();
  };
  /** Press `key` with the sent log cleared and return the one command it sent. */
  const pressSends = async (key, expected, what) => {
    await clearSent();
    await page.keyboard.press(key);
    assert.deepEqual(await sentAtLeast(1), [expected], what);
  };
  const boot = async (p) => {
    await p.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });
    await emitOn(p, {
      reset: true,
      settings: [],
      events: [...empties(0, 1, 2, 3, 4), transport(0, false, 120), { Selected: { frame: 0, lane: 0 } }],
      anchor: anchorAt(0),
      meter: { peak: 0, clip: false },
    });
  };
  const VIEWS = await page.evaluate(() => import('/src/ui/stage/views.ts').then((m) => m.STAGE_VIEWS.map((v) => ({ id: v.id, name: v.name, wantsScope: v.wantsScope === true }))));
  const view = () => page.evaluate(() => document.querySelector('.sv')?.dataset.view ?? null);
  const isOpen = () => page.evaluate(() => {
    const sv = document.querySelector('.sv');
    return { open: !!sv, cmdInert: document.querySelector('.cmd').inert, stageInert: document.querySelector('main.stage').inert };
  });
  const expectOpen = async (want, how) => {
    await page.waitForFunction((w) => !!document.querySelector('.sv') === w, want);
    assert.deepEqual(await isOpen(), { open: want, cmdInert: want, stageInert: want }, `${how}: stage view ${want ? 'open' : 'closed'}, normal UI inert iff open`);
  };
  const setOpen = async (want) => {
    if ((await isOpen()).open === want) return;
    await page.keyboard.press(want ? 'b' : 'Escape');
    await expectOpen(want, want ? 'B opens' : 'Escape closes');
  };
  /** Step to look `id` with V (the stage is open). */
  const showView = async (id) => {
    for (let k = 0; k < VIEWS.length && (await view()) !== id; k++) await page.keyboard.press('v');
    assert.equal(await view(), id, `the stage shows ${id}`);
  };
  /** A fresh engine, the stage closed, the default window, motion on. */
  const fresh = async () => {
    await page.emulateMedia({ reducedMotion: 'no-preference' });
    await page.setViewportSize({ width: 1280, height: 820 });
    await setOpen(false);
    await boot(page);
    await page.evaluate(() => { for (let i = 0; i < 5; i++) window.__lf.looper.setMute(i, false); });
    await clearSent();
  };
  const settle = (ms = 250) => page.waitForTimeout(ms);
  const failures = [];
  const group = async (name, body) => {
    if (only && !only.includes(name)) return;
    console.log(`── ${name}`);
    try {
      await fresh();
      await body();
      console.log(`ok ${name}`);
    } catch (error) {
      failures.push(name);
      console.log(`FAIL ${name}: ${error.message}`);
    }
  };

  // ── the live scope taps ───────────────────────────────────────────────────────────────────────────
  /** Drive the engine fake's scope columns (`scopeRig`'s `sweep`) and keep `seq` in step. */
  const sweep = async (o) => {
    seq = await page.evaluate((s) => window.__sc.sweep(s), { tail: 0, silentAfter: 0, pass0: 1, ...o, seq });
  };
  /** Each `[bay, from, to]` of the scope look's bays, read off the canvas (`scopeRig`'s `read`). */
  const readBays = (bays) => page.evaluate((b) => window.__sc.read(b), bays);

  // ── scenes ────────────────────────────────────────────────────────────────────────────────────────
  /** Four lanes with different loops over 8 bars (playing, muted, overdubbing, stopped) and one empty,
   * lane 3 selected, the input at a working level, `at` frames into the loop, and the engine's live
   * scope columns over the whole loop with the newest one at the drawn playhead (the scope look places
   * a column by its own device frame, so the partial last pass is what puts it there). The columns go
   * in LAST, after `settleMs` has let the lane light rise, because they are the one thing on the
   * screenshot sheets with a clock on it: the look fades a stale stream out 250 ms after the last
   * batch, so a scene that emitted them first would be photographed half faded. */
  const rich = async (at, settleMs = 0) => {
    await emit({
      events: [
        transport(LOOP, true, 120),
        laneEvent(0, playing(LOOP)),
        laneEvent(1, playing(LOOP)),
        laneEvent(2, lane('Overdubbing', { length: LOOP })),
        laneEvent(3, stopped(LOOP)),
        ...empties(4),
        { Selected: { frame: 0, lane: 2 } },
        beatAt(at),
      ],
      anchor: anchorAt(at),
      peaks: [0, 1, 2, 3].map((i) => wave(i, LOOP)),
      meter: { peak: 0.32, clip: false },
    });
    await page.evaluate(() => window.__lf.looper.setMute(1, true));
    if (settleMs > 0) await settle(settleMs);
    await sweep({ span: LOOP, amp: RICH_AMP, gain: RICH_GAIN, cuts: RICH_CUTS, passes: 3, tail: Math.round(at / BIN) });
  };
  /** `count` lanes with loops over 8 bars, all PLAYING or all STOPPED. */
  const loops = (count, state, at = RATE) => emit({
    events: [transport(LOOP, true, 120), ...[0, 1, 2, 3, 4].map((i) => laneEvent(i, i < count ? lane(state, { length: LOOP, canReverse: true }) : lane('Empty'))), beatAt(at)],
    anchor: anchorAt(at),
    peaks: Array.from({ length: count }, (_, i) => wave(i, LOOP, i % 4)),
  });
  /** A first take's count-in on lane 1: no loop yet, the click counts `left`. */
  const countIn = (left) => emit({
    events: [laneEvent(0, lane('Recording', { armed: true })), transport(0, true, 120), { Beat: { frame: 0, beatInBar: (4 - left) % 4, countLeft: left, clicked: true } }],
    anchor: anchorAt(0),
  });
  /** A first take on lane 1, `seconds` in (it began on the counted downbeat, a bar after the press). */
  const firstTake = (seconds) => emit({
    events: [laneEvent(0, lane('Recording'), BAR), transport(0, true, 120), { Beat: { frame: BAR, beatInBar: 0, countLeft: 0, clicked: true } }],
    anchor: anchorAt(BAR + seconds * RATE),
    peaks: [wave(0, 0, 2, Math.floor((seconds * RATE) / PEAK_FRAMES))],
  });
  /** Lane 1 plays its 8-bar loop; lane 2 is a later take: armed for the boundary, or recording since it. */
  const laterTake = async (recording, at) => {
    await emit({ events: [transport(LOOP, true, 120), laneEvent(0, playing(LOOP)), ...empties(1, 2, 3, 4), { Selected: { frame: 0, lane: 1 } }, beatAt(at)], anchor: anchorAt(at), peaks: [wave(0, LOOP)] });
    if (!recording) return emit({ events: [laneEvent(1, lane('Recording', { armed: true }))] });
    return emit({ events: [laneEvent(1, lane('Recording'), LOOP)], anchor: anchorAt(LOOP + at), peaks: [wave(1, 0, 3, Math.floor(at / PEAK_FRAMES))] });
  };

  // ── readers ───────────────────────────────────────────────────────────────────────────────────────
  /** The tokens' colours as the browser computes them. */
  const tokens = await page.evaluate(() => Object.fromEntries(['bg', 'faint', 'dim', 'play', 'rec', 'dub', 'cyan', 'engaged'].map((name) => {
    const el = document.createElement('i');
    el.style.color = `var(--${name})`;
    document.body.append(el);
    const rgb = getComputedStyle(el).color;
    el.remove();
    return [name, rgb];
  })));
  const chips = () => page.evaluate(() => [...document.querySelectorAll('.sv-chip')].map((el) => {
    const cs = getComputedStyle(el);
    const box = el.getBoundingClientRect();
    return {
      state: el.dataset.state,
      muted: el.dataset.muted === 'true',
      label: el.getAttribute('aria-label'),
      current: el.getAttribute('aria-current') === 'true',
      color: cs.color,
      face: cs.backgroundColor,
      animation: cs.animationName,
      box: [box.x, box.y, box.width, box.height].map(Math.round).join(','),
    };
  }));
  const hud = () => page.evaluate(() => ({
    msg: document.querySelector('.sv-msg').textContent,
    tone: document.querySelector('.sv-msg').dataset.tone ?? null,
    count: document.querySelector('.sv-count').textContent,
    bar: document.querySelector('.sv-bar')?.textContent ?? null,
    dot: [...document.querySelectorAll('.sv-dot')].findIndex((d) => d.classList.contains('is-on')),
    pill: document.querySelector('.sv-btn--view').textContent.trim(),
    text: document.querySelector('.sv').textContent,
  }));
  /** The stage canvas two frames on, counted by colour family: play green, rec red, dub amber, the
   * warm-white of the selection, grey (and the `bright` greys among them), and everything above the floor (`lit`); `warmAt` is the mean
   * distance of the warm-white pixels from the numeral's anchor and their mean height, in CSS px. */
  const census = () => page.evaluate(async () => {
    await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
    const sv = document.querySelector('.sv');
    const c = document.querySelector('.sv-canvas');
    const scale = c.width / c.clientWidth;
    const ax = parseFloat(sv.style.getPropertyValue('--sv-count-x')) * scale;
    const ay = parseFloat(sv.style.getPropertyValue('--sv-count-y')) * scale;
    const d = c.getContext('2d').getImageData(0, 0, c.width, c.height).data;
    const out = { play: 0, rec: 0, dub: 0, warm: 0, grey: 0, bright: 0, lit: 0 };
    let dist = 0;
    let height = 0;
    for (let p = 0; p < d.length; p += 4) {
      const r = d[p], g = d[p + 1], b = d[p + 2];
      if (Math.max(r, g, b) < 40) continue;
      out.lit++;
      if (g > 80 && g - r > 40 && g - b > 15) out.play++;
      else if (r > 140 && g < 90 && r - g > 70) out.rec++;
      else if (r > 150 && g >= 90 && r - b > 80) out.dub++;
      else if (r > 200 && g > 195 && b > 180 && r >= b) {
        out.warm++;
        const x = (p / 4) % c.width, y = Math.floor(p / 4 / c.width);
        dist += Math.hypot(x - ax, y - ay);
        height += y;
      } else if (Math.abs(r - g) < 14 && Math.abs(g - b) < 18) {
        out.grey++;
        if (g > 85) out.bright++;
      }
    }
    return { ...out, warmDist: Math.round(dist / Math.max(1, out.warm) / scale), warmY: Math.round(height / Math.max(1, out.warm) / scale) };
  });
  /** Wait until no lane's slow light can move a pixel any more, and return the shown phase. `light.slow`
   * is a 400 ms follower Strata rounds into a ribbon's height (`rowH * (0.84 + 0.16 * slow)`): a stopped
   * lane's ribbon takes its last pixel step about a second after the stop, later on a slow runner, and
   * that step is not the loop moving. `rowH * 0.84` is a multiple of 1/25, so the last step comes at
   * `slow >= 0.125 / rowH`: under 1e-4 no ribbon lower than 1250 px has one left. Only lanes that do
   * not sound fall, so a fixture with a sounding lane never gets here. */
  const lightAtRest = async () => {
    // Polled from here: `waitForFunction` takes a predicate's promise as truthy and would not wait.
    const read = () => page.evaluate(() => import('/src/ui/stage/visual.ts').then(({ light }) => ({ phase: light.phase, move: light.move, slow: Math.max(...light.slow) })));
    const t0 = Date.now();
    let l = await read();
    while (l.slow >= 1e-4) {
      assert.ok(Date.now() - t0 < 15_000, `the stage's light never came to rest: ${JSON.stringify(l)}`);
      await settle(50);
      l = await read();
    }
    return { ...l, waitedMs: Date.now() - t0 };
  };
  /** Pixels that differ between the canvas now and `ms` later. */
  const moved = (ms) => page.evaluate(async (wait) => {
    const c = document.querySelector('.sv-canvas');
    const grab = async () => {
      await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
      return c.getContext('2d').getImageData(0, 0, c.width, c.height).data;
    };
    const a = await grab();
    await new Promise((r) => setTimeout(r, wait));
    const b = await grab();
    let n = 0;
    for (let p = 0; p < a.length; p += 4) if (Math.abs(a[p] - b[p]) + Math.abs(a[p + 1] - b[p + 1]) + Math.abs(a[p + 2] - b[p + 2]) > 24) n++;
    return n;
  }, ms);
  /** HUD boxes: whether any two overlap, any leaves the window, and a chip number's cap height. */
  const layout = () => page.evaluate(async () => {
    await document.fonts.ready;
    const parts = { chips: '.sv-chips', tempo: '.sv-tempo', msg: '.sv-msg', controls: '.sv-ctl', numeral: '.sv-count__n' };
    const boxes = Object.entries(parts).map(([name, q]) => [name, document.querySelector(q)?.getBoundingClientRect()]).filter(([, b]) => b && b.width > 0);
    const problems = [];
    for (const [name, b] of boxes) {
      if (b.left < 0 || b.top < 0 || b.right > innerWidth || b.bottom > innerHeight) problems.push(`${name} leaves the window`);
      for (const [other, o] of boxes) if (name < other && b.left < o.right && o.left < b.right && b.top < o.bottom && o.top < b.bottom) problems.push(`${name} overlaps ${other}`);
    }
    const chip = getComputedStyle(document.querySelector('.sv-chip'));
    const ctx = document.createElement('canvas').getContext('2d');
    ctx.font = `${chip.fontWeight} ${chip.fontSize} ${chip.fontFamily}`;
    return { problems, capPx: Math.round(ctx.measureText('5').actualBoundingBoxAscent * 10) / 10, boxes: Object.fromEntries(boxes.map(([n, b]) => [n, [b.x, b.y, b.width, b.height].map(Math.round).join(',')])) };
  });
  let vp = '1280x820';
  const shots = [];
  const shoot = async (scene) => {
    const file = `${outDir}/${vp}-${await view()}-${scene}.png`;
    await page.screenshot({ path: file });
    shots.push({ file, vp, view: await view(), scene });
  };

  await boot(page);
  // Nothing may remount under the stage view: remember a normal lane's canvas element.
  await page.evaluate(() => { window.__svCanvas = document.querySelector('.lp-lane canvas'); });

  await group('enter', async () => {
    await expectOpen(false, 'at launch');
    await page.keyboard.press('b');
    await expectOpen(true, 'B key opens');
    await page.keyboard.press('b');
    await expectOpen(false, 'B key closes');
    await page.evaluate(() => import('/src/app/actions.ts').then((m) => m.runAction('stageView')));
    await expectOpen(true, 'stageView action opens');
    await page.evaluate(() => import('/src/app/actions.ts').then((m) => m.runAction('stageView')));
    await expectOpen(false, 'stageView action closes');
    await page.getByRole('button', { name: 'Stage view', exact: true }).click();
    await expectOpen(true, 'command-bar cap opens');
    await page.getByRole('button', { name: 'Exit stage view', exact: true }).click();
    await expectOpen(false, 'the exit button closes');
    await page.keyboard.press('B');
    await expectOpen(true, 'Shift+B opens');
    assert.equal(await page.evaluate(() => document.activeElement === document.querySelector('.sv')), true, 'focus moves to the dialog root');
    assert.equal(await page.locator('.sv').getAttribute('role'), 'dialog');
    assert.equal(await page.locator('.sv-canvas').getAttribute('aria-hidden'), 'true', 'the canvas is hidden from the accessibility tree');
    assert.equal(await page.locator('.sv-msg').getAttribute('aria-live'), 'polite', 'the message line is a polite live region');
    await page.keyboard.press('Escape');
    await expectOpen(false, 'Escape closes');
    const same = await page.evaluate(() => window.__svCanvas === document.querySelector('.lp-lane canvas') && window.__svCanvas.isConnected);
    assert.equal(same, true, 'the normal lane canvas is the same element after the stage view (no remount)');
  });

  await group('views', async () => {
    await page.evaluate(() => localStorage.removeItem('lf.stageView'));
    await page.reload();
    await page.waitForFunction(() => '__lf' in window);
    await boot(page);
    await setOpen(true);
    assert.equal(await view(), VIEWS[0].id, `a first open shows the default look, ${VIEWS[0].id}`);
    assert.equal(VIEWS[0].id, 'scope', 'the default look is scope, the one that draws the engine\'s live columns');
    await pressSends('3', { SelectTrack: 2 }, 'a digit inside the view sends SelectTrack');
    await emit({ events: [{ Selected: { frame: 0, lane: 2 } }] });
    const seen = [];
    for (let k = 0; k < VIEWS.length; k++) {
      const h = await hud();
      seen.push(await view());
      assert.equal(h.pill, VIEWS[k].name, `the view switch names the look (${h.pill})`);
      assert.equal(await page.locator('.sv canvas').count(), 1, 'one stage canvas');
      assert.deepEqual((await chips()).map((c) => c.current), [false, false, true, false, false], `lane 3 stays selected in ${VIEWS[k].id}`);
      await clearSent();
      if (k % 2 === 0) await page.keyboard.press('v');
      else {
        await page.mouse.move(640, 400);
        await page.locator('.sv-btn--view').click();
      }
      await settle(120);
      // A look switch touches the transport with nothing, as it always did. The one command it may
      // send is the live scope columns being asked for or let go of, and only where `wantsScope`
      // actually changes between the two looks: pin that exactly, so a switch that sends a command
      // it should not, or drops one it should, still fails here.
      const from = VIEWS[k];
      const to = VIEWS[(k + 1) % VIEWS.length];
      const wantsColumns = (d) => d.wantsScope === true;
      const owed = wantsColumns(from) === wantsColumns(to) ? [] : [{ SetScope: wantsColumns(to) }];
      assert.deepEqual(await sent(), owed, `switching ${from.id} to ${to.id} sends the engine only what the columns need`);
    }
    console.log(JSON.stringify({ cycle: seen, wrapsTo: await view() }));
    assert.deepEqual(seen, VIEWS.map((v) => v.id), 'V and the view switch step the looks in order');
    assert.equal(await view(), VIEWS[0].id, 'the cycle wraps to the first look');
    // Leave it on the last look: it must survive a close, a reopen and a reload.
    const lastView = VIEWS.at(-1).id;
    await showView(lastView);
    await setOpen(false);
    await setOpen(true);
    assert.equal(await view(), lastView, 'the look survives close and reopen');
    const size = await page.evaluate(() => {
      const c = document.querySelector('.sv-canvas');
      return { width: c.width, height: c.height, cw: c.clientWidth, ch: c.clientHeight, dpr: devicePixelRatio };
    });
    assert.deepEqual([size.width, size.height], [Math.round(size.cw * Math.min(size.dpr, 1.5)), Math.round(size.ch * Math.min(size.dpr, 1.5))], 'the backing store matches the element');
    await page.setViewportSize({ width: 1000, height: 700 });
    await page.waitForFunction(() => document.querySelector('.sv-canvas').width === 1000 && document.querySelector('.sv-canvas').height === 700);
    await page.reload();
    await page.waitForFunction(() => '__lf' in window);
    await boot(page);
    assert.equal((await isOpen()).open, false, 'a reload starts in the normal UI (open is session-only)');
    await setOpen(true);
    assert.equal(await view(), lastView, 'the look survives a page reload');
    await page.evaluate(() => { window.__svCanvas = document.querySelector('.lp-lane canvas'); });
    // The device-pixel ratio is capped at 1.5: on a 2x screen the backing store is 1.5x the element.
    const context = await browser.newContext({ deviceScaleFactor: 2 });
    const hi = await open({ context, viewport: { width: 1000, height: 700 }, init });
    await boot(hi.page);
    await hi.page.keyboard.press('b');
    await hi.page.waitForSelector('.sv-canvas');
    const capped = await hi.page.evaluate(() => {
      const c = document.querySelector('.sv-canvas');
      return { width: c.width, cw: c.clientWidth, dpr: devicePixelRatio };
    });
    await context.close();
    console.log(JSON.stringify({ size, capped }));
    assert.equal(capped.dpr, 2, 'control: the screen is 2x');
    assert.equal(capped.width, Math.round(capped.cw * 1.5), 'the device-pixel ratio is capped at 1.5');
  });

  await group('chips', async () => {
    await setOpen(true);
    const expectChips = async (scene, want, selected) => {
      await settle(120);
      const got = await chips();
      console.log(JSON.stringify({ scene, chips: got.map((c) => `${c.state}${c.muted ? '+muted' : ''}${c.current ? '*' : ''}`) }));
      for (const [i, [state, token, word]] of want.entries()) {
        assert.equal(got[i].state, state, `${scene}: chip ${i + 1} reads ${got[i].state}, want ${state}`);
        assert.match(got[i].label, new RegExp(`^Track ${i + 1}, ${word}`), `${scene}: chip ${i + 1} is named "${got[i].label}"`);
        if (i === selected) {
          assert.equal(got[i].face, tokens.engaged, `${scene}: the selected chip has the warm face`);
          assert.equal(got[i].color, tokens.bg, `${scene}: the selected chip's number is dark`);
        } else assert.equal(got[i].color, tokens[token], `${scene}: chip ${i + 1}'s number is ${token}`);
      }
      assert.deepEqual(got.map((c) => c.current), got.map((_, i) => i === selected), `${scene}: aria-current follows the selection`);
      return got;
    };
    const E = ['empty', 'faint', 'empty'];
    const atEmpty = await expectChips('empty', [E, E, E, E, E], 0);
    await emit({ events: [{ Selected: { frame: 0, lane: 4 } }] });
    await countIn(4);
    await expectChips('count-in', [['armed', 'dub', 'armed'], E, E, E, E], 4);
    await emit({ events: [laneEvent(0, lane('Recording', { autoArmed: true }))] });
    // LISTEN is cyan across the app (the app.css header; the looper lanes), never the armed amber.
    await expectChips('listening', [['listening', 'cyan', 'listening'], E, E, E, E], 4);
    await firstTake(1);
    const rec = await expectChips('recording', [['rec', 'rec', 'recording'], E, E, E, E], 4);
    assert.equal(rec[0].animation, 'sv-rec', 'a recording chip pulses');
    await emit({
      events: [transport(LOOP, true, 120), laneEvent(0, playing(LOOP)), laneEvent(1, stopped(LOOP)), laneEvent(2, playing(LOOP)), laneEvent(3, lane('Overdubbing', { length: LOOP })), ...empties(4)],
      anchor: anchorAt(RATE),
      peaks: [0, 1, 2, 3].map((i) => wave(i, LOOP)),
    });
    await page.evaluate(() => window.__lf.looper.setMute(2, true));
    const mixed = await expectChips('mixed', [['play', 'play', 'playing'], ['stop', 'dim', 'stopped'], ['play', 'faint', 'muted'], ['dub', 'dub', 'overdubbing'], E], 4);
    assert.equal(mixed[2].muted, true, 'a muted playing lane is marked muted');
    await emit({ events: [laneEvent(4, lane('Recording', { armed: true })), { Beat: { frame: RATE, beatInBar: 2, countLeft: 0, clicked: false } }] });
    const armed = await expectChips('armed later take', [['play', 'play', 'playing'], ['stop', 'dim', 'stopped'], ['play', 'faint', 'muted'], ['dub', 'dub', 'overdubbing'], ['armed', 'dub', 'armed']], 4);
    assert.deepEqual(armed.map((c) => c.box), atEmpty.map((c) => c.box), 'the chips did not move between EMPTY and the mixed states');
    const { text } = await hud();
    console.log(JSON.stringify({ stageText: text }));
    assert.doesNotMatch(text, /STOP\b|EMPTY|PLAY|\bDUB\b|\bREC\b|MUTE|REV|dB|\d s\b|VOL|BARS?/, 'no state word, dB, loop seconds or MUTE/REV flag is left');
  });

  for (const v of VIEWS) {
    await group(`pixels-${v.id}`, async () => {
      await setOpen(true);
      await showView(v.id);
      const empty = await census();
      await loops(1, 'Playing');
      await settle(400);
      const play = await census();
      await page.evaluate(() => window.__lf.looper.setMute(0, true));
      await settle(300);
      const muted = await census();
      await page.evaluate(() => window.__lf.looper.setMute(0, false));
      await loops(1, 'Stopped');
      await settle(900);
      const stop = await census();
      // At rest the loop is cued at its start: the engine's phase runs on, the drawing holds still.
      const phases = await page.evaluate(async () => {
        const a = window.__lf.looper.phaseValue();
        await new Promise((r) => setTimeout(r, 300));
        return [a, window.__lf.looper.phaseValue()];
      });
      await lightAtRest();
      const restMoved = await moved(300);
      await loops(1, 'Playing');
      await settle(300);
      const playMoved = await moved(300);
      await emit({ events: [laneEvent(0, lane('Overdubbing', { length: LOOP }))] });
      await settle(400);
      const dub = await census();
      await laterTake(true, 3 * BAR);
      await settle(300);
      const rec = await census();
      console.log(JSON.stringify({ view: v.id, empty, play, muted, stop, dub, rec, restMoved, playMoved, phases }));
      assert.ok(play.play > 1500, `PLAYING draws in the play colour (${play.play} px)`);
      assert.ok(empty.play === 0 && empty.lit < play.lit / 3, `EMPTY is near the floor (${empty.lit} lit px against ${play.lit})`);
      assert.ok(muted.play < play.play * 0.02, `MUTED has no play colour left (${muted.play} px)`);
      assert.ok(muted.grey > empty.grey + 500, `MUTED draws its loop in grey (${muted.grey} px against ${empty.grey} empty)`);
      assert.ok(stop.play < play.play * 0.02 && stop.bright > muted.bright + 1000, `STOPPED is grey and brighter than MUTED (${stop.bright} bright grey px against ${muted.bright})`);
      assert.ok(dub.dub > 60 && dub.play > 500, `an overdub adds amber over the loop's green (${dub.dub} amber, ${dub.play} green px)`);
      assert.ok(rec.rec > 300, `a recording take draws in rec-red (${rec.rec} px)`);
      assert.ok(phases[1] !== phases[0], 'control: the engine phase runs on at rest');
      assert.ok(restMoved < 40, `at rest the drawing holds still (${restMoved} px moved in 300 ms)`);
      assert.ok(playMoved > 400, `control: a playing loop moves (${playMoved} px in 300 ms)`);
      // The warm-white selection mark follows the selected lane.
      await loops(4, 'Stopped');
      await emit({ events: [{ Selected: { frame: 0, lane: 0 } }] });
      await settle(900);
      const first = await census();
      await pressSends('4', { SelectTrack: 3 }, 'a digit inside the view sends SelectTrack');
      await emit({ events: [{ Selected: { frame: 0, lane: 3 } }] });
      await settle(150);
      const fourth = await census();
      console.log(JSON.stringify({ view: v.id, selection: { first: [first.warm, first.warmDist, first.warmY], fourth: [fourth.warm, fourth.warmDist, fourth.warmY] } }));
      assert.ok(first.warm > 200 && fourth.warm > 200, 'the selected lane carries a warm-white mark');
      if (v.id === 'orbit') assert.ok(fourth.warmDist < first.warmDist - 20, `orbit: the selection circle moves inward for lane 4 (${first.warmDist} to ${fourth.warmDist} px)`);
      else assert.ok(fourth.warmY > first.warmY + 40, `${v.id}: the selection mark moves down for lane 4 (${first.warmY} to ${fourth.warmY} px)`);
    });
  }

  await group('scope', async () => {
    // The engine's live scope columns, which only the SCOPE look draws: the taps being asked for and
    // let go, the columns reaching the bays, the accumulation the look's whole claim rests on, and an
    // xrun costing one 4 ms window rather than the trace. Nothing but this group emits a batch, so
    // every other group still runs the look with no column at all, as the browser rig did before them.
    await setOpen(true);
    await showView('scope');
    await setOpen(false);
    await settle(150);

    // ── 1. the taps are asked for, and let go ──────────────────────────────────────────────────────
    // `src/app.tsx` owns the one `SetScope`, and waits for the feed's FIRST reset frame before it
    // sends anything; `fresh` sent that frame, so the ask is owed from here on.
    const other = VIEWS.find((d) => !d.wantsScope);
    assert.ok(VIEWS[0].id === 'scope' && VIEWS[0].wantsScope === true, 'SCOPE is the default look and the one that wants the columns');
    assert.ok(other, 'another look does not want them (the control for letting them go)');
    const asked = [];
    const step = async (what, act) => {
      await clearSent();
      await act();
      await settle(150);
      const got = await sent();
      asked.push([what, got]);
      return got;
    };
    const opened = await step('open on SCOPE', () => setOpen(true));
    const away = await step(`step to ${VIEWS[1].id}`, () => page.keyboard.press('v'));
    const back = await step('step back to SCOPE', () => showView('scope'));
    const closed = await step('close while SCOPE shows', () => setOpen(false));
    console.log(JSON.stringify({ taps: asked }));
    assert.deepEqual(opened, [{ SetScope: true }], 'opening the view on SCOPE asks the engine for the taps, and sends nothing else');
    // Which step owes which command is the `views` group's assertion, computed from the looks' own
    // `wantsScope`; this one owes the OPEN and the CLOSE, and that a step back asks again.
    assert.deepEqual(away, VIEWS[1].wantsScope ? [] : [{ SetScope: false }], `stepping to ${VIEWS[1].id} lets the taps go, and sends nothing else`);
    assert.deepEqual(back.at(-1), { SetScope: true }, `stepping back to SCOPE asks for them again (${JSON.stringify(back)})`);
    assert.deepEqual(closed, [{ SetScope: false }], 'closing the view while SCOPE shows lets the taps go');
    assert.deepEqual(asked.flatMap(([, cs]) => cs).filter((c) => !('SetScope' in c)), [], 'none of those steps touches the transport');

    // ── 2. the columns draw ────────────────────────────────────────────────────────────────────────
    /** `count` lanes PLAYING over a master loop of `span` frames and NO recorded take: the still
     * bitmap holds no contour, so everything lit inside a bay is the live light or the beam.
     *
     * The wait is long on purpose, and twice over. A bay whose lane does not sound CLEARS its live
     * layer instead of fading it, so each lane's light has to be up (150 ms) before any column lands.
     * And a reset frame empties the MIRROR but not the look's trace canvas: what the scene before left
     * there is carried away by the stale-stream fade, which needs 250 ms of silence and then 400 ms.
     * Measured: at a 300 ms wait the previous case's light was still 87 % standing, the next batch
     * froze the fade where it stood, and a baseline read there made a dim case measure NEGATIVE
     * light (2026-10-09). */
    const liveLanes = async (span, count) => {
      await boot(page);
      await emit({
        events: [transport(span, true, 120), ...[0, 1, 2, 3, 4].map((i) => laneEvent(i, i < count ? playing(span) : lane('Empty'))), { Selected: { frame: 0, lane: 0 } }],
        anchor: anchorAt(0),
      });
      await settle(900);
    };
    await setOpen(true);
    assert.equal(await view(), 'scope', 'the SCOPE look shows');
    const HERO = [0, 0.04, 0.9];
    const LANE1 = [1, 0.04, 0.9];
    await liveLanes(LOOP, 5);
    const darkBays = await readBays([HERO, LANE1]);
    const dark = await census();
    await sweep({ span: LOOP, amp: RICH_AMP, gain: RICH_GAIN, cuts: RICH_CUTS, passes: 3 });
    const litBays = await readBays([HERO, LANE1]);
    const lit = await census();
    console.log(JSON.stringify({ columnsDraw: { census: { noColumns: dark, columns: lit }, hero: [darkBays[0], litBays[0]], lane1: [darkBays[1], litBays[1]] } }));
    assert.ok(lit.play > dark.play + 20_000, `the lane bays carry live light that was not there before the columns (${lit.play} play-green px against ${dark.play}, whose only green is the beam)`);
    assert.ok(litBays[1].lit > darkBays[1].lit + 8000, `lane 1's bay: the live light is new (${litBays[1].lit} lit px against ${darkBays[1].lit}, whose only light is the beam)`);
    assert.ok(litBays[1].g > darkBays[1].g * 1.4, `lane 1's bay: and it is the lane's own green (${litBays[1].g} green over its rectangle against ${darkBays[1].g})`);
    assert.ok(litBays[0].lit > darkBays[0].lit + 10_000, `the hero bay carries the engine's output as a body (${litBays[0].lit} lit px against ${darkBays[0].lit}, which is the beam alone)`);
    assert.ok(litBays[0].r > darkBays[0].r * 1.4, `and that body is the warm white of the master (${litBays[0].r} red over its rectangle against ${darkBays[0].r})`);

    // ── 3. a locked pass accumulates, a one-off decays ─────────────────────────────────────────────
    // The look dims each new column's own pixel column by FADE and adds the light at ADD over it, so a
    // pass landing on the pass before it converges on ADD / FADE while a one-off tops out at ADD and
    // the next passes of silence carry it away. That is the look's whole claim and the only thing that
    // says a locked loop from a one-off without anything moving.
    //
    // Run at BOTH densities, because the fold is where this can go wrong: on a long loop many raw
    // columns share one device pixel, and a look that dimmed and drew such a pixel once per COLUMN
    // instead of once per PASS would measure how many columns landed there, which repeating a pass
    // does not change. The batch boundaries are moved across the pulse for the same reason: a fold
    // that only holds inside one batch fails exactly where a pulse straddles two.
    const densities = [
      { name: 'short loop: one column over several device px', span: SHORT, cols: SHORT_COLUMNS, pulse: [40, 50], split: [0, 45, SHORT_COLUMNS], passes: 10, silence: 4 },
      { name: 'long loop: several columns to a device px', span: LOOP, cols: LOOP_COLUMNS, pulse: [1200, 1340], split: [0, 700, 1270, 2000, 2700, 3400, LOOP_COLUMNS], passes: 8, silence: 3 },
    ];
    for (const d of densities) {
      const amp = pulseAmp(d.cols, d.pulse[0], d.pulse[1]);
      const whole = cutsEvery(d.cols, CHUNK);
      // The pulse's own stretch of the bay, with a 2 % margin for the width of one column.
      const win = [[1, Math.max(0, d.pulse[0] / d.cols - 0.02), Math.min(1, d.pulse[1] / d.cols + 0.02)]];
      /** One scene's worth: the bay's green with the trace clear, then after `passes` passes. The
       * baseline is read AFTER a primer-only sweep, which parks the beam off the right edge: read
       * before any column the beam stands wherever the loop phase is, which can be inside the window. */
      const run = async (cuts, passes, silentAfter) => {
        await liveLanes(d.span, 1);
        await sweep({ span: d.span, amp, gain: LANE1_ONLY, cuts, passes: 0 });
        const base = (await readBays(win))[0];
        await sweep({ span: d.span, amp, gain: LANE1_ONLY, cuts, passes, silentAfter });
        const after = (await readBays(win))[0];
        return { light: after.g - base.g, base: base.g, raw: after.g, box: after.box };
      };
      const single = await run(whole, 1, 0);
      const locked = await run(whole, d.passes, 0);
      // The stream stops here. The per-column dim only erases where a new column lands, so the trace
      // would otherwise freeze with light on it: the look fades it out over a bounded time and then
      // holds still. Read the bay once the fade is over, and then that nothing moves at all.
      await settle(1200);
      const faded = (await readBays(win))[0];
      const stillPx = await moved(300);
      const oneOff = await run(whole, 1 + d.silence, 1);
      const singleCut = await run(d.split, 1, 0);
      const lockedCut = await run(d.split, d.passes, 0);
      const oneOffCut = await run(d.split, 1 + d.silence, 1);
      const geom = await page.evaluate((span) => ({ ...window.__sc.geom(), cw: window.__sc.geom().cw(span) }), d.span);
      console.log(JSON.stringify({
        accumulation: d.name,
        columnsPerPass: d.cols,
        columnWidthPx: geom.cw,
        tracePx: geom.tw,
        window: single.box,
        passes: d.passes,
        // Green summed over the window, the same scene's still picture subtracted: what the live
        // columns alone put there.
        greenOverTheStillPicture: {
          onePass: single.light,
          passRepeated: locked.light,
          onePassThenSilence: oneOff.light,
          onePassSplitBatch: singleCut.light,
          passRepeatedSplitBatch: lockedCut.light,
          onePassThenSilenceSplitBatch: oneOffCut.light,
        },
        streamStopped: { greenLeft: faded.g - locked.base, movedPx: stillPx },
      }));
      assert.ok(single.light > 20_000, `${d.name}: one pass of columns lights the bay (${single.light} green over the still picture)`);
      assert.ok(locked.light > single.light * 1.8, `${d.name}: the same pass repeated ${d.passes} times is brighter than one (${locked.light} against ${single.light})`);
      assert.ok(oneOff.light < single.light * 0.5, `${d.name}: one pass then ${d.silence} of silence falls below its own single pass (${oneOff.light} against ${single.light})`);
      assert.ok(singleCut.light > 20_000, `${d.name}: one pass split across two batches lights the bay (${singleCut.light})`);
      assert.ok(lockedCut.light > singleCut.light * 1.8, `${d.name}: a pass split across two batches still accumulates (${lockedCut.light} against ${singleCut.light})`);
      assert.ok(oneOffCut.light < singleCut.light * 0.5, `${d.name}: and split, a one-off still decays (${oneOffCut.light} against ${singleCut.light})`);
      assert.ok(Math.abs(lockedCut.light - locked.light) < locked.light * 0.25, `${d.name}: where the batch boundary falls does not change what a locked pass leaves (${lockedCut.light} split against ${locked.light} whole)`);
      assert.ok(faded.g - locked.base < single.light * 0.05, `${d.name}: the stale stream fades out, not freezes mid-light (${faded.g - locked.base} green left of ${locked.light})`);
      assert.ok(stillPx < 20, `${d.name}: and then the drawing holds still (${stillPx} px moved in 300 ms)`);
    }

    // ── 4. a gap does not wipe the trace ───────────────────────────────────────────────────────────
    // An xrun, a ring overrun, a held feed: the engine says it cannot splice this batch onto the one
    // before it. That costs one 4 ms window; a full-screen clear mid-jam would read as a bug. (The one
    // case the look IS allowed to clear on is the sweep's SPAN changing, which means every old
    // column's x now means something else.) The read sits in the same evaluate as the emit, so the
    // stale-stream fade 250 ms later cannot be mistaken for the gap's doing.
    const amp = pulseAmp(SHORT_COLUMNS, 40, 50);
    const win = [[1, 0.28, 0.42]];
    const cuts = cutsEvery(SHORT_COLUMNS, CHUNK);
    await liveLanes(SHORT, 1);
    await sweep({ span: SHORT, amp, gain: LANE1_ONLY, cuts, passes: 0 });
    const gapBase = (await readBays(win))[0].g;
    await sweep({ span: SHORT, amp, gain: LANE1_ONLY, cuts, passes: 6 });
    const gap = await page.evaluate(async (o) => {
      const before = window.__sc.read(o.bays)[0];
      window.__lf.native.emit({ seq: o.seq, reset: false, events: [], scope: window.__sc.batch(o.base, o.amp, o.gain, 0, 4, false, true) });
      await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
      return { before, after: window.__sc.read(o.bays)[0] };
    }, { bays: win, seq: ++seq, base: 9 * SHORT, amp, gain: LANE1_ONLY });
    const held = [gap.before.g - gapBase, gap.after.g - gapBase];
    console.log(JSON.stringify({ gap: { greenOverTheStillPicture: { beforeTheGap: held[0], afterIt: held[1] }, still: gap.after.g } }));
    assert.ok(held[0] > 20_000, `control: the bay holds live light before the gap (${held[0]} green over the still picture)`);
    assert.ok(held[1] > held[0] * 0.8, `a gap costs one 4 ms window, not the trace (${held[1]} green left of ${held[0]})`);
    // Leave the look as the groups after this one expect it: no columns, and the trace faded out.
    await boot(page);
    await settle(900);
  });

  await group('count', async () => {
    await setOpen(true);
    for (const v of VIEWS) {
      await showView(v.id);
      for (const left of [4, 3, 2, 1]) {
        await countIn(left);
        await page.waitForFunction((n) => document.querySelector('.sv-count').textContent === String(n), left);
        const h = await hud();
        assert.equal(h.msg, '', `${v.id}: the message line is empty under the numeral ${left} (${h.msg})`);
        assert.equal(h.dot, (4 - left) % 4, `${v.id}: the beat dot steps with the count`);
      }
      for (const [width, height] of viewports) {
        await page.setViewportSize({ width, height });
        await settle(150);
        const l = await layout();
        const box = await page.evaluate(() => {
          const b = document.querySelector('.sv-count__n').getBoundingClientRect();
          const sv = document.querySelector('.sv').style;
          return { w: b.width, h: b.height, cx: b.x + b.width / 2, cy: b.y + b.height / 2, ax: parseFloat(sv.getPropertyValue('--sv-count-x')), ay: parseFloat(sv.getPropertyValue('--sv-count-y')) };
        });
        console.log(JSON.stringify({ view: v.id, vp: `${width}x${height}`, numeral: box, problems: l.problems }));
        assert.deepEqual(l.problems, [], `${v.id} ${width}x${height}: the numeral fits the window and overlaps nothing`);
        assert.ok(box.h >= 120, `${v.id} ${width}x${height}: the numeral is large (${Math.round(box.h)} px)`);
        assert.ok(Math.abs(box.cx - box.ax) < 2 && Math.abs(box.cy - box.ay) < box.h * 0.08, `${v.id} ${width}x${height}: the numeral sits on the view's anchor`);
      }
      await page.setViewportSize({ width: 1280, height: 820 });
      // The take starts: the count is over.
      await firstTake(0.5);
      await page.waitForFunction(() => document.querySelector('.sv-count').textContent === '');
      await boot(page);
    }
    // An armed later take beside a playing lane waits for the downbeat: a message, never a numeral.
    await laterTake(false, 2 * BAR);
    await page.waitForFunction(() => document.querySelector('.sv-msg').textContent !== '');
    const h = await hud();
    console.log(JSON.stringify({ armedLater: { msg: h.msg, tone: h.tone, count: h.count } }));
    assert.equal(h.msg, '2 · WAITING FOR DOWNBEAT');
    assert.equal(h.tone, 'wait');
    assert.equal(h.count, '', 'an armed later take shows no numeral');
    // A later take armed on stopped loops is counted in by the engine: the numeral and no message, and
    // the loop held at its start (the downbeat restarts it), so once the beat's ripple is gone and the
    // stopped lane's light has fallen, nothing on the canvas moves.
    await boot(page);
    await emit({ events: [transport(LOOP, true, 120), laneEvent(0, stopped(LOOP)), ...empties(1, 2, 3, 4), { Selected: { frame: 0, lane: 1 } }], anchor: anchorAt(3 * BAR), peaks: [wave(0, LOOP)] });
    await emit({ events: [{ Beat: { frame: 3 * BAR, beatInBar: 1, countLeft: 3, clicked: true } }, laneEvent(1, lane('Recording', { armed: true }))] });
    await page.waitForFunction(() => document.querySelector('.sv-count').textContent === '3');
    // The line that was leaving (the wait above) takes 160 ms to empty: wait for it, then it must stay empty.
    await page.waitForFunction(() => document.querySelector('.sv-msg').textContent === '', undefined, { timeout: 2000 })
      .catch(() => assert.fail('a later take counted in from stopped loops shows the numeral, not a wait'));
    await settle(300);
    const counted = await hud();
    assert.equal(counted.msg, '', `no message returns under that numeral (${counted.msg})`);
    await settle(600);
    const rest = await lightAtRest();
    const drift = await moved(250);
    console.log(JSON.stringify({ countedLater: { count: counted.count, msg: counted.msg, movedPx: drift, ...rest } }));
    assert.ok(drift < 20, `under that count the loop is held at its start (${drift} px moved in 250 ms)`);
  });

  await group('cue', async () => {
    await setOpen(true);
    await loops(2, 'Playing');
    await pressSends('3', { SelectTrack: 2 }, 'digit 3 sends SelectTrack');
    await emit({ events: [{ Selected: { frame: 0, lane: 2 } }] });
    await pressSends('Enter', { Action: 'PlayStop' }, 'Enter inside the view sends the engine action');
    // The engine refuses PLAY/STOP on an EMPTY lane and names why (lf-engine actions.rs).
    await emit({ events: [{ Refused: { frame: 0, lane: 2, reason: 'Empty' } }] });
    await page.waitForFunction(() => document.querySelector('.sv-msg').textContent !== '');
    const h = await hud();
    console.log(JSON.stringify({ cue: h.msg, tone: h.tone }));
    assert.equal(h.msg, '3 · nothing to play, record first');
    assert.equal(h.tone, 'cue');
    assert.equal(await page.evaluate(() => getComputedStyle(document.querySelector('.sv-msg')).color), tokens.engaged, 'a refusal reads warm-white');
    await page.waitForFunction(() => document.querySelector('.sv-msg').textContent === '', undefined, { timeout: 4000 });
    // A pending stop speaks in the same line, in its own tone.
    await emit({ events: [laneEvent(0, lane('Playing', { length: LOOP, stopAt: LOOP }))] });
    await page.waitForFunction(() => document.querySelector('.sv-msg').textContent !== '');
    assert.deepEqual([(await hud()).msg, (await hud()).tone], ['1 · STOPPING AT LOOP END', 'stop']);
  });

  await group('bar', async () => {
    await setOpen(true);
    assert.equal((await hud()).bar, null, 'no loop: no bar read-out');
    await loops(2, 'Stopped');
    await settle(150);
    assert.match((await hud()).bar, /^1\s*\/\s*8$/, 'at rest the loop is cued at its start: bar 1');
    assert.equal((await hud()).dot, -1, 'at rest no beat dot is lit');
    await setOpen(false);
    // A loop at 240 BPM (one bar a second) on a clock anchored now, a `Beat` on the feed at each beat as
    // the engine sends them, sampled every 40 ms for a loop and a half: the read-out beside the loop
    // phase read in the same instant. Away from a bar line the reading is the phase's bar; at a bar line
    // it may still show the bar before (it moves on the beat, not per frame).
    const BEAT240 = RATE / 4;
    for (const bars of [2, 4]) {
      const master = bars * 4 * BEAT240;
      await emit({ events: [transport(master, true, 240), laneEvent(0, playing(master)), ...empties(1, 2, 3, 4)], anchor: anchorAt(0), peaks: [wave(0, master)] });
      await setOpen(true);
      const samples = await page.evaluate(async ({ bars, beat, seq0 }) => {
        const seen = [];
        const t0 = performance.now();
        window.__lf.native.emit({ seq: seq0, reset: false, events: [], anchor: { frame: 0, atMs: Date.now(), rate: 48000, grid: 0 } });
        let next = 0;
        let seq = seq0;
        const until = t0 + 300 + (bars + 1.5) * 1000;
        while (performance.now() < until) {
          const elapsed = performance.now() - t0;
          // The engine sends a beat as it renders it; the store shows it when it is heard.
          while (next * 250 <= elapsed) {
            window.__lf.native.emit({ seq: ++seq, reset: false, events: [{ Beat: { frame: next * beat, beatInBar: next % 4, countLeft: 0, clicked: false } }] });
            next++;
          }
          if (elapsed > 300) {
            seen.push({
              text: document.querySelector('.sv-bar').textContent,
              phase: window.__lf.looper.phaseValue(),
              dot: [...document.querySelectorAll('.sv-dot')].findIndex((d) => d.classList.contains('is-on')),
              beat: (next - 1) % 4,
            });
          }
          await new Promise((resolve) => setTimeout(resolve, 40));
        }
        return seen;
      }, { bars, beat: BEAT240, seq0: (seq += 1000) });
      await setOpen(false);
      const readings = samples.map(({ text, phase }) => {
        const m = /^(\d+)\s*\/\s*(\d+)$/.exec(text.trim());
        const at = phase * bars;
        return { bar: m ? Number(m[1]) : null, of: m ? Number(m[2]) : null, want: Math.floor(at) + 1, nearLine: at % 1 < 0.15 || at % 1 > 0.9 };
      });
      const sequence = readings.map((x) => x.bar).filter((b, i, all) => i === 0 || b !== all[i - 1]);
      console.log(JSON.stringify({ scene: `bar-${bars}`, samples: readings.length, sequence }));
      assert.ok(readings.every((x) => x.of === bars), `${bars}-bar loop: every reading is "N / ${bars}" (${[...new Set(samples.map((x) => x.text))].join(', ')})`);
      assert.deepEqual(readings.filter((x) => !x.nearLine && x.bar !== x.want), [], `${bars}-bar loop: away from a bar line the read-out shows the phase's bar`);
      assert.ok(sequence.every((b, i) => i === 0 || b === (sequence[i - 1] % bars) + 1), `${bars}-bar loop: it steps one bar at a time and wraps (${sequence.join(' ')})`);
      assert.ok(sequence.some((b, i) => i > 0 && b === 1 && sequence[i - 1] === bars), `${bars}-bar loop: it crossed the loop boundary ${bars} → 1 (${sequence.join(' ')})`);
      assert.deepEqual([...new Set(sequence)].sort((a, b) => a - b), Array.from({ length: bars }, (_, i) => i + 1), `${bars}-bar loop: every bar shows`);
      assert.ok(samples.filter((x) => x.dot === x.beat).length >= samples.length * 0.9, `${bars}-bar loop: the lit beat dot follows the scripted Beat`);
    }
  });

  await group('controls', async () => {
    await page.mouse.move(300, 300);
    await setOpen(true);
    const hidden = () => page.evaluate(() => {
      const ctl = document.querySelector('.sv-ctl');
      return ctl.classList.contains('is-idle') && getComputedStyle(ctl).pointerEvents === 'none';
    });
    assert.equal(await page.locator('.sv-ctl button').count(), 2, 'exactly two buttons');
    assert.equal(await page.locator('.sv button').count(), 2, 'and no other button in the view');
    assert.deepEqual(await page.locator('.sv-ctl button').evaluateAll((all) => all.map((b) => b.tabIndex)), [-1, -1], 'neither takes the transport keys');
    assert.equal(await hidden(), false, 'the buttons show as the view opens');
    // The hide is waited for, never slept past: a stalled machine may be late, but it must come, and
    // not before 3 s (checked at 1.8 s, with a second to spare for a stall).
    const hides = (what) => page.waitForFunction(() => {
      const ctl = document.querySelector('.sv-ctl');
      return ctl.classList.contains('is-idle') && getComputedStyle(ctl).pointerEvents === 'none';
    }, undefined, { timeout: 6000 }).catch(() => assert.fail(what));
    await settle(1800);
    assert.equal(await hidden(), false, 'still there after 1.8 s');
    await hides('hidden after 3 s without the pointer');
    await page.mouse.move(420, 360);
    assert.equal(await hidden(), false, 'a pointer move brings them back');
    await hides('hidden again');
    await page.keyboard.press('Shift');
    assert.equal(await hidden(), false, 'a key brings them back');
    await hides('hidden a third time');
    await page.keyboard.press('b');
    await expectOpen(false, 'B exits while the buttons are hidden');
  });

  for (const v of VIEWS) {
    await group(`pointer-${v.id}`, async () => {
      await setOpen(true);
      await showView(v.id);
      // Only lane 4 holds a loop: any play-green pixel is its form.
      await emit({ events: [transport(LOOP, true, 120), ...empties(0, 1, 2, 4), laneEvent(3, playing(LOOP))], anchor: anchorAt(BAR), peaks: [wave(3, LOOP, 1)] });
      await settle(400);
      const at = await page.evaluate(async () => {
        await new Promise((r) => requestAnimationFrame(r));
        const c = document.querySelector('.sv-canvas');
        const scale = c.width / c.clientWidth;
        const d = c.getContext('2d').getImageData(0, 0, c.width, c.height).data;
        // The leftmost play-green pixel in the middle band of the canvas's height.
        for (let x = 0; x < c.width; x++) {
          for (let y = Math.round(c.height * 0.3); y < c.height * 0.8; y++) {
            const p = 4 * (y * c.width + x);
            if (d[p + 1] > 80 && d[p + 1] - d[p] > 40 && d[p + 1] - d[p + 2] > 15) return { x: x / scale, y: y / scale };
          }
        }
        return null;
      });
      assert.ok(at, `${v.id}: lane 4's form is on the canvas`);
      await clearSent();
      await page.mouse.click(at.x + 1, at.y);
      assert.deepEqual(await sentAtLeast(1), [{ SelectTrack: 3 }], `${v.id}: a pointer press on lane 4's form sends SelectTrack 3`);
      if (v.id === 'orbit') {
        const centre = await page.evaluate(() => {
          const sv = document.querySelector('.sv').style;
          return { x: parseFloat(sv.getPropertyValue('--sv-count-x')), y: parseFloat(sv.getPropertyValue('--sv-count-y')) };
        });
        await clearSent();
        await page.mouse.click(centre.x, centre.y);
        await settle(150);
        assert.deepEqual(await sent(), [], 'orbit: a press in the centre hole sends nothing');
      }
      console.log(JSON.stringify({ view: v.id, pressedAt: at }));
    });
  }

  // Still on `VIEWS[0]` alone, and that is a measured hole, not a preference: the other two looks'
  // reduced-motion behaviour is checked by nobody, and the CONTROL below (the drawing MUST move while
  // motion is allowed) changes meaning whenever the look order moves. Running it per look, as `pixels`
  // and `budget` do, costs about 8 s and cannot be done on THIS scene: measured 2026-10-09, a beat in
  // a first take's count-in moves scope 2408 px and orbit 1580, and strata 0 — strata draws no beat
  // ripple at rest, so its control would be vacuous and `still < 20` would pass however strata read
  // the preference. Per look needs a scene that moves all three AND still holds still under the
  // preference, which the count-in is the only scene the group has: a playing loop's playhead moves
  // under reduced motion too. That scene is the open question, not the loop over VIEWS.
  await group('reduced', async () => {
    await setOpen(true);
    await showView(VIEWS[0].id);
    const acrossBeat = async () => {
      await countIn(3);
      await settle(700);
      // The beat lands 40 ms into a 320 ms window (a ripple lives 450 ms): a late emit still falls inside.
      const [, n] = await Promise.all([settle(40).then(() => countIn(2)), moved(320)]);
      return n;
    };
    const control = await acrossBeat();
    await boot(page);
    await page.emulateMedia({ reducedMotion: 'reduce' });
    await settle(200);
    const still = await acrossBeat();
    await firstTake(1);
    await settle(150);
    const rec = (await chips())[0];
    console.log(JSON.stringify({ look: VIEWS[0].id, movedAcrossBeat: { control, reduced: still }, recChipAnimation: rec.animation }));
    assert.ok(control > 300, `control: a beat moves the drawing (${control} px)`);
    assert.ok(still < 20, `reduced motion: the drawing holds still across a beat (${still} px)`);
    assert.equal(rec.animation, 'none', 'reduced motion: the recording chip does not pulse');
  });

  for (const v of VIEWS) {
    await group(`legibility-${v.id}`, async () => {
      await setOpen(true);
      await showView(v.id);
      await rich(3 * BAR);
      // The longest standing message: lane 5 armed for the next boundary.
      await emit({ events: [laneEvent(4, lane('Recording', { armed: true }))] });
      for (const [width, height] of legible) {
        await page.setViewportSize({ width, height });
        await page.mouse.move(width / 2, height / 2);
        await settle(200);
        const l = await layout();
        console.log(JSON.stringify({ view: v.id, vp: `${width}x${height}`, ...l }));
        assert.deepEqual(l.problems, [], `${v.id} ${width}x${height}: nothing overlaps or leaves the window`);
        assert.ok(l.capPx >= 14, `${v.id} ${width}x${height}: a chip number's cap height is at least 14 px (${l.capPx})`);
        assert.ok(l.boxes.msg, 'the message line shows the wait');
      }
    });
  }

  for (const v of VIEWS) {
    await group(`budget-${v.id}`, async () => {
      await page.setViewportSize({ width: 1920, height: 1080 });
      await setOpen(true);
      await showView(v.id);
      await loops(5, 'Playing');
      await emit({ meter: { peak: 0.3, clip: false } });
      await page.mouse.move(900, 500);
      await settle(3200); // the two buttons gone
      // The JIT's warm-up: the frame run by hand until its code is optimised (unoptimised code boxes
      // every fractional intermediate, which says nothing about the steady state). A FIXED count
      // measured the tier instead of the code: 6000 iterations optimised strata and scope but not
      // orbit, whose draw is the largest, and an unoptimised orbit reported ~1 kB a frame of boxed
      // doubles against a 32-byte bar, red on CI and on the dev box while the same source read clean
      // (2026-10-09). So run bursts until the hand-run frame stops getting faster, which is what
      // "optimised" looks like from here, and cap the wait. A look that never settles fails the bar
      // below, as it should.
      const warm = await page.evaluate(() => {
        const s = window.__raf;
        s.manual = true;
        const until = performance.now() + 20000;
        // One burst is bounded in BOTH frames and wall time, so a slow look cannot run past the
        // deadline inside a burst the loop only checks between them.
        const burst = () => {
          const t0 = performance.now();
          let k = 0;
          for (; k < 4000 && performance.now() - t0 < 2000; k++) s.stage(performance.now());
          return (performance.now() - t0) / Math.max(1, k);
        };
        // TWO consecutive bursts without a real improvement, not one: a single flat burst is as likely
        // to be timing noise before the tier changes as it is to be the tier having changed.
        let best = Infinity;
        let flat = 0;
        let passes = 0;
        while (passes < 16 && performance.now() < until) {
          const ms = burst();
          passes++;
          if (ms < best * 0.85) flat = 0;
          else flat++;
          if (ms < best) best = ms;
          if (flat >= 2) break;
        }
        s.manual = false;
        return { passes, bestMs: +best.toFixed(4), settled: flat >= 2 };
      });
      await settle(400);
      const cdp = await page.context().newCDPSession(page);
      await cdp.send('HeapProfiler.enable');
      await cdp.send('HeapProfiler.startSampling', { samplingInterval: 64, includeObjectsCollectedByMajorGC: true, includeObjectsCollectedByMinorGC: true });
      const stats = await page.evaluate(async () => {
        const s = window.__raf;
        for (const k of ['stageFrame', 'frame']) Object.assign(s[k], { n: 0, ms: 0, max: 0 });
        s.on = true;
        const until = performance.now() + 20000; // a dead loop fails below instead of hanging here
        while (s.stageFrame.n < 240 && performance.now() < until) await new Promise((r) => setTimeout(r, 50));
        s.on = false;
        const c = document.querySelector('.sv-canvas');
        return { stage: { ...s.stageFrame }, lanes: { ...s.frame }, canvas: [c.width, c.height], hiddenLaneWidth: document.querySelector('.lp-lane canvas').clientWidth };
      });
      const { profile } = await cdp.send('HeapProfiler.stopSampling');
      await cdp.detach();
      // Everything allocated under the stage's frame, by the function the profiler names for it. One
      // boxed number a frame is the floor: the looper's `phaseValue` reads Date.now(), a fresh heap number
      // each time (as it does for the looper lanes' loop), named as `phaseValue` or, once inlined, as its
      // caller in the stage. The probe's own trampoline (its performance.now()) is left out.
      const by = {};
      let bytes = 0;
      const walk = (node, inside) => {
        const here = inside || node.callFrame.functionName === 'stageFrame';
        if (here && node.selfSize > 0) {
          const name = `${node.callFrame.functionName || '(anonymous)'}@${node.callFrame.url.split('/').pop().split('?')[0]}`;
          by[name] = (by[name] ?? 0) + node.selfSize;
          if (node.callFrame.url !== '') bytes += node.selfSize;
        }
        for (const child of node.children) walk(child, here);
      };
      walk(profile.head, false);
      const mean = stats.stage.ms / stats.stage.n;
      console.log(JSON.stringify({ view: v.id, warm, frames: stats.stage.n, meanMs: +mean.toFixed(3), maxMs: +stats.stage.max.toFixed(2), canvas: stats.canvas,
        frameIntervalMs: +((stats.stage.last - stats.stage.first) / (stats.stage.n - 1)).toFixed(2),
        bytesPerFrame: +(bytes / stats.stage.n).toFixed(1), allocatedBy: by,
        hiddenLooperLanes: { meanMs: +(stats.lanes.ms / Math.max(1, stats.lanes.n)).toFixed(3), clientWidth: stats.hiddenLaneWidth } }));
      // Without this the warm-up can run out of bursts or clock having never settled, and the heap bar
      // below would then be read off a frame still climbing down the tiers, which is the exact failure
      // the fixed warm-up had: it went red on code with no allocation site in it.
      assert.ok(warm.settled, `${v.id}: the frame's cost settled before the heap window was sampled (${warm.passes} warm-up bursts, best ${warm.bestMs} ms)`);
      assert.ok(stats.stage.n >= 240, `${v.id}: the stage frame ran 240 times within 20 s (${stats.stage.n})`);
      assert.ok(mean < 6, `${v.id}: mean frame script time under 6 ms (${mean.toFixed(3)} ms)`);
      assert.ok(bytes / stats.stage.n < 32, `${v.id}: the frame allocates no object in the steady state (${(bytes / stats.stage.n).toFixed(1)} bytes a frame, the floor is one boxed number: ${JSON.stringify(by)})`);

      // A take in flight: lane 5 records beside four playing lanes and a peak bin arrives every frame, so
      // its form changes every frame and the cached drawing is rebuilt each time (in playback it never
      // is). The same time bar. The rebuild is not held to the heap bar: that one is steady playback's.
      await emit({
        events: [transport(LOOP, true, 120), ...[0, 1, 2, 3].map((i) => laneEvent(i, playing(LOOP))), laneEvent(4, lane('Recording'), LOOP), beatAt(BAR)],
        anchor: anchorAt(LOOP + BAR),
        peaks: [...[0, 1, 2, 3].map((i) => wave(i, LOOP)), wave(4, 0, 3, Math.floor(BAR / PEAK_FRAMES))],
      });
      await settle(300);
      const rec = await page.evaluate(async ({ start, seq0 }) => {
        const s = window.__raf;
        Object.assign(s.stageFrame, { n: 0, ms: 0, max: 0 });
        s.on = true;
        let bins = start;
        let seq = seq0;
        const until = performance.now() + 20000;
        while (s.stageFrame.n < 240 && performance.now() < until) {
          await new Promise((r) => requestAnimationFrame(r));
          window.__lf.native.emit({ seq: ++seq, reset: false, events: [], peaks: [{ lane: 4, start: bins, count: bins + 1, min: [-0.3 - 0.2 * Math.sin(bins / 3)], max: [0.35 + 0.25 * Math.sin(bins / 5)] }] });
          bins++;
        }
        s.on = false;
        return { n: s.stageFrame.n, ms: s.stageFrame.ms, max: s.stageFrame.max, seq, binsAdded: bins - start };
      }, { start: Math.floor(BAR / PEAK_FRAMES), seq0: seq });
      seq = rec.seq;
      const recMean = rec.ms / Math.max(1, rec.n);
      console.log(JSON.stringify({ view: v.id, recording: { frames: rec.n, meanMs: +recMean.toFixed(3), maxMs: +rec.max.toFixed(2), binsAdded: rec.binsAdded } }));
      assert.ok(rec.n >= 240, `${v.id}: the stage frame ran 240 times while a lane records (${rec.n})`);
      assert.ok(rec.binsAdded >= 200, `${v.id}: the recording lane's peaks changed on those frames (${rec.binsAdded} bins)`);
      assert.ok(recMean < 6, `${v.id}: mean frame script time under 6 ms while a lane records (${recMean.toFixed(3)} ms)`);
      // Closing the stage stops its loop: no frame of it runs afterwards.
      await setOpen(false);
      const after = await page.evaluate(async () => {
        const before = window.__raf.stageFrame.seen;
        await new Promise((r) => setTimeout(r, 400));
        return window.__raf.stageFrame.seen - before;
      });
      assert.equal(after, 0, `${v.id}: no stage frame runs after the view closes (${after})`);
    });
  }

  await group('long-take', async () => {
    // A free take 1.4 loops in is drawn over two loops (its span), so its head sits at about 0.7 of the
    // ring, where its contour ends, not on the loop's playhead (0.4). Along a ray through the head the
    // red runs the band's whole thickness; the contour alone is under half of it there.
    await page.setViewportSize({ width: 1920, height: 1080 });
    await setOpen(true);
    await showView('orbit');
    const at = 1.4 * LOOP;
    await emit({ events: [transport(LOOP, true, 120), laneEvent(0, playing(LOOP)), ...empties(1, 2, 3, 4), { Selected: { frame: 0, lane: 0 } }, beatAt(BAR)], anchor: anchorAt(BAR), peaks: [wave(0, LOOP)] });
    await emit({ events: [laneEvent(1, lane('Recording'), LOOP)], anchor: anchorAt(LOOP + at), peaks: [wave(1, 0, 3, Math.floor(at / PEAK_FRAMES))] });
    await settle(450);
    const red = await page.evaluate(async () => {
      await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
      const sv = document.querySelector('.sv');
      const c = document.querySelector('.sv-canvas');
      const scale = c.width / c.clientWidth;
      const cx = parseFloat(sv.style.getPropertyValue('--sv-count-x')) * scale;
      const cy = parseFloat(sv.style.getPropertyValue('--sv-count-y')) * scale;
      const d = c.getContext('2d').getImageData(0, 0, c.width, c.height).data;
      /** Rec-red pixels along the ray at `turn` of the ring (0 = twelve o'clock, clockwise). */
      const along = (turn) => {
        const dx = Math.sin(2 * Math.PI * turn);
        const dy = -Math.cos(2 * Math.PI * turn);
        let n = 0;
        for (let r = 0; r < c.height / 2; r++) {
          const x = Math.round(cx + dx * r);
          const y = Math.round(cy + dy * r);
          if (x < 0 || y < 0 || x >= c.width || y >= c.height) break;
          const p = 4 * (y * c.width + x);
          if (d[p] > 140 && d[p + 1] < 90 && d[p] - d[p + 1] > 70) n++;
        }
        return n;
      };
      let head = 0;
      let headAt = 0;
      for (let t = 0.69; t <= 0.76; t += 0.001) {
        const n = along(t);
        if (n > head) [head, headAt] = [n, t];
      }
      return { head, headAt: +headAt.toFixed(3), contour: along(0.6), atLoopPhase: along(0.42) };
    });
    console.log(JSON.stringify({ longTake: red }));
    assert.ok(red.head >= 36, `orbit: a take longer than the loop has its head where its contour ends (longest red run ${red.head} px at ${red.headAt} of the ring)`);
    assert.ok(red.contour > 0 && red.contour < red.head, `orbit: the contour reaches past the loop's playhead (${red.contour} px at 0.6 of the ring)`);
    vp = '1920x1080';
    await shoot('long-take');
  });

  await group('keys', async () => {
    // Each key is HELD and the keyboard's own lit cell read (its downNotes) with the notes it sent, first
    // outside the view as the control that the key does play there, then inside, where it must not.
    const hold = async (key, litSelector) => {
      await clearSent();
      await page.keyboard.down(key);
      await settle(250);
      const lit = await page.evaluate((s) => document.querySelector(s) !== null, litSelector);
      await page.keyboard.up(key);
      await settle(150);
      const commands = await sent();
      const played = await notesSent();
      return { lit, notes: played.filter((n) => n.on).length, played, commands };
    };
    const outsideA = await hold('a', '.kb__key--down');
    assert.equal(outsideA.lit, true, 'control: outside the view, A lights a key');
    assert.ok(outsideA.notes > 0, 'control: outside the view, A sends its note');
    await setOpen(true);
    const insideA = await hold('a', '.kb__key--down');
    assert.equal(insideA.lit, false, 'inside the view, A lights no key');
    assert.equal(insideA.notes, 0, `inside the view, A sends no note (${JSON.stringify(insideA.commands)})`);
    await setOpen(false);
    await page.evaluate(() => window.__lf.selectSynth(window.__lf.activeSlot(), 'drum'));
    await page.waitForSelector('.kb__pads');
    const outside3 = await hold('3', '.kb__pad--down');
    assert.equal(outside3.lit, true, 'control: outside the view, drum mode 3 plays the Cowbell pad');
    assert.ok(outside3.notes > 0, 'control: outside the view, drum mode 3 sends its pad note');
    assert.ok(!outside3.commands.some((c) => c.SelectTrack !== undefined), 'control: outside the view, drum mode 3 does not select');
    // V is the Hi Tom pad outside the view and the look's key inside it.
    const stored = () => page.evaluate(() => localStorage.getItem('lf.stageView'));
    const before = await stored();
    const outsideV = await hold('v', '.kb__pad--down');
    assert.equal(outsideV.lit, true, 'outside the view, drum mode V plays the Hi Tom pad');
    assert.ok(outsideV.played.some((n) => n.on && n.note === 50), 'outside the view, V sends the Hi Tom note');
    assert.equal(await stored(), before, 'outside the view, V leaves the look alone');
    // The same action from a pedal (no key involved) does nothing while the view is closed.
    await clearSent();
    await page.evaluate(() => import('/src/app/actions.ts').then((m) => m.runAction('stageNextView')));
    await settle(100);
    assert.equal(await stored(), before, 'outside the view, the stageNextView action leaves the look alone');
    assert.deepEqual(await sent(), [], 'and sends the engine nothing');
    // A pad key released with Shift down (its key reads '!') still releases its pad, and plays again.
    // Named by code: Playwright then shifts the release's key as a keyboard does ('1' would not).
    await clearSent();
    await page.keyboard.down('Digit1');
    await page.keyboard.down('Shift');
    await page.keyboard.up('Digit1');
    await page.keyboard.up('Shift');
    await settle(150);
    const shifted = await notesSent();
    assert.ok(shifted.some((n) => !n.on && n.note === 49), 'a pad key released with Shift held sends its release');
    assert.equal(await page.evaluate(() => document.querySelector('.kb__pad--down') !== null), false, 'a pad key released with Shift held leaves its pad unlit');
    const again1 = await hold('1', '.kb__pad--down');
    assert.ok(again1.played.some((n) => n.on && n.note === 49), 'the next 1 after a shifted release plays the Crash pad');
    await setOpen(true);
    const inside3 = await hold('3', '.kb__pad--down');
    assert.equal(inside3.lit, false, 'inside the view, drum mode 3 plays no pad');
    assert.equal(inside3.notes, 0, 'inside the view, drum mode 3 sends no note');
    assert.deepEqual(inside3.commands.filter((c) => c.SelectTrack !== undefined), [{ SelectTrack: 2 }], 'inside the view, drum mode 3 sends SelectTrack 2');
    const look = await view();
    const insideV = await hold('v', '.kb__pad--down');
    console.log(JSON.stringify({ v: { outside: outsideV.played, inside: insideV.commands, look: [look, await view()] } }));
    assert.equal(insideV.lit, false, 'inside the view, V plays no pad');
    // V inside the view steps the look and is never read as a note key: no `Press`, whatever else it
    // does. The one command it may send is the live scope columns being asked for or let go of, and
    // only where `wantsScope` differs between the look it leaves and the one it steps to (the `views`
    // group pins that mapping look by look).
    const def = (id) => VIEWS.find((d) => d.id === id);
    const steppedTo = def(await view());
    const owedV = def(look).wantsScope === steppedTo.wantsScope ? [] : [{ SetScope: steppedTo.wantsScope }];
    assert.deepEqual(insideV.commands, owedV, `inside the view, V sends the engine nothing but what the columns need (${JSON.stringify(insideV.commands)})`);
    assert.deepEqual(insideV.played, [], 'inside the view, V plays no note');
    assert.notEqual(await view(), look, 'inside the view, V steps the look');
    const picker = await page.evaluate(() => import('/src/app/actions.ts').then((m) => m.ACTION_LABELS.stageNextView));
    assert.equal(picker, 'Stage view: next look', 'the look switch is a named action a pedal can learn');
    await setOpen(false);
  });

  await group('shots', async () => {
    await setOpen(true);
    for (const v of VIEWS) {
      await showView(v.id);
      for (const [width, height] of viewports) {
        vp = `${width}x${height}`;
        await page.setViewportSize({ width, height });
        await page.mouse.move(width / 2, height / 2);
        for (const [name, at] of [['rich-a', 0.6 * BAR], ['rich-b', 2.9 * BAR], ['rich-c', 6.02 * BAR]]) {
          // The settle is inside the scene, before its columns: the shot must catch the live trace
          // while the stream still counts as alive (the look fades a stale one out after 250 ms).
          await rich(at, 450);
          await shoot(name);
        }
        await page.evaluate(() => window.__lf.looper.setMute(1, false));
        await loops(4, 'Stopped');
        await settle(1000);
        await shoot('rest');
        await laterTake(true, 2.6 * BAR);
        await settle(450);
        await shoot('later-take');
        await laterTake(false, 5.3 * BAR);
        await settle(450);
        await shoot('armed');
        await boot(page);
        await countIn(3);
        await settle(250);
        await shoot('count-in');
        await firstTake(3);
        await settle(450);
        await shoot('first-take');
        await boot(page);
        await emit({ events: [laneEvent(0, lane('Recording', { autoArmed: true }))] });
        await settle(300);
        await shoot('listening');
        await boot(page);
      }
      // The app's minimum window: one rich scene per look.
      vp = '960x600';
      await page.setViewportSize({ width: 960, height: 600 });
      await page.mouse.move(480, 300);
      await rich(2.9 * BAR, 450);
      await shoot('rich-b');
      await boot(page);
    }
    // Tiled sheets for the eye: per look, sizes across, scenes down.
    const sheet = await browser.newPage({ viewport: { width: 1900, height: 1000 } });
    for (const v of VIEWS) {
      for (const [part, scenes] of [['a', ['rich-a', 'rich-b', 'rich-c', 'rest']], ['b', ['later-take', 'armed', 'count-in', 'first-take', 'listening']]]) {
        const rows = [];
        for (const scene of scenes) {
          const cells = [];
          for (const [width, height] of viewports) {
            const shot = shots.find((s) => s.view === v.id && s.scene === scene && s.vp === `${width}x${height}`);
            if (shot) cells.push(`<figure><img src="data:image/png;base64,${(await readFile(shot.file)).toString('base64')}"><figcaption>${v.id} · ${scene} · ${shot.vp}</figcaption></figure>`);
          }
          rows.push(`<div class="row">${cells.join('')}</div>`);
        }
        await sheet.setContent(`<style>body{margin:8px;background:#333;color:#ddd;font:12px system-ui}.row{display:flex;gap:8px;align-items:flex-start;margin-bottom:6px}figure{margin:0}img{height:${part === 'a' ? 380 : 320}px;display:block}</style>${rows.join('')}`);
        await sheet.screenshot({ path: `${outDir}/sheet-${v.id}-${part}.png`, fullPage: true });
      }
    }
    await sheet.close();
    console.log(`${shots.length} screenshots and their sheets in ${outDir}`);
  });

  assert.deepEqual(consoleErrors, [], 'no console.error');
  assert.deepEqual(failures, [], `red groups: ${failures.join(', ')}`);
});
