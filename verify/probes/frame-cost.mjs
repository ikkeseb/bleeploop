/**
 * What a frame of the UI costs the page's main thread, the thread a jam shares with everything the
 * WebView does: the 60 fps loops (`src/ui/looper/waveform.ts` `frame`, `src/ui/stage/stage-loop.ts`
 * `stageFrame`) and the feed frame that lands between them. A report, not a gate: it prints the numbers
 * and fails only when a scene did not run as scripted.
 *
 * Each scene gets a fresh page at 1920x1080 on the engine fake (`src/platform/host.web.ts`) and a page-
 * side feeder that sends what the native feed sends (`src-tauri/src/engine_io/feed.rs`): about 60 frames
 * a second, as JSON text, each with a clock anchor and a moving meter, the beats at 120 BPM, and the
 * wire's other fields. Five lanes play an 8-bar loop. A late timer sends every peak bin and column that
 * came due, as the native feed does (columns capped at its `SCOPE_BATCH`, a cut or the first batch `gap`).
 *
 * - looper: the main view.
 * - looper-take: the same with lane 5 as a later take in flight, a peak bin every 1024 frames, so the
 *   lane's cached bitmap is rebuilt on every bin (`rasterise`). The heaviest path the main view has.
 * - scope, orbit, strata: each stage look. SCOPE's frames also carry its live columns, all seven
 *   sources, three to five 4 ms columns a frame, the newest ahead of the clock so they take the beam.
 *
 * A scene warms up on real frames (`--warm=`, default 15 s), then runs four windows of its own, so no
 * instrument measures another's overhead:
 *
 * - timing (`--seconds=`, default 10): a callback's script time, read around it by the page's clock
 *   (5 microseconds here: the document is served cross-origin isolated, the probe's one change to the
 *   page), summed per frame over both loops; the feed's JSON size, and its JSON.parse, decode and
 *   store time. Decode is timed on a second, separate pass of the same frame, so "store" is the mean
 *   of `emit` less the mean of decode, and carries the fake's own small bookkeeping.
 * - trace: the main thread's tasks over 16 ms, how much of each was script, what the rest was, and
 *   the garbage collector's pauses.
 * - heap: bytes allocated per second by the sampling heap profiler, under `stageFrame`, under `frame`,
 *   the feed's parse, its decode, its store and the rest of the page. The feeder's own string building
 *   and the fake's bookkeeping (`sameLaneMix` and its like, which the real host does not run) are the
 *   `rig` figure, not the app's cost. An inlined callee counts as its caller, so the line between
 *   decode and fake is approximate; each scene prints its top sites.
 * - cpu: the sampling profiler's self time by function, which names a cost before anything is changed.
 *
 * Cannot see paint or raster: Chromium here renders in software (`verify/harness/probe.ts`), so a frame's
 * raster and any task it stretches say nothing about the rig's GPU, which is why script time and
 * allocation are the report and a long task is split by its script. It cannot see WebView2 or the rig's
 * CPU under an ASIO callback, the machine's load (it prints the page's frame rate; run it on a quiet
 * machine to compare), and not how a frame reaches the page: a frame under 8192 bytes reaches the
 * WebView as a script to evaluate (Tauri's `Channel::send`), whose cost this `JSON.parse` only stands in
 * for. The columns' shape is scripted, not the engine's.
 *
 * Results land in `logs/frame-cost/result.json`, with a screenshot of each scene, taken after its
 * windows. `--case=a,b` runs those scenes; `--no-feed` keeps the feed still, the control for a GC
 * pause: what stays is not the feed's.
 * Run: pnpm probe frame-cost
 * @no-ci a timing report on a shared machine: its numbers are for reading, and it bars nothing but the scene running
 */
import assert from 'node:assert/strict';
import { mkdir, writeFile } from 'node:fs/promises';
import { arg, probe } from '../harness/probe.ts';

