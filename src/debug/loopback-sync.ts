/**
 * DEV probe: where does a take land against the click, measured through a physical loopback cable
 * (an interface output wired into input 1)? The cable plays the click back into the native input
 * exactly as it leaves the interface, so it is a player who hits every click as it is heard.
 *
 *   take A  lane 0, FIXED first take of BARS bars, native monitor gain 0 (no feedback). Every click
 *           in the committed loop is measured against its beat: the offset and its slope over the
 *           take (drift inside a take).
 *   take B  lane 1, a later take over the same master, lane 0 muted. A short pulse is scheduled on
 *           each half beat, and the native monitor gain is raised just enough for the pulse to come
 *           round the cable once more through input → plugin → native monitor → output. The pulse's
 *           direct arrival gives the placement X; the echo spacing gives RT, the native round trip a
 *           guitarist hears.
 *
 * A player who puts the native wet on the heard click plays RT earlier than the cable does, so a
 * correct compensation C leaves the cable's pulse RT late: residual = X − RT is how far a perfectly
 * timed hit lands from the grid (positive = late). Needs one effect plugin with an audio input
 * (`VITE_LF_PROBE_PLUGIN`, name substring, default "Pro-Q", set flat) and runs in its own app profile
 * (the runner's config overlay), so it never touches the owner's jam, recovery or settings.
 * `[loopback]` lines go through `console.error` (→ the same log as the Rust host).
 *
 * Trigger: `VITE_LF_PROBE=loopback-sync` at Vite start (DEV only). Knobs:
 *   `VITE_LF_PROBE_PLUGIN`   plugin name substring (default `Pro-Q`)
 *   `VITE_LF_PROBE_CHANNEL`  0-based input channel of the cable (default `0` = input 1)
 *   `VITE_LF_PROBE_BARS`     take A length in bars at 120 BPM (default 16)
 *   `VITE_LF_PROBE_TRIM`     rec align (ms) to apply in the probe's profile (default 0)
 */
import { engine } from '../audio/engine';
import { clock } from '../audio/clock';
import { looper } from '../audio/looper/looper';
import { engineState } from '../audio/looper/state';
import { availablePlugins, selectPlugin } from '../audio/instrument';
import { nativeHostReady, slotPlugins } from '../audio/instrument-slots';
import { goLive } from '../audio/native-io';
import { pluginBridge } from '../audio/plugin-bridge';
import { recordLatency } from '../audio/record-latency';
import { platform } from '../platform';

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
const log = (...args: unknown[]) => console.error('[loopback]', ...args);
const ms = (frames: number, sr: number) => +((frames / sr) * 1000).toFixed(2);
const median = (xs: number[]) => {
  const s = [...xs].sort((a, b) => a - b);
  return s.length ? (s[(s.length - 1) >> 1] + s[s.length >> 1]) / 2 : NaN;
};

async function until(label: string, predicate: () => boolean, seconds: number): Promise<void> {
  for (let i = 0; i < seconds * 10; i++) {
    if (predicate()) return;
    await sleep(100);
  }
  throw Error(`timed out waiting for ${label}`);
}

/** Committed loop of lane `i` (frame 0 = the master downbeat). */
function committed(i: number): Float32Array {
  const t = engineState.tracks[i];
  return t.record.slice(0, t.lengthFrames);
}

/** First index in [from, to) where |x| crosses `level`; -1 if none. */
function crossing(x: Float32Array, from: number, to: number, level: number): number {
  for (let i = Math.max(0, Math.round(from)); i < Math.min(x.length, to); i++) if (Math.abs(x[i]) >= level) return i;
  return -1;
}

function peakIn(x: Float32Array, from: number, to: number): { at: number; value: number } {
  let at = -1;
  let value = 0;
  for (let i = Math.max(0, Math.round(from)); i < Math.min(x.length, to); i++) {
    if (Math.abs(x[i]) > value) {
      value = Math.abs(x[i]);
      at = i;
    }
  }
  return { at, value };
}

