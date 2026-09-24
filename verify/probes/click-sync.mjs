/**
 * Rendered-PCM check of loop-vs-click alignment: three lanes carry one short marker at loop frame 0,
 * the metronome is on, and the probe captures the click blips (tapped where they connect to masterGain)
 * beside each lane's gain node. Measures, per downbeat, the loop marker against the click's
 * onset: during 20 s of steady playback (drift), across normal STOP -> PLAY ALL restarts, and across
 * fast restarts inside the click's 120 ms anti-flam window (a swallowed downbeat).
 * Run: pnpm probe click-sync [--url=<server>]
 *
 * Asserts: the loop marker and the click onset coincide (within 0.1 ms) on every downbeat of the steady
 * run and of the normal restarts, with no odd click interval and no spread between lanes. A fast
 * restart can land within the click's anti-flam window of the last blip and swallow the new downbeat
 * (clock.ts MIN_CLICK_SPACING); those are printed, not failed.
 *
 * Web Audio only: it sees the render timeline, not the output device, the native monitor or the input
 * path (`pnpm native:loopback` measures those through a cable).
 *
 * @no-ci ~100 s of rendered audio; run it after a change to the click, the master pulse or lane starts
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

await probe(async ({ open }) => {
  const { page, consoleErrors } = await open();

  const report = await page.evaluate(async () => {
    const lf = window.__lf;
    const { engineState } = await import('/src/audio/looper/state.ts');
    const { defaultFxStates } = await import('/src/audio/fx/fx.ts');
    await lf.looper.init();
    const ctx = lf.engine.ctx;
    await ctx.resume();
    const sr = ctx.sampleRate;
    const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

    const workletCode = `
      class ClickSyncCapture extends AudioWorkletProcessor {
        next = -1;
        constructor(options) {
          super();
          this.frames = options.processorOptions.frames;
          this.channels = options.processorOptions.channels;
          this.samples = Array.from({ length: this.channels }, () => new Float32Array(this.frames));
          this.base = -1;
          this.written = 0;
          this.armRequested = false;
          this.sent = false;
          this.port.onmessage = ({ data }) => { if (data === 'arm') this.armRequested = true; };
          this.port.postMessage({ kind: 'ready' });
        }
        process(inputs) {
          const frame = Math.max(currentFrame, this.next);
          this.next = frame + 128;
          if (this.armRequested && this.base < 0) {
            this.base = frame;
            this.port.postMessage({ kind: 'armed', frame });
          }
          if (this.base < 0 || this.sent) return true;
          const input = inputs[0] ?? [];
          const count = Math.min(128, this.frames - this.written);
          for (let c = 0; c < this.channels; c++) {
            const source = input[c];
            for (let k = 0; k < count; k++) this.samples[c][this.written + k] = source?.[k] ?? 0;
          }
          this.written += count;
          if (this.written >= this.frames) {
            this.sent = true;
            this.port.postMessage({ kind: 'done', base: this.base, samples: this.samples });
          }
          return true;
        }
      }
      registerProcessor('click-sync-capture', ClickSyncCapture);
    `;
    const moduleUrl = URL.createObjectURL(new Blob([workletCode], { type: 'text/javascript' }));
    await ctx.audioWorklet.addModule(moduleUrl);
    URL.revokeObjectURL(moduleUrl);

    const LANES = 3;
    const bpm = 120;
    const masterFrames = Math.round(sr * (60 / bpm) * 4);
    const beatFrames = masterFrames / 4;
    const MARKER = 64;
    lf.looper.clearAll();
    lf.looper.setLoopEndStopEnabled(false);
    lf.looper.setFixedLengthEnabled(false);
    await lf.looper.loadSession({
      bpm,
      bars: 1,
      masterLengthFrames: masterFrames,
      tracks: Array.from({ length: LANES }, (_, index) => ({
        index,
        pcm: Float32Array.from({ length: masterFrames }, (_, f) => (f < MARKER ? 0.3 : 0)),
        volume: 1,
        muted: false,
        reversed: false,
        state: 'STOPPED',
        fx: defaultFxStates(),
      })),
    });
    lf.clock.setClickVolume(1);

    // The click's own channel: every metronome blip is an OscillatorNode -> GainNode -> masterGain
    // (clock.ts triggerClick); mirror each such GainNode into clickBus as it connects to masterGain.
    const clickBus = ctx.createGain();
    const clickGains = new WeakSet();
    const nativeConnect = AudioNode.prototype.connect;
    AudioNode.prototype.connect = function (target, ...rest) {
      if (this instanceof OscillatorNode && target instanceof GainNode) clickGains.add(target);
      if (target === lf.engine.masterGain && clickGains.has(this)) nativeConnect.call(this, clickBus);
      return nativeConnect.call(this, target, ...rest);
    };

    /** Capture `seconds` of [master, lane 0..] starting now; `whileArmed` runs once the base frame is fixed. */
    const capture = async (seconds, whileArmed) => {
      const frames = Math.ceil(seconds * sr);
      const merger = ctx.createChannelMerger(LANES + 1);
      const node = new AudioWorkletNode(ctx, 'click-sync-capture', {
        numberOfInputs: 1,
        numberOfOutputs: 1,
        outputChannelCount: [1],
        channelCount: LANES + 1,
        channelCountMode: 'explicit',
        channelInterpretation: 'discrete',
        processorOptions: { frames, channels: LANES + 1 },
      });
      const messages = {};
      const got = (kind) => new Promise((resolve) => { messages[kind] = resolve; });
      const ready = got('ready');
      const armed = got('armed');
      const done = got('done');
      node.port.onmessage = ({ data }) => messages[data.kind]?.(data);
      clickBus.connect(merger, 0, 0);
      const lanesWired = [];
      const wireLanes = () => {
        for (let lane = 0; lane < LANES; lane++) {
          const gain = engineState.tracks[lane].gain;
          if (gain && !lanesWired.includes(lane)) { gain.connect(merger, 0, lane + 1); lanesWired.push(lane); }
        }
      };
      wireLanes();
      merger.connect(node);
      node.connect(ctx.destination);
      await ready;
      node.port.postMessage('arm');
      await armed;
      await whileArmed?.();
      wireLanes();
      const data = await done;
      clickBus.disconnect(merger);
      for (const lane of lanesWired) engineState.tracks[lane].gain.disconnect(merger);
      merger.disconnect();
      node.disconnect();
      node.port.close();
      return data;
    };

    const onsets = (x, threshold, gap) => {
      const out = [];
      let quiet = gap;
      for (let i = 0; i < x.length; i++) {
        if (Math.abs(x[i]) > threshold) {
          if (quiet >= gap) out.push(i);
          quiet = 0;
        } else quiet++;
      }
      return out;
    };
    const residualOf = (data) => data.samples[0];
    /** Per loop downbeat: marker frame (lane 0), the nearest click onset and their delta. */
    const analyse = (data) => {
      const residual = residualOf(data);
      const clicks = onsets(residual, 1e-3, Math.round(0.08 * sr)).map((i) => data.base + i);
      const markers = data.samples.slice(1).map((lane) => onsets(lane, 0.1, Math.round(0.5 * sr)).map((i) => data.base + i));
      const laneSpread = markers[0].map((m, k) => Math.max(...markers.map((lane) => Math.abs((lane[k] ?? Infinity) - m))));
      const downbeats = markers[0].map((marker) => {
        let nearest = null;
        for (const c of clicks) if (nearest === null || Math.abs(c - marker) < Math.abs(nearest - marker)) nearest = c;
        const deltaFrames = nearest === null ? null : marker - nearest;
        return { marker, click: nearest, deltaMs: deltaFrames === null ? null : +(deltaFrames / sr * 1000).toFixed(3),
          clickMissing: deltaFrames === null || Math.abs(deltaFrames) > beatFrames / 2 };
      });
      const intervals = clicks.slice(1).map((c, k) => c - clicks[k]);
      const oddIntervals = intervals.filter((d) => Math.abs(d - beatFrames) > 2);
      const anchor = Math.round(engineState.masterStartTime * sr);
      const clickGridErr = clicks.map((c) => { const e = ((c - anchor) % beatFrames + beatFrames) % beatFrames; return e > beatFrames / 2 ? e - beatFrames : e; });
      return { clicks: clicks.length, clickGridErrMax: Math.max(0, ...clickGridErr.map(Math.abs)), downbeats, maxLaneSpreadFrames: Math.max(0, ...laneSpread.filter(Number.isFinite)), oddIntervals };
    };

    const out = {};
    // Calibration: metronome off, the residual (master minus lanes) must hold no onsets.
    lf.clock.setMetronome(false);
    lf.looper.playAll();
    const calib = await capture(3);
    out.calibrationResidualOnsets = onsets(residualOf(calib), 1e-3, Math.round(0.08 * sr)).length;
    lf.looper.stopAll();
    await wait(300);

    lf.clock.setMetronome(true);
    // A: steady playback, one PLAY ALL, 20 s.
    const steady = await capture(20.5, async () => { lf.looper.playAll(); });
    out.steady = analyse(steady);
    out.steady.anchorFrame = Math.round(engineState.masterStartTime * sr);

    const restarts = async (label, count, minMs, maxMs) => {
      const rows = [];
      for (let n = 0; n < count; n++) {
        lf.looper.stopAll();
        const pause = minMs + Math.random() * (maxMs - minMs);
        let anchor = null;
        const data = await capture(4.6 + pause / 1000, async () => {
          await wait(pause);
          lf.looper.playAll();
          anchor = Math.round(engineState.masterStartTime * sr);
        });
        const a = analyse(data);
        rows.push({ pauseMs: Math.round(pause), anchorToMarkerFrames: a.downbeats[0] ? a.downbeats[0].marker - anchor : null, ...a });
      }
      out[label] = rows;
    };
    await restarts('restartNormal', 12, 200, 900);
    await restarts('restartFast', 12, 5, 110);
    lf.looper.stopAll();
    return out;
  });

  const fmt = (a) => a.downbeats.map((d) => (d.clickMissing ? 'MISSING' : `${d.deltaMs}`)).join(' ');
  console.log(`calibration residual onsets (metronome off, want 0): ${report.calibrationResidualOnsets}`);
  console.log(`steady 20 s: clicks=${report.steady.clicks} clickGridErrMax=${report.steady.clickGridErrMax}f laneSpread=${report.steady.maxLaneSpreadFrames}f oddIntervals=${JSON.stringify(report.steady.oddIntervals)}`);
  console.log(`  marker-click ms per downbeat: ${fmt(report.steady)}`);
  for (const label of ['restartNormal', 'restartFast']) {
    console.log(`${label}:`);
    for (const r of report[label]) {
      console.log(`  pause=${r.pauseMs}ms clicks=${r.clicks} gridErr=${r.clickGridErrMax}f anchor->marker=${r.anchorToMarkerFrames}f laneSpread=${r.maxLaneSpreadFrames}f odd=${JSON.stringify(r.oddIntervals)} | ${fmt(r)}`);
    }
  }
  assert.equal(report.calibrationResidualOnsets, 0, 'the click tap carried sound with the metronome off');
  for (const [label, rows] of [['steady', [report.steady]], ['restartNormal', report.restartNormal]]) {
    for (const r of rows) {
      assert.ok(r.downbeats.length > 0, `${label}: no loop marker found`);
      for (const d of r.downbeats) {
        assert.ok(!d.clickMissing && Math.abs(d.deltaMs) <= 0.1, `${label}: marker vs click ${d.clickMissing ? 'MISSING' : `${d.deltaMs} ms`}`);
      }
      assert.deepEqual(r.oddIntervals, [], `${label}: a click interval off the beat`);
      assert.equal(r.maxLaneSpreadFrames, 0, `${label}: lanes apart`);
    }
  }
  assert.deepEqual(consoleErrors, [], 'console errors during the probe');
});