const outDir = 'logs/frame-cost';
const RATE = 48000;
const BIN = 192; // `lf_engine::scope::scope_bin_frames(48_000)`: a 4 ms column
const SOURCES = 7; // `lf_engine::scope::SCOPE_SOURCES`
const PEAK_FRAMES = 1024; // the engine's waveform bin (`lf-engine/src/overview.rs`)
const BAR = 2 * RATE; // one 4/4 bar at 120 BPM
const LOOP = 8 * BAR;
/** How far ahead of the clock the newest column stands (125 ms), inside the SCOPE look's LEAD_COLUMNS. */
const LEAD = 6000;
/** The most scope columns one native frame carries (`SCOPE_BATCH` in `src-tauri/src/engine_io/feed.rs`): past it the oldest go and the batch says `gap`. */
const SCOPE_BATCH = 64;
const WARM_S = Number(arg('warm') ?? 15);
const WINDOW_S = Number(arg('seconds') ?? 10);
const CPU_S = 5;
/** The control: the loops run on a still feed, so a GC pause that stays is not the feed's. */
const NO_FEED = process.argv.includes('--no-feed');
const LONG_MS = 16;
/** A GC pause listed with the heap around it. */
const PAUSE_MS = 2;
const SCENES = [
  { id: 'looper' },
  { id: 'looper-take', take: true },
  { id: 'scope', look: 'scope', columns: true },
  { id: 'orbit', look: 'orbit' },
  { id: 'strata', look: 'strata' },
];
const only = arg('case')?.split(',');

const lane = (state, extra = {}) => ({ state, length: 0, armed: false, autoArmed: false, canUndo: false, canReverse: false, reversed: false, stopAt: null, fading: false, retakePass: 0, ...extra });
const laneEvent = (i, info, frame = 0) => ({ Lane: { frame, lane: i, info } });
const transport = (master, locked) => ({ Transport: { frame: 0, master, bpm: 120, locked } });

/** `bins` waveform bins of lane `i`: a busy part with accents, a different one per lane. */
function wave(i, bins) {
  let seed = 7 + 31 * i;
  const rnd = () => (seed = (seed * 16807) % 2147483647) / 2147483647;
  const max = [];
  const min = [];
  for (let b = 0; b < bins; b++) {
    const a = Math.min(1, (0.2 + 0.6 * rnd() ** 3) * (b % 12 === 0 ? 1.5 : 1));
    max.push(+a.toFixed(3));
    min.push(+(-a * (0.7 + 0.3 * rnd())).toFixed(3));
  }
  return { lane: i, start: 0, count: bins, min, max };
}

/** The page's rAF seam: each loop's callback timed into typed arrays, one index per frame. */
function timingRig() {
  window.__lfEngineFake = true;
  const CAP = 1 << 14;
  const t = (window.__cost = { on: false, n: 0, at: new Float64Array(CAP), lanes: new Float64Array(CAP), stage: new Float64Array(CAP), last: new Float64Array(1) });
  const real = window.requestAnimationFrame.bind(window);
  const trampolines = new Map();
  window.requestAnimationFrame = (cb) => {
    const slot = cb.name === 'frame' ? t.lanes : cb.name === 'stageFrame' ? t.stage : null;
    if (!slot) return real(cb);
    let tr = trampolines.get(cb);
    if (!tr) {
      tr = (now) => {
        if (!t.on) return cb(now);
        if (now !== t.last[0]) {
          t.last[0] = now;
          if (t.n + 1 < CAP) t.n++;
          t.at[t.n] = now;
        }
        const t0 = performance.now();
        cb(now);
        slot[t.n] = performance.now() - t0;
      };
      trampolines.set(cb, tr);
    }
    return real(tr);
  };
}

/**
 * The page side of the feed. Each tick builds the frame's JSON from a pool of made batches, so the
 * feeder itself costs little, then hands it on as the host does: parse, then the fake's `emit`, which
 * is the real decoder and the real store. `parseFrame` carries a name for the heap profile.
 */