/** Least-squares slope of y over x. */
function slope(xs: number[], ys: number[]): number {
  const n = xs.length;
  const mx = xs.reduce((a, b) => a + b, 0) / n;
  const my = ys.reduce((a, b) => a + b, 0) / n;
  let num = 0;
  let den = 0;
  for (let i = 0; i < n; i++) {
    num += (xs[i] - mx) * (ys[i] - my);
    den += (xs[i] - mx) ** 2;
  }
  return den ? num / den : NaN;
}

export async function runLoopbackSync(): Promise<void> {
  try {
    await run();
  } catch (e) {
    log(`FAIL ${String(e instanceof Error ? e.message : e)}`);
  }
}

async function run(): Promise<void> {
  const want = String(import.meta.env.VITE_LF_PROBE_PLUGIN ?? 'Pro-Q').toLowerCase();
  const channel = Number(import.meta.env.VITE_LF_PROBE_CHANNEL ?? 0);
  const bars = Number(import.meta.env.VITE_LF_PROBE_BARS ?? 16) || 16;
  const bpm = 120;

  await until('the plugin scan', () => nativeHostReady() && availablePlugins().length > 0, 600);
  const desc = availablePlugins().find((d) => d.name.toLowerCase().includes(want));
  if (!desc) throw Error(`no scanned plugin matches "${want}"`);
  await engine.start();
  await looper.init();
  const ctx = engine.ctx;
  const sr = ctx.sampleRate;
  // The profile is the probe's own (the runner's identifier overlay): a jam here is an earlier run's.
  looper.clearAll();
  await selectPlugin(0, desc);
  if (slotPlugins()[0]?.id !== desc.id) throw Error(`could not load ${desc.name}`);
  await sleep(1500);
  await goLive(0, null, channel, null);
  await platform.pluginHost.setMonitorGain(0, 0);
  log(`live: ${desc.name} [${desc.format}] input ${channel + 1}, sr=${sr}; settling`);
  await sleep(4500);

  recordLatency.setOffsetMs(Number(import.meta.env.VITE_LF_PROBE_TRIM ?? 0) || 0);
  clock.setBpm(bpm);
  clock.setMetronome(true);
  clock.setClickVolume(1);
  looper.setFixedLengthEnabled(true);
  looper.setFixedLengthBars(bars);
  const beatFrames = Math.round((sr * 60) / bpm);

  // ── take A: clicks only, no feedback ───────────────────────────────────────────────────────
  const lossA0 = pluginBridge.recordLossSnapshot();
  // The web-side bridge queue over the take, against ctx time.
  const queue: { t: number; frames: number; wall: number; consumed: number }[] = [];
  let sampling = true;
  const sampler = (async () => {
    while (sampling) {
      const st = pluginBridge.stats(0);
      if (st) queue.push({ t: ctx.currentTime, frames: st.queue, wall: performance.now(), consumed: st.consumed });
      await sleep(5);
    }
  })();
  looper.recDub(0);
  const recWall = performance.now();
  try {
    await until('take A to commit', () => looper.stateOf(0) === 'PLAYING', bars * 2 + 30);
  } catch (e) {
    sampling = false;
    await sampler;
    // Where the bridge queue went while the take recorded (diag shows the held PV instead).
    for (let t = 0; t < 40; t += 2) {
      const part = queue.filter((q) => q.wall >= recWall + t * 1000 && q.wall < recWall + (t + 2) * 1000);
      if (part.length) log(`fill +${t}s: queue min ${ms(Math.min(...part.map((q) => q.frames)), sr)} median ${ms(median(part.map((q) => q.frames)), sr)} ms`);
    }
    throw e;
  }
  const commitWall = performance.now();
  await sleep(6000);
  sampling = false;
  await sampler;
  // Around the commit: every jump of the web queue, a main-thread gap, or the context clock
  // falling behind the wall clock (a render stall).
  const around = queue.filter((q) => q.wall > commitWall - 3000);
  for (let i = 1; i < around.length; i++) {
    const a = around[i - 1];
    const q = around[i];
    const wallMs = q.wall - a.wall;
    const ctxMs = (q.t - a.t) * 1000;
    const jump = ms(q.frames - a.frames, sr);
    if (Math.abs(jump) > 6 || wallMs > 40 || (wallMs > 30 && ctxMs < wallMs - 25)) {
      log(`event +${Math.round(q.wall - commitWall)} ms after commit: queue ${ms(a.frames, sr)}→${ms(q.frames, sr)} ms, wall gap ${wallMs.toFixed(1)} ms, ctx advanced ${ctxMs.toFixed(1)} ms, consumed +${q.consumed - a.consumed}f`);
    }
  }
  const before = around.filter((q) => q.wall < commitWall);
  const after = around.filter((q) => q.wall > commitWall + 4000);
  log(`queue median: 3 s before commit ${ms(median(before.map((q) => q.frames)), sr)} ms, 4–6 s after ${ms(median(after.map((q) => q.frames)), sr)} ms`);
  const lossA1 = pluginBridge.recordLossSnapshot();
  const comp = recordLatency.lastCompensation();
  const a = committed(0);
  looper.setMute(0, true);
  // Noise floor from every 64th sample: sorting the whole take would stall the main thread.
  const floor = median(Array.from({ length: Math.floor(a.length / 64) }, (_, i) => Math.abs(a[i * 64])));
  const beatsA = Math.floor(a.length / beatFrames);
  const clickPeaks: number[] = [];
  const clickOffsets: number[] = [];
  const beatIdx: number[] = [];
  for (let k = 0; k < beatsA; k++) {
    // Search from half a beat early: a take placed early puts the click before its beat.
    const from = Math.max(0, k * beatFrames - beatFrames / 2);
    const p = peakIn(a, from, from + beatFrames);
    if (p.value < floor * 8) continue;
    const onset = crossing(a, from, p.at + 1, p.value * 0.3);
    clickPeaks.push(p.value);
    clickOffsets.push(onset - k * beatFrames);
    beatIdx.push(k);
  }
  if (clickOffsets.length < beatsA / 2) {
    throw Error(`take A: only ${clickOffsets.length}/${beatsA} clicks found (floor ${floor.toExponential(2)}; is the cable in input ${channel + 1} and the gain up?)`);
  }
  const clickX = median(clickOffsets);
  const drift = slope(beatIdx.map((k) => (k * beatFrames) / sr / 60), clickOffsets.map((f) => (f / sr) * 1000));
  // Three of four beats are plain blips, so the median is one: peak 0.56 at click volume 1 (clock.ts).
  const gain = median(clickPeaks) / 0.56;
  log(`take A: ${clickOffsets.length}/${beatsA} clicks, onset offset median ${ms(clickX, sr)} ms, spread ${ms(Math.min(...clickOffsets), sr)}..${ms(Math.max(...clickOffsets), sr)} ms, slope ${drift.toFixed(3)} ms/min, loop gain ${gain.toFixed(3)}, bridge loss ${lossA1.droppedFrames - lossA0.droppedFrames}f/${lossA1.underruns - lossA0.underruns} underruns`);
  log(`C terms: ${JSON.stringify(comp)}`);
  // The web bridge queue over the take, in eighths (median per slice).
  const slices = 8;
  const perSlice = Math.floor(queue.length / slices);
  for (let j = 0; j < slices; j++) {
    const part = queue.slice(j * perSlice, (j + 1) * perSlice);
    const bar = Math.round((j * bars) / slices);
    const i = beatIdx.findIndex((k) => k >= bar * 4);
    log(`slice ${j}: bar ${bar} click ${i < 0 ? '?' : ms(clickOffsets[i], sr)} ms | queue ${ms(median(part.map((q) => q.frames)), sr)} ms`);
  }

  // ── take B: pulses on the half beats, one echo round ───────────────────────────────────────
  clock.setMetronome(false);
  const monitorGain = Math.min(1.5, Math.max(0.05, 0.3 / Math.max(gain, 1e-3)));
  await platform.pluginHost.setMonitorGain(0, monitorGain);
  const pulse = ctx.createBuffer(1, 8, sr);
  pulse.getChannelData(0).fill(0.4);
  const scheduled = new Set<number>();
  let pulsing = true;
  const pulser = (async () => {
    while (pulsing) {
      const start = engineState.masterStartTime;
      const beat = beatFrames / sr;
      const now = ctx.currentTime;
      for (let n = Math.ceil((now - start) / beat - 0.5); ; n++) {
        const at = start + (n + 0.5) * beat;
        if (at > now + 0.25) break;
        if (at < now + 0.03 || scheduled.has(n)) continue;
        scheduled.add(n);
        const src = ctx.createBufferSource();
        src.buffer = pulse;
        src.connect(engine.masterGain);
        src.start(at);
        src.onended = () => src.disconnect();
      }
      await sleep(40);
    }
  })();
  const lossB0 = pluginBridge.recordLossSnapshot();
  looper.recDub(1);
  await until('take B to commit', () => looper.stateOf(1) === 'PLAYING', bars * 2 * 2 + 30);
  pulsing = false;
  await pulser;
  const lossB1 = pluginBridge.recordLossSnapshot();
  await platform.pluginHost.setMonitorGain(0, 0);
  looper.setMute(1, true);
  const b = committed(1);
  const directs: number[] = [];
  const windowPre = Math.round(0.002 * sr);
  const windowPost = Math.round(0.45 * sr);
  const avg = new Float64Array(windowPre + windowPost);
  let averaged = 0;
  for (let k = 0; k < Math.floor(b.length / beatFrames); k++) {
    const expect = Math.round((k + 0.5) * beatFrames);
    const p = peakIn(b, expect + clickX - beatFrames / 4, expect + clickX + beatFrames / 4);
    if (p.value < floor * 8) continue;
    directs.push(p.at - expect);
    if (p.at - windowPre < 0 || p.at + windowPost > b.length) continue;
    for (let i = 0; i < avg.length; i++) avg[i] += b[p.at - windowPre + i];
    averaged++;
  }
  if (directs.length < 8) {
    const where: number[] = [];
    for (let k = 0; k < Math.min(8, Math.floor(b.length / beatFrames)); k++) where.push(ms(peakIn(b, k * beatFrames, (k + 1) * beatFrames).at - k * beatFrames, sr));
    log(`take B peaks per beat (ms after the beat): ${where.join(' ')}; clickX ${ms(clickX, sr)} ms`);
  }
  if (directs.length < 8) throw Error(`take B: only ${directs.length} pulses found (peak ${peakIn(b, 0, b.length).value.toExponential(2)}, floor ${floor.toExponential(2)})`);
  for (let i = 0; i < avg.length; i++) avg[i] /= averaged;
  // The echo: the largest peak after the direct pulse has rung out (3 ms), in the averaged window.
  const ring = windowPre + Math.round(0.003 * sr);
  const direct = Math.abs(avg[windowPre]);
  let echoAt = -1;
  let echo = 0;
  for (let i = ring; i < avg.length; i++) if (Math.abs(avg[i]) > echo) { echo = Math.abs(avg[i]); echoAt = i; }
  const tail = Array.from(avg.subarray(avg.length - Math.round(0.01 * sr)), Math.abs);
  const noise = Math.max(...tail);
  const rt = echoAt - windowPre;
  const pulseX = median(directs);
  const residual = pulseX - rt;
  log(`take B: ${directs.length} pulses, direct offset median ${ms(pulseX, sr)} ms, spread ${ms(Math.min(...directs), sr)}..${ms(Math.max(...directs), sr)} ms; echo at +${ms(rt, sr)} ms, echo/direct ${(echo / direct).toFixed(3)}, echo/noise ${(echo / Math.max(noise, 1e-9)).toFixed(1)}, monitor gain ${monitorGain.toFixed(2)}, bridge loss ${lossB1.droppedFrames - lossB0.droppedFrames}f/${lossB1.underruns - lossB0.underruns} underruns`);
  log(`result: X=${ms(pulseX, sr)}ms RT=${ms(rt, sr)}ms residual=${ms(residual, sr)}ms clickX=${ms(clickX, sr)}ms drift=${drift.toFixed(3)}ms/min trim=${recordLatency.offsetMs()}ms C=${comp ? ms(comp.frames, sr) + 'ms' : 'none'}`);
}
