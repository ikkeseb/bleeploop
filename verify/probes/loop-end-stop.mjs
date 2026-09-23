/**
 * Playback proof for opt-in loop-end stop, measuring lane deadlines and the rendered UI. Uses real
 * AudioWorklet PCM captured at the lane gain/master output to prove: audio stops exactly on the loop
 * edge while the main thread is blocked, resume keeps master phase, a second STOP is immediate and
 * CLEAR prevents stale completion, a retiring reversed source stays audible to the edge, two playing
 * lanes stop on the same frame, Stop All commits a pending overdub immediately while playback waits
 * for loop end, a queued boundary click cancels (and restores on new playback), and the pending core
 * button disables transforms and exposes force stop.
 * Run: pnpm probe loop-end-stop [--url=<server>]
 * Does not establish native ASIO feel; the audio path is Chromium's Web Audio renderer.
 */
import { probe } from '../harness/probe.ts';
import { mkdir } from 'node:fs/promises';

await probe(async ({ open }) => {
  const { page } = await open({ viewport: { width: 1600, height: 900 } });
  await page.evaluate(async () => {
    const { engineState } = await import('/src/audio/looper/state.ts');
    const { defaultFxStates } = await import('/src/audio/fx/fx.ts');
    const lf = window.__lf;
    await lf.looper.init();
    const ctx = lf.engine.ctx;
    await ctx.resume();
    // The capture's base frame: Chromium can report a fresh node's FIRST quantum 128+ frames stale, and
    // can repeat a quantum's currentFrame, either of which shifts every measured window. So the node
    // says `ready` on its first quantum, and only the quantum after the main thread answers `arm` takes
    // the base, floored at the previous quantum + 128 (the handshake playback-restart.mjs uses).
    const code = `class StopCapture extends AudioWorkletProcessor {
      constructor() {
        super(); this.samples = new Float32Array(sampleRate * 10); this.base = -1; this.length = 0;
        this.next = -1; this.ready = false; this.armRequested = false;
        this.port.onmessage = ({ data }) => {
          if (data === 'arm') this.armRequested = true;
          else this.port.postMessage({ kind: 'data', base: this.base, samples: this.samples.slice(0, this.length) });
        };
      }
      process(inputs) {
        const frame = Math.max(currentFrame, this.next); this.next = frame + 128;
        if (!this.ready) { this.ready = true; this.port.postMessage({ kind: 'ready' }); }
        if (this.base < 0) {
          if (!this.armRequested) return true;
          this.base = frame; this.port.postMessage({ kind: 'armed' });
        }
        const input = inputs[0]?.[0];
        const count = Math.min(128, this.samples.length - this.length);
        for(let i=0;i<count;i++) this.samples[this.length+i] = input?.[i] ?? 0;
        this.length += count; return true;
      }
    } registerProcessor('stop-capture', StopCapture);`;
    const url = URL.createObjectURL(new Blob([code], { type: 'text/javascript' }));
    await ctx.audioWorklet.addModule(url);
    URL.revokeObjectURL(url);
    const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
    const until = async (time) => { while (ctx.currentTime < time) await wait(2); };
    const load = async (count = 1, amplitude = 0.2, bars = 1) => {
      lf.looper.clearAll();
      lf.clock.setMetronome(false);
      const frames = Math.round(ctx.sampleRate * 0.8) * bars;
      await lf.looper.loadSession({ bpm: 300, bars, masterLengthFrames: frames,
        tracks: Array.from({ length: count }, (_, index) => ({ index,
          pcm: Float32Array.from({ length: frames }, (_, i) => amplitude * (1 + i / frames)),
          volume: 1, muted: false, reversed: false, fx: defaultFxStates() })) });
      await until(engineState.masterStartTime + 0.12);
    };
    // Resolves with the reader once the capture is armed. Each reply's handler is in place before what
    // triggers it: `ready` before the node is wired, `armed` before `arm`, `data` before `read`.
    const capture = async (input) => {
      const node = new AudioWorkletNode(ctx, 'stop-capture');
      const handlers = {};
      node.port.onmessage = ({ data }) => handlers[data.kind]?.(data);
      const reply = (kind, ms) => new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error(`capture ${kind} timed out after ${ms} ms`)), ms);
        handlers[kind] = (data) => { clearTimeout(timer); resolve(data); };
      });
      const ready = reply('ready', 1500);
      input.connect(node); node.connect(ctx.destination);
      await ready;
      const armed = reply('armed', 1500);
      node.port.postMessage('arm');
      await armed;
      return async () => {
        const data = reply('data', 5000);
        node.port.postMessage('read');
        const result = await data;
        input.disconnect(node); node.disconnect();
        return result;
      };
    };
    const measure = (data, start, end) => {
      let sum = 0, peak = 0, count = 0, last = -1;
      const first = Math.max(0, Math.ceil(start * ctx.sampleRate) - data.base);
      const limit = Math.min(data.samples.length, Math.floor(end * ctx.sampleRate) - data.base);
      for (let i = first; i < limit; i++) {
        const value = data.samples[i]; sum += value * value; peak = Math.max(peak, Math.abs(value)); count++;
        if (Math.abs(value) > 0.001) last = data.base + i;
      }
      return { rms: Math.sqrt(sum / Math.max(1, count)), peak, last, count };
    };
    window.__stopProbe = { lf, ctx, state: engineState, wait, until, load, capture, measure };
  });

  const results = [];
  const check = (name, result) => {
    results.push({ name, ...result });
    if (!result.pass) throw new Error(`${name}: ${JSON.stringify(result)}`);
  };
  check('Immediate remains the default', await page.evaluate(async () => {
    const p = window.__stopProbe; await p.load();
    const enabled = p.lf.looper.loopEndStopEnabled(); p.lf.looper.playStop(0);
    return { pass: !enabled && p.lf.looper.trackInfo(0).state === 'STOPPED' };
  }));
  await page.getByRole('button', { name: 'Stop playing loops at loop end', exact: true }).click();
  check('Audio stops on loop edge while main thread is blocked', await page.evaluate(async () => {
    const p = window.__stopProbe; await p.load();
    const read = await p.capture(p.state.tracks[0].gain);
    p.lf.looper.playStop(0); const end = p.lf.looper.trackInfo(0).stopAt;
    const recBefore = p.lf.looper.trackInfo(0).state;
    await p.lf.looper.recDub(0); p.lf.looper.reverse(0);
    const blocked = p.lf.looper.trackInfo(0).state === 'PLAYING' && !p.lf.looper.trackInfo(0).reversed;
    const wallEnd = performance.now() + (end - p.ctx.currentTime + 0.12) * 1000;
    while (performance.now() < wallEnd) { /* block the dispatcher, leave audio rendering live */ }
    await p.until(end + 0.15); const data = await read();
    const before = p.measure(data, end - 0.08, end);
    const after = p.measure(data, end, end + 0.1);
    const frameError = before.last + 1 - Math.round(end * p.ctx.sampleRate);
    return { pass: recBefore === 'PLAYING' && blocked && before.rms > 0.1 && after.peak < 0.00001 && Math.abs(frameError) <= 1 && p.lf.looper.trackInfo(0).state === 'STOPPED', frameError, before, after };
  }));
  check('Resume keeps the master phase', await page.evaluate(async () => {
    const p = window.__stopProbe; const read = await p.capture(p.state.tracks[0].gain);
    p.lf.looper.playStop(0); await p.wait(120); const data = await read();
    let maxError = 0, count = 0;
    for (let i = 0; i < data.samples.length; i++) {
      if (data.samples[i] < 0.001) continue;
      const frame = data.base + i;
      const phase = ((frame / p.ctx.sampleRate - p.state.masterStartTime) / 0.8 % 1 + 1) % 1;
      // Ignore a one-frame wrap ambiguity; everywhere else the ramp exposes phase errors.
      if (phase < 0.002 || phase > 0.998) continue;
      maxError = Math.max(maxError, Math.abs(data.samples[i] - 0.2 * (1 + phase))); count++;
    }
    return { pass: count > 100 && maxError < 0.0001, maxError, count };
  }));
  check('Second STOP is immediate and CLEAR prevents stale completion', await page.evaluate(async () => {
    const p = window.__stopProbe; await p.load(); p.lf.looper.playStop(0);
    const oldEnd = p.lf.looper.trackInfo(0).stopAt; p.lf.looper.playStop(0);
    const stopped = p.lf.looper.trackInfo(0).state === 'STOPPED';
    p.lf.looper.playStop(0); p.lf.looper.playStop(0); await p.load();
    await p.until(oldEnd + 0.1);
    return { pass: stopped && p.lf.looper.trackInfo(0).state === 'PLAYING' && p.lf.looper.trackInfo(0).stopAt === null };
  }));
  check('Retiring reverse source stays audible until the stop edge', await page.evaluate(async () => {
    const p = window.__stopProbe; await p.load();
    const read = await p.capture(p.state.tracks[0].gain); p.lf.looper.reverse(0);
    const retiring = p.state.tracks[0].retiringSources.size; p.lf.looper.playStop(0);
    const end = p.lf.looper.trackInfo(0).stopAt; await p.until(end + 0.15);
    const data = await read(), before = p.measure(data, end - 0.08, end), after = p.measure(data, end, end + 0.1);
    return { pass: retiring > 0 && before.rms > 0.1 && before.peak < 0.41 && after.peak < 0.00001, retiring, before, after };
  }));
  check('Two playing lanes stop on the same audio frame', await page.evaluate(async () => {
    const p = window.__stopProbe; await p.load(2);
    const reads = await Promise.all([p.capture(p.state.tracks[0].gain), p.capture(p.state.tracks[1].gain)]);
    p.lf.looper.stopAll();
    const ends = [p.lf.looper.trackInfo(0).stopAt, p.lf.looper.trackInfo(1).stopAt];
    await p.until(ends[0] + 0.15);
    const data = await Promise.all(reads.map((read) => read()));
    const edges = data.map((samples) => ({
      before: p.measure(samples, ends[0] - 0.08, ends[0]),
      after: p.measure(samples, ends[0], ends[0] + 0.1),
    }));
    return { pass: ends[0] === ends[1] && edges.every(({ before, after }) =>
      before.rms > 0.1 && Math.abs(before.last + 1 - Math.round(ends[0] * p.ctx.sampleRate)) <= 1 && after.peak < 0.00001), ends, edges };
  }));
  check('Stop All commits an overdub immediately while playback waits for loop end', await page.evaluate(async () => {
    const p = window.__stopProbe; await p.load(2); await p.lf.looper.recDub(1); await p.wait(80);
    const wasDubbing = p.lf.looper.trackInfo(1).state === 'OVERDUBBING';
    p.lf.looper.stopAll();
    const playback = p.lf.looper.trackInfo(0), overdub = p.lf.looper.trackInfo(1);
    const kept = p.lf.looper.trackPeak(1) > 0.1;
    return { pass: wasDubbing && playback.state === 'PLAYING' && playback.stopAt !== null
      && overdub.state === 'STOPPED' && overdub.stopAt === null && overdub.canUndo && kept, playback, overdub };
  }));
  for (const resumeOther of [false, true]) {
    check(resumeOther ? 'New playback restores a cancelled future click' : 'Stop All cancels the queued boundary click', await page.evaluate(async (restore) => {
      const p = window.__stopProbe; await p.load(2, 0);
      p.lf.looper.stop(1); p.lf.clock.setMetronome(true); p.lf.clock.setClickVolume(0.7);
      const read = await p.capture(p.lf.engine.masterGain);
      const end = p.state.masterStartTime + 0.8;
      await p.until(end - 0.045); p.lf.looper.stopAll();
      if (restore) { await p.wait(5); p.lf.looper.playStop(1); }
      await p.until(end + 0.12); const data = await read();
      const boundary = p.measure(data, end, end + 0.07);
      return { pass: boundary.count > 100 && (restore ? boundary.peak > 0.02 : boundary.peak < 0.00001), boundary };
    }, resumeOther));
  }

  await page.evaluate(async () => { const p = window.__stopProbe; await p.load(1, 0.2, 8); p.lf.looper.playStop(0); });
  await mkdir('logs', { recursive: true });
  await page.screenshot({ path: 'logs/loop-end-stop.png' });
  await page.setViewportSize({ width: 1280, height: 720 });
  await page.screenshot({ path: 'logs/loop-end-stop-1280.png' });
  // The refused core names its reason (src/ui/looper/gates.ts) in both the aria-label and the title.
  const pendingCore = page.getByRole('button', { name: 'Track 1 stopping at loop end, wait or stop now', exact: true });
  const pendingCoreTitle = await pendingCore.getAttribute('title');
  check('Pending lane exposes force stop and disables transforms', {
    pass: await page.getByRole('button', { name: 'Track 1 stop now', exact: true }).isVisible()
      && await pendingCore.isDisabled()
      && pendingCoreTitle === 'stopping at loop end, wait or stop now'
      && await page.getByRole('button', { name: 'Track 1 reverse', exact: true }).isDisabled(),
    pendingCoreTitle,
  });
  await page.getByRole('button', { name: 'Track 1 stop now', exact: true }).click();
  check('Pointer force stop completes', { pass: await page.evaluate(() => window.__lf.looper.trackInfo(0).state === 'STOPPED') });
  console.log(JSON.stringify(results, null, 2));
});