async function feedRig(o) {
  const { decodeFeedFrame } = await import('/src/platform/engine-wire.ts');
  const { native } = window.__lf;
  const TICK = 1000 / 60;
  let seed = 4242;
  const rnd = () => (seed = (seed * 16807) % 2147483647) / 2147483647;
  /** A batch's two arrays of arrays, as the JSON tail of a scope object, rounded as the feed rounds. */
  const batch = (n) => {
    const side = (sign) => Array.from({ length: o.sources }, (_, s) => Array.from({ length: n }, () => +(sign * (0.1 + 0.08 * s + 0.4 * rnd())).toFixed(3)));
    return `"min":${JSON.stringify(side(-1))},"max":${JSON.stringify(side(1))}}`;
  };
  const tails = [];
  for (let n = 0; n <= o.scopeBatch; n++) tails.push(Array.from({ length: 16 }, () => batch(n)));
  const parseFrame = (text) => JSON.parse(text);
  const CAP = 1 << 13;
  const f = (window.__feed = {
    run: false, split: false, stat: false, n: 0, ticks: 0, seq: o.seq, batches: 0,
    ages: new Float64Array(1 << 10), agesIn: new Uint8Array(1 << 10), nAges: 0, sampleAges: false, ageWindow: 0,
    bytes: new Float64Array(CAP), parse: new Float64Array(CAP), decode: new Float64Array(CAP), emit: new Float64Array(CAP),
  });
  const t0Wall = Date.now();
  let nextColumn = -1;
  let firstBatch = true;
  let beat = -1;
  let bins = o.takeBins;
  let k = 0;
  function feedTick() {
    k++;
    const wall = Date.now();
    const frame = Math.round(o.at + ((wall - t0Wall) * o.rate) / 1000);
    const beatNow = Math.floor(frame / (o.rate / 2));
    let events = '';
    if (beat >= 0 && beatNow !== beat) events = `{"Beat":{"frame":${beatNow * (o.rate / 2)},"beatInBar":${beatNow % 4},"countLeft":0,"clicked":false}}`;
    beat = beatNow;
    let peaks = '';
    if (o.take) {
      // As the native feed: the count rounds up, and one update holds the bins that changed since the last
      // frame, the unfinished last bin among them while it fills.
      const total = Math.ceil((frame - o.at + o.takeFrames) / o.peakFrames);
      if (total > 0) {
        const start = Math.max(0, Math.min(bins, total) - 1);
        const lo = [];
        const hi = [];
        for (let b = start; b < total; b++) {
          const a = 0.3 + 0.2 * Math.sin(b / 3);
          lo.push((-a).toFixed(3));
          hi.push(a.toFixed(3));
        }
        peaks = `{"lane":4,"start":${start},"count":${total},"min":[${lo}],"max":[${hi}]}`;
        bins = total;
      }
    }
    let scope = '';
    if (o.columns) {
      const to = Math.floor((frame + o.lead) / o.bin);
      if (nextColumn < 0) nextColumn = to - 4;
      // As the native feed: the newest `scopeBatch` columns go, and a cut or the first batch says `gap`.
      const due = to - nextColumn;
      const n = Math.min(due, o.scopeBatch);
      if (n > 0) {
        scope = `,"scope":{"frame":${(to - n) * o.bin},"bin":${o.bin},"gap":${firstBatch || due > n},${tails[n][k & 15]}`;
        firstBatch = false;
        nextColumn = to;
        f.batches++;
      }
    }
    const level = (0.2 + 0.3 * Math.abs(Math.sin(k / 7))).toFixed(7);
    const text = `{"seq":${++f.seq},"reset":false,"events":[${events}],"device":[],"anchor":{"frame":${frame},"atMs":${wall},"rate":${o.rate},"grid":0},"meter":{"peak":[${level},0.0],"clip":[false,false]},"peaks":[${peaks}]${scope}}`;
    const t0 = performance.now();
    const raw = parseFrame(text);
    const t1 = performance.now();
    if (f.split) decodeFeedFrame(raw);
    const t2 = performance.now();
    native.emit(raw);
    const t3 = performance.now();
    f.ticks++;
    if (f.stat && f.n < CAP) {
      f.bytes[f.n] = text.length;
      f.parse[f.n] = t1 - t0;
      f.decode[f.n] = t2 - t1;
      f.emit[f.n] = t3 - t2;
      f.n++;
    }
  }
  if (o.columns) {
    // Freshness through the windows: the newest column's lead over the frame heard now, read between ticks.
    const { looper } = await import('/src/ui/state/audio.ts');
    const v = { lo: null, hi: null, at: 0, count: 0, frame: 0, bin: 0, epoch: 0 };
    setInterval(() => {
      if (!f.sampleAges || f.nAges >= f.ages.length) return;
      looper.scopeInto(v);
      f.agesIn[f.nAges] = f.ageWindow;
      f.ages[f.nAges++] = v.frame - looper.heardFrameValue();
    }, 250);
  }
  let done = 0;
  let start = 0;
  f.start = () => {
    f.run = true;
    start = performance.now();
    done = 0;
    const pump = () => {
      if (!f.run) return;
      const due = Math.floor((performance.now() - start) / TICK);
      // A late timer sends what was due, up to a held tick's worth, as the feed thread catches up.
      if (due - done > 3) done = due - 3;
      while (done < due) {
        feedTick();
        done++;
      }
      setTimeout(pump, Math.max(0, start + (done + 1) * TICK - performance.now()));
    };
    pump();
  };
}

const sorted = (values) => Float64Array.from(values).sort();
const pct = (s, p) => (s.length === 0 ? 0 : s[Math.min(s.length - 1, Math.floor((p / 100) * s.length))]);
const mean = (v) => (v.length === 0 ? 0 : v.reduce((a, b) => a + b, 0) / v.length);
const ms = (v) => +v.toFixed(3);
const spread = (values) => {
  const s = sorted(values);
  return { median: ms(pct(s, 50)), p95: ms(pct(s, 95)), p99: ms(pct(s, 99)), worst: ms(s.length ? s[s.length - 1] : 0), mean: ms(mean(s)) };
};

/** One timing window: script per frame, and the feed's size and cost. */
async function timed(page) {
  return page.evaluate(async (seconds) => {
    const t = window.__cost;
    const f = window.__feed;
    t.n = 0;
    f.n = 0;
    t.lanes.fill(0);
    t.stage.fill(0);
    f.split = f.stat = true;
    t.on = true;
    await new Promise((r) => setTimeout(r, seconds * 1000));
    t.on = false;
    f.split = f.stat = false;
    const n = t.n;
    const cut = (a, count) => Array.from(a.subarray(1, count + 1));
    const raw = (a, count) => Array.from(a.subarray(0, count));
    return { frames: n, spanMs: t.at[n] - t.at[1], at: cut(t.at, n), lanes: cut(t.lanes, n), stage: cut(t.stage, n), feed: { n: f.n, bytes: raw(f.bytes, f.n), parse: raw(f.parse, f.n), decode: raw(f.decode, f.n), emit: raw(f.emit, f.n) }, isolated: self.crossOriginIsolated };
  }, WINDOW_S);
}

/** One trace window: the main thread's tasks and the collector's pauses. */
async function traced(browser, page) {
  await browser.startTracing(page, { categories: ['devtools.timeline', 'disabled-by-default-devtools.timeline', 'v8', 'disabled-by-default-v8.gc'] });
  await page.waitForTimeout(WINDOW_S * 1000);
  const { traceEvents } = JSON.parse((await browser.stopTracing()).toString());
  const counts = new Map();
  for (const e of traceEvents) if (e.ph === 'X' && e.name === 'RunTask') counts.set(`${e.pid}:${e.tid}`, (counts.get(`${e.pid}:${e.tid}`) ?? 0) + 1);
  const names = new Map(traceEvents.filter((e) => e.ph === 'M' && e.name === 'thread_name').map((e) => [`${e.pid}:${e.tid}`, e.args.name]));
  const key = [...counts.keys()].filter((k) => names.get(k) === 'CrRendererMain').sort((a, b) => counts.get(b) - counts.get(a))[0];
  const main = traceEvents.filter((e) => e.ph === 'X' && `${e.pid}:${e.tid}` === key).sort((a, b) => a.ts - b.ts);
  const tasks = main.filter((e) => e.name === 'RunTask' && e.dur !== undefined);
  const SCRIPT = new Set(['FireAnimationFrame', 'TimerFire', 'FunctionCall', 'EvaluateScript', 'v8.callFunction']);
  /** Outermost script events by start, so a nested call counts once. */
  const scripts = [];
  let end = 0;
  for (const e of main) {
    if (SCRIPT.has(e.name) && e.ts >= end) {
      scripts.push(e);
      end = e.ts + e.dur;
    }
  }
  const long = [];
  for (const task of tasks) {
    if (task.dur / 1000 <= LONG_MS) continue;
    const inside = main.filter((e) => e !== task && e.ts >= task.ts && e.ts + e.dur <= task.ts + task.dur);
    const script = scripts.filter((e) => e.ts >= task.ts && e.ts + e.dur <= task.ts + task.dur).reduce((a, e) => a + e.dur, 0);
    const by = {};
    for (const e of inside) by[e.name] = (by[e.name] ?? 0) + e.dur;
    const top = Object.entries(by).sort((a, b) => b[1] - a[1]).slice(0, 4).map(([n, d]) => `${n} ${(d / 1000).toFixed(1)}`);
    long.push({ ms: ms(task.dur / 1000), script: ms(script / 1000), top });
  }
  // Pauses on the main thread: the outer events, not the phases nested in them.
  const gcs = main.filter((e) => e.name === 'MinorGC' || e.name === 'MajorGC' || e.name === 'V8.GC_MC_INCREMENTAL' || e.name === 'V8.GCFinalizeMC' || e.name === 'V8.GCScavenger');
  const pauseNames = gcs.some((e) => e.name === 'MinorGC' || e.name === 'MajorGC') ? ['MinorGC', 'MajorGC'] : ['V8.GCScavenger', 'V8.GCFinalizeMC', 'V8.GC_MC_INCREMENTAL'];
  const gc = {};
  for (const name of pauseNames) {
    const d = gcs.filter((e) => e.name === name).map((e) => e.dur / 1000);
    gc[name] = { n: d.length, worstMs: ms(d.length ? Math.max(...d) : 0), totalMs: ms(d.reduce((a, b) => a + b, 0)) };
  }
  const MB = 1024 * 1024;
  const first = tasks[0]?.ts ?? 0;
  const pauses = gcs.filter((e) => pauseNames.includes(e.name) && e.dur / 1000 > PAUSE_MS).map((e) => ({
    name: e.name, atS: +((e.ts - first) / 1e6).toFixed(1), ms: ms(e.dur / 1000), mbBefore: +((e.args?.usedHeapSizeBefore ?? 0) / MB).toFixed(1), mbAfter: +((e.args?.usedHeapSizeAfter ?? 0) / MB).toFixed(1), type: e.args?.type,
  }));
  const sum = (names) => main.filter((e) => names.includes(e.name)).reduce((a, e) => a + e.dur, 0) / 1000;
  const busy = tasks.reduce((a, e) => a + e.dur, 0) / 1000;
  return {
    tasks: tasks.length, busyMs: ms(busy), long,
    longScript: long.filter((l) => l.script > LONG_MS).length, gc, pauses,
    splitMs: { script: ms(scripts.reduce((a, e) => a + e.dur, 0) / 1000), style: ms(sum(['UpdateLayoutTree'])), layout: ms(sum(['Layout'])), prePaint: ms(sum(['PrePaint'])), paint: ms(sum(['Paint', 'Layerize', 'Commit'])) },
  };
}

/** One heap window: bytes allocated per second, by whose call tree. */
async function heaped(page) {
  const cdp = await page.context().newCDPSession(page);
  await cdp.send('HeapProfiler.enable');
  await cdp.send('HeapProfiler.startSampling', { samplingInterval: 256, includeObjectsCollectedByMajorGC: true, includeObjectsCollectedByMinorGC: true });
  await page.waitForTimeout(WINDOW_S * 1000);
  const { profile } = await cdp.send('HeapProfiler.stopSampling');
  await cdp.detach();
  const bucket = { stage: 0, lanes: 0, parse: 0, decode: 0, store: 0, fake: 0, rig: 0, other: 0 };
  const sites = {};
  const ROOT = { stageFrame: 'stage', frame: 'lanes', parseFrame: 'parse', emit: 'fake', feedTick: 'rig' };
  /** Under the fake's `emit`, whose module owns a site: the real decoder, or the real store. */
  const owner = (from, url) => (from !== 'fake' || url === '' ? from : url.includes('host.web') ? 'fake' : url.includes('engine-wire') ? 'decode' : 'store');
  const walk = (node, from) => {
    const b = owner(from === null || from === 'rig' ? (ROOT[node.callFrame.functionName] ?? from) : from, node.callFrame.url);
    if (node.selfSize > 0) {
      bucket[b ?? 'other'] += node.selfSize;
      const name = `${b ?? 'other'} ${node.callFrame.functionName || '(anonymous)'}@${node.callFrame.url.split('/').pop().split('?')[0]}:${node.callFrame.lineNumber + 1}`;
      sites[name] = (sites[name] ?? 0) + node.selfSize;
    }
    for (const child of node.children) walk(child, b);
  };
  walk(profile.head, null);
  const perSecond = Object.fromEntries(Object.entries(bucket).map(([k, v]) => [k, Math.round(v / WINDOW_S)]));
  const top = Object.entries(sites).sort((a, b) => b[1] - a[1]).slice(0, 10).map(([n, v]) => `${Math.round(v / WINDOW_S)} B/s ${n}`);
  return { perSecond, top };
}

/** One CPU window: the sampling profiler's self time, as a share of the window. */
async function profiled(page) {
  const cdp = await page.context().newCDPSession(page);
  await cdp.send('Profiler.enable');
  await cdp.send('Profiler.setSamplingInterval', { interval: 200 });
  await cdp.send('Profiler.start');
  await page.waitForTimeout(CPU_S * 1000);
  const { profile } = await cdp.send('Profiler.stop');
  await cdp.detach();
  const byId = new Map(profile.nodes.map((n) => [n.id, n]));
  const self = {};
  for (let i = 0; i < profile.samples.length; i++) {
    const c = byId.get(profile.samples[i]).callFrame;
    const name = `${c.functionName || '(anonymous)'}@${c.url.split('/').pop().split('?')[0]}:${c.lineNumber + 1}`;
    self[name] = (self[name] ?? 0) + (profile.timeDeltas[i] ?? 0);
  }
  return Object.entries(self).sort((a, b) => b[1] - a[1]).slice(0, 14).map(([n, us]) => `${(us / 1000 / CPU_S).toFixed(2)} ms/s ${n}`);
}

async function scene(browser, open, base, s) {
  const init = async (p) => {
    // Cross-origin isolated: the page's clock reads to 5 microseconds rather than 100. Chromium counts a
    // document the route fulfils as public and blocks Vite's local socket, so the page logs that once
    // and no HMR runs, which `pnpm probe` turns off anyway.
    await p.route((u) => u.href === `${base}/` || u.href === base, async (route) => {
      const response = await route.fetch();
      await route.fulfill({ response, headers: { ...response.headers(), 'cross-origin-opener-policy': 'same-origin', 'cross-origin-embedder-policy': 'require-corp' } });
    });
    await p.addInitScript(timingRig);
  };
  const app = await open({ viewport: { width: 1920, height: 1080 }, init });
  const { page } = app;
  await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });
  let seq = 0;
  const emit = (frame) => page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], ...frame });
  const anchor = (frame) => ({ frame, atMs: Date.now(), rate: RATE, grid: 0 });
  await emit({ reset: true, settings: [], events: [...[0, 1, 2, 3, 4].map((i) => laneEvent(i, lane('Empty'))), transport(0, false), { Selected: { frame: 0, lane: 0 } }], anchor: anchor(0), meter: { peak: [0, 0], clip: [false, false] } });
  const bins = Math.ceil(LOOP / PEAK_FRAMES);
  const takeBins = s.take ? Math.floor(BAR / PEAK_FRAMES) : 0;
  const count = s.take ? 4 : 5;
  const at = s.take ? LOOP + BAR : RATE;
  await emit({
    events: [
      transport(LOOP, true),
      ...[0, 1, 2, 3].map((i) => laneEvent(i, lane('Playing', { length: LOOP, canReverse: true }))),
      s.take ? laneEvent(4, lane('Recording'), LOOP) : laneEvent(4, lane('Playing', { length: LOOP, canReverse: true })),
    ],
    anchor: anchor(at),
    peaks: [...Array.from({ length: count }, (_, i) => wave(i, bins)), ...(s.take ? [{ ...wave(4, takeBins), count: takeBins }] : [])],
  });
  if (s.look) {
    await page.keyboard.press('b');
    await page.waitForFunction(() => !!document.querySelector('.sv'));
    for (let k = 0; k < 3 && (await page.evaluate(() => document.querySelector('.sv').dataset.view)) !== s.look; k++) await page.keyboard.press('v');
    assert.equal(await page.evaluate(() => document.querySelector('.sv').dataset.view), s.look, `the stage shows ${s.look}`);
  }
  await page.mouse.move(900, 500);
  await page.evaluate(feedRig, { seq, rate: RATE, at, bin: BIN, sources: SOURCES, lead: LEAD, columns: !!s.columns, take: !!s.take, takeBins, takeFrames: s.take ? BAR : 0, peakFrames: PEAK_FRAMES, scopeBatch: SCOPE_BATCH });
  if (!NO_FEED) await page.evaluate(() => window.__feed.start());
  await page.waitForTimeout(WARM_S * 1000);

  const ticks = () => page.evaluate(() => window.__feed.ticks);
  const nextWindow = () => page.evaluate(() => void window.__feed.ageWindow++);
  await page.evaluate(() => void (window.__feed.sampleAges = true));
  const ticks0 = await ticks();
  const time = await timed(page);
  const ticks1 = await ticks();
  await nextWindow();
  const trace = await traced(browser, page);
  await nextWindow();
  const heap = await heaped(page);
  await nextWindow();
  const cpu = await profiled(page);
  await page.evaluate(() => void (window.__feed.sampleAges = false));
  await page.screenshot({ path: `${outDir}/${s.id}.png` });
  const after = await page.evaluate(() => {
    return { batches: window.__feed.batches, ages: Array.from(window.__feed.ages.subarray(0, window.__feed.nAges)), agesIn: Array.from(window.__feed.agesIn.subarray(0, window.__feed.nAges)), sent: window.__lf.native.sent.filter((c) => c.SetScope !== undefined) };
  });
  await page.evaluate(() => void (window.__feed.run = false));
  app.consumeConsoleErrors(/WebSocket connection to|failed to connect to websocket/);
  await page.close();

  const ages = sorted(after.ages);
  const windowAges = [0, 1, 2, 3].map((w) => sorted(after.ages.filter((_, i) => after.agesIn[i] === w)));
  const seconds = time.spanMs / 1000;
  const total = time.lanes.map((v, i) => v + time.stage[i]);
  const gaps = time.at.slice(1).map((v, i) => v - time.at[i]);
  const f = time.feed;
  const feed = {
    frames: f.n,
    perSecond: +(f.n / WINDOW_S).toFixed(1),
    jsonBytes: { median: pct(sorted(f.bytes), 50), worst: f.n ? Math.max(...f.bytes) : 0 },
    parseUs: { mean: Math.round(mean(f.parse) * 1000), p99: Math.round(pct(sorted(f.parse), 99) * 1000) },
    decodeUs: { mean: Math.round(mean(f.decode) * 1000), p99: Math.round(pct(sorted(f.decode), 99) * 1000) },
    emitUs: { mean: Math.round(mean(f.emit) * 1000), p99: Math.round(pct(sorted(f.emit), 99) * 1000), worst: f.n ? Math.round(Math.max(...f.emit) * 1000) : 0 },
    storeUsMean: Math.round((mean(f.emit) - mean(f.decode)) * 1000),
  };
  const out = {
    scene: s.id, isolated: time.isolated, frames: time.frames, fps: +(time.frames / seconds).toFixed(1), intervalP99: ms(pct(sorted(gaps), 99)),
    script: spread(total), lanes: spread(time.lanes), stage: spread(time.stage), feed, trace, heap, cpu,
    // The batches sent over the whole scene, warm-up included, and how far ahead of the clock the newest
    // column stood (frames), sampled every 250 ms through the four windows, with each window's median.
    scope: s.columns && !NO_FEED ? { batches: after.batches, ageFrames: { n: ages.length, min: ages[0] ?? 0, median: pct(ages, 50), max: ages.at(-1) ?? 0, windows: windowAges.map((v) => pct(v, 50)) } } : null,
  };
  console.log(JSON.stringify(out));
  // Rates are reported (`fps`, `feed.perSecond`), never asserted: a correct scene can run slow on a loaded
  // machine. What is checked is that the scene ran as scripted.
  assert.ok(time.isolated, 'the page is cross-origin isolated, so its clock reads fine enough');
  assert.ok(time.frames >= 2, `${s.id}: the loops ran (${time.frames} frames in ${WINDOW_S} s)`);
  assert.ok(NO_FEED || ticks1 - ticks0 > 0, `${s.id}: the feed ran (${ticks1 - ticks0} ticks in ${WINDOW_S} s)`);
  if (s.columns && !NO_FEED) {
    assert.deepEqual(after.sent.at(-1), { SetScope: true }, 'SCOPE asked for its columns');
    assert.ok(after.batches > 0, `columns flowed (${after.batches} batches)`);
    // Each window's median, so a stall under load does not fail a correct run; a negative or far-ahead one
    // means the look's freshness path (`age > 0 && age <= LEAD_COLUMNS * bin`) was not the one exercised.
    const fresh = windowAges.every((v) => v.length > 0 && pct(v, 50) > 0 && pct(v, 50) <= 50 * BIN);
    assert.ok(fresh, `the newest column is fresh by the clock in every window (medians ${out.scope.ageFrames.windows} frames ahead, ${ages.length} samples)`);
  }
  return out;
}

await probe(async ({ browser, open, url }) => {
  await mkdir(outDir, { recursive: true });
  const base = url.replace(/\/$/, '');
  const rows = [];
  for (const s of SCENES) if (!only || only.includes(s.id)) rows.push(await scene(browser, open, base, s));

  console.log('\nScript and allocation only: this Chromium renders in software, so paint and raster mean nothing here. Not a frame on the rig.');
  console.log('\nscene       | fps  | script ms med / p95 / p99 / worst | tasks>16ms (script>16) | GC n, worst, total ms       | alloc KB/s draw / feed / other (rig)');
  for (const r of rows) {
    const gc = Object.entries(r.trace.gc).map(([k, v]) => `${k.replace('GC', '')} ${v.n}, ${v.worstMs}, ${v.totalMs}`).join('; ');
    const a = r.heap.perSecond;
    const kb = (v) => (v / 1024).toFixed(1);
    console.log(`${r.scene.padEnd(11)} | ${String(r.fps).padEnd(4)} | ${`${r.script.median} / ${r.script.p95} / ${r.script.p99} / ${r.script.worst}`.padEnd(33)} | ${`${r.trace.long.length} (${r.trace.longScript})`.padEnd(22)} | ${gc.padEnd(27)} | ${kb(a.stage + a.lanes)} / ${kb(a.parse + a.decode + a.store)} / ${kb(a.other)} (${kb(a.rig + a.fake)})`);
  }
  console.log('\nscene       | feed frames/s | JSON B med / worst | parse / decode / store mean us | emit p99 / worst us');
  for (const r of rows) {
    const f = r.feed;
    console.log(`${r.scene.padEnd(11)} | ${String(f.perSecond).padEnd(13)} | ${`${f.jsonBytes.median} / ${f.jsonBytes.worst}`.padEnd(18)} | ${`${f.parseUs.mean} / ${f.decodeUs.mean} / ${f.storeUsMean}`.padEnd(30)} | ${f.emitUs.p99} / ${f.emitUs.worst}`);
  }
  await writeFile(`${outDir}/result.json`, `${JSON.stringify(rows, null, 2)}\n`);
});
